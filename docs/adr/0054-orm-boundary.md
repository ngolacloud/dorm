# ADR 0054 — The ORM boundary

Status: **Accepted** · 2026-11-03

## Context

DORM is an independent ORM (ADR 0073, the independent-ORM decision — the ORM as
a product). Over the L1–L16 campaign it grew capabilities that, on reflection, belong to a
*framework* or *service* layer built on top of it, not to the ORM itself:
message-broker clients, a generated REST API, user accounts, a long-running RAG
worker process, an HTTP transport for MCP, and a proposed gRPC data plane.

Read through the lens of Fowler's *Patterns of Enterprise Application
Architecture*, DORM is the **data-source layer**: one database, one transaction,
one process. Everything whose guarantee comes from communication between
services, from time (loops, scheduling, waits), from the identity of the caller,
from the domain, or from a policy decision is **not** the ORM's — it is the
layer around it.

Without a written rule, every new capability drifts inward and DORM stops being
usable as a plain library. This ADR fixes the rule and the boundary so a
reviewer can decide, per capability, where it belongs.

## Decision

### The rule (two tests, checked in code review)

A capability enters DORM only if **both** hold:

1. **Would someone using DORM in a command-line tool — no web server, no talking
   to other services — need this?** If not, it does not enter DORM.
2. **Does the guarantee come from a database transaction?** If it instead comes
   from communication between services, from time, from caller identity, or from
   policy, it belongs to the layer above and lives behind a port that layer owns.

When both tests say DORM but the capability is *support* (outbox, jobs, event
store, idempotency keys), it enters an **optional** crate; `dorm-core` never
depends on it.

The dependency is one-way: a framework built on DORM depends on DORM; **DORM
never depends on a framework.**

### What stays in DORM

- The core: `dorm-core`, `dorm-derive`, `dorm-schema`, `dorm-migrations`,
  `dorm-sqlite`, and the PostgreSQL and MySQL executors.
- Optional transactional support, each as its own crate implementing a port the
  layer above defines, never a dependency of `dorm-core`: `dorm-outbox`,
  `dorm-jobs`, `dorm-eventstore`, and idempotency keys (moving out of
  `dorm-core` into an optional crate — they are support tables, not ORM).
- Data tooling: `dorm-shell`, `dorm-studio` (localhost only), `dorm-rag` as a
  library, `dorm-mcp` over `stdio`.

Each support function works over an explicit session (`with_session`) and also
offers a variant taking `&Database`; none assumes a global default. One-shot
functions (`relay` a batch, `run_once`) stay one-shot — the loop that repeats
them, with concurrency, shutdown, scheduling and metrics, belongs to the caller.

### What is out of DORM (moves to the layer above, history preserved)

A capability leaving DORM is not rewritten from scratch: its code moves to the
consuming framework's repository with its git history preserved
(`git filter-repo`), and its ADRs travel with it.

| Capability | Was in | Why it is not the ORM's |
| --- | --- | --- |
| Messaging: CloudEvents, `Publisher`, NATS/AMQP/Kafka backends | `dorm-messaging` | Broker clients are communication between services (test 1 fails). The in-database `OutboxBus::drain` (consume with an inbox) returns to `dorm-outbox`. |
| Generated REST API (`dorm api generate`/`serve`) | `dorm-cli`, `dorm-schema` | An HTTP surface is a service layer; it is generated from facades, not tables, by the layer above. |
| User accounts (`createsuperuser`, `changepassword`, `user`, `orm_accounts`) | `dorm-cli` | Identity and authentication are policy and caller identity, not a transaction. |
| RAG worker as a looping process | `dorm-cli`, `dorm-rag` | The loop is time/scheduling; the library keeps the one-shot indexing function. |
| MCP HTTP transport | `dorm-mcp/src/http.rs` | HTTP needs the authentication and zero-trust of the layer above. `stdio` stays. |
| Command/Query dispatch (`dispatch`/`ask`) | `dorm-core/src/cqrs.rs` ([ADR 0012](0012-native-cqrs.md)) | Use-case dispatch is Service Layer. DORM keeps the read/write routing mechanism (`db.write`/`db.read`); the ADR is amended, not deleted. |
| Proposed gRPC data plane (`dorm-server`) | review-branch proposals | Rejected: see below. Cross-service access is contracts and events, never a shared data plane. |

### The rejected and renumbered review-branch proposals

The 2026-10-31 review proposed a serialized query IR and a gRPC data plane so
non-Rust clients could reach a module's data. That is polyglot/data-plane
access, which this boundary places outside the ORM. Those proposals are
renumbered (they collided with accepted ADR numbers) and marked:

- [ADR 0055](0055-query-ir.md) — Query IR: **Rejected**.
- [ADR 0056](0056-runtime-model-catalog.md) — runtime model catalogue:
  **reframed** as a library schema-metadata API (pending).
- [ADR 0057](0057-data-plane-grpc.md) — gRPC data plane: **Rejected**.
- [ADR 0058](0058-remote-db-objects-and-named-sql.md) — remote named SQL:
  **Rejected** (local raw SQL stays, [ADR 0005](0005-raw-sql.md)).
- [ADR 0059](0059-security-and-multitenancy.md) — data-plane security:
  **Rejected** (tenant-scope mechanism landed as [ADR 0021](0021-tenant-scope-enforcement.md)).
- [ADR 0060](0060-concurrency-control.md) — concurrency control: **partially
  superseded** by [ADR 0023](0023-optimistic-concurrency.md); pessimistic
  primitives pending.
- [ADR 0061](0061-zero-downtime-migrations.md) — zero-downtime migrations:
  **superseded** by [ADR 0022](0022-online-migrations.md)/[0038](0038-online-migration-applier.md).
- [ADR 0062](0062-extended-type-system.md) — extended types: **superseded** by
  [ADR 0025](0025-extended-type-system.md).

If narrow, read-only, one-module tooling access from another language is ever
needed, it is a consuming project's own binary with a deliberately narrow role —
never the way services talk to each other.

## Consequences

- **Deprecation before removal.** A capability leaving DORM is deprecated in
  0.3 (the command/API warns and says where it went) and removed in 0.4.
- **Optional support crates implement ports, not architecture.** `dorm-outbox`,
  `dorm-jobs`, `dorm-eventstore` and the idempotency crate are the DORM
  implementations of ports the layer above owns; the loops, runtimes and brokers
  that use them are not DORM's.
- **A plain CLI user loses nothing.** The core and the data tooling remain usable
  with no framework and no new dependencies — the independence property ADR 0073
  requires, proven by `dorm-example`.
- **The README gains a "DORM with frameworks" section** documenting the library
  API a framework drives, and listing the projects that use it.

## Alternatives considered

- **Let DORM become the framework.** Rejected: it would break the independent-ORM
  property (ADR 0073) and force a plain user to carry broker, HTTP and identity
  dependencies they do not need.
- **Keep the data plane in DORM as an optional crate.** Rejected: its guarantee
  is cross-service communication, which fails both tests; an optional crate would
  still pull the ORM's centre of gravity toward being a server.
