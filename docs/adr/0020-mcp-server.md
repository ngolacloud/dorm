# ADR 0020 — A native MCP server generated from the registry

Status: proposed · 2026-11-01 · builds on [ADR 0019](0019-rag.md) (RAG) and on the
read-only guard and observability of [ADR 0008](0008-observability.md); mirrors the
generated-binary distribution of DORM Studio.

> Numbering note. The brief asked for `0011-mcp-server.md`; the live ledger was
> already at `0018`, so this is **0020**, paired with RAG's **[0019](0019-rag.md)**.

## Context

The models are already a machine-readable description — `ModelMeta`/`FieldMeta` —
and the dynamic query path (`fetch_value_rows`, `insert/update/delete_value_rows`)
already runs string-keyed queries through the one safe compiler. An LLM agent that
could read that description and drive those queries would get a typed, tenant-safe
interface to the data for free, with no per-model glue. ADR 0019 adds the missing
piece: `semantic_search` lets the agent find the right rows from natural language
before it acts on them with structured tools.

The risk is the mirror image of the opportunity. An agent that can call `update`
and `delete` is an agent that can destroy data; an agent whose tenant comes from a
tool argument is an agent one prompt-injection away from crossing tenants; and the
row text the agent reads is attacker-controllable content that must not be mistaken
for instructions. The server has to expose power and withhold trust in the same
breath.

## Decision

Ship `dorm-mcp` as an **optional library** with `serve(config)`, plus `dorm mcp
generate` which writes `src/bin/mcp.rs` into the user's project — the same pattern
as `dorm studio generate`, because the binary has to **link the project's models**
for the registry to be populated. It runs as `cargo run --bin mcp -- --transport
stdio|http [--write] [--tenant <slug>] [--models a,b]`.

### Transport: a self-contained JSON-RPC implementation, not `rmcp` (as built)

This ADR first specified the official Rust SDK `rmcp`. On evaluation, `rmcp 3.1.4`
requires **edition 2024 and Rust 1.88**, and its dependency tree (≈80 crates:
`schemars`, `oauth2`, `reqwest`, …) does not build offline in this project's
pinned, vendored setup. Rather than raise the workspace MSRV far past 1.75, pull a
large tree, and give up offline builds — all for a protocol that is, over stdio,
newline-delimited **JSON-RPC 2.0** with a small, stable method set — `dorm-mcp`
implements the protocol directly on `serde_json` + `tokio`. The result is
self-contained, builds offline, keeps MSRV 1.75, and is unit-testable without a
model host or a socket. `rmcp` can be reintroduced behind a feature if its build
story eases; the tool catalogue and execution are already decoupled from the
transport. This is the "a command does something real and tested" rule winning
over a dependency the environment cannot honour.

### Tools are generated from the registry — nothing is hand-written per model

Read tools, always present: `list_models`; `describe_model` (fields, types,
forward and reverse relations by `related_name`, whether it has RAG, whether it is
`tenant_scoped`); `query` (JSON filters using the `filter_str` keys —
`name__icontains`, `apps__name` — plus `order_by`, `limit`/`offset`, `values`,
over `fetch_value_rows`); `get`; `count`; `aggregate`; and `semantic_search` across
one or more models returning `{model, pk, score, excerpt}` so the agent feeds those
pks into the next tool. **No tool accepts raw SQL** — the ADR 0005 `:raw` valve is
not exposed.

Write tools, registered **only with `--write`**: `insert`, `update`, `delete`, over
`insert/update/delete_value_rows`. Each supports `dry_run: true`, which returns the
compiled SQL and bound parameters (exactly what `:sql` shows) and an affected-row
count without executing; each enforces a per-operation row cap; and an `update` or
`delete` **with no filter is refused**. Every tool's `inputSchema` is JSON Schema
derived from `FieldMeta` (type, nullability, `max_length`), and carries the MCP
annotations `readOnlyHint`/`destructiveHint`/`idempotentHint`. The tool
descriptions spell out the intended flow — `semantic_search → get/query → dry_run →
action` — so the model is steered toward looking before leaping.

### Resources and prompts

Resources: `dorm://models`, `dorm://models/{name}` (the schema), and
`dorm://records/{model}/{pk}`. Prompts: "investigate a record" and "summarise by a
criterion", each a small template that opens with the read-only flow.

### Security is the design, not a wrapper

- **Read-only by default** via `db::set_read_only(true)`, so the core's
  `guard_write` refuses any write even if a tool is mis-registered — defence in
  depth, not a single check.
- **The tenant comes from the credential, never a tool argument.** Over stdio it is
  the `--tenant` the operator launched with; over HTTP it is derived from the
  validated token (OAuth 2.1 / bearer, per the MCP spec), and writing needs a
  distinct scope. The server binds **loopback only** by default, like Studio.
- **Allow/deny lists** for models and fields (`DELONIX_ORM_MCP_*`); a denied field
  is also excluded from the RAG template, so the agent can neither read it back nor
  retrieve on it.
- **Limits**: rows per response, bytes per response, a timeout reusing
  `DELONIX_ORM_STATEMENT_TIMEOUT_MS`, and an HTTP rate limit.
- **Audit**: one `tracing` event per call — subject, tenant, tool, arguments with
  secrets stripped, rows affected, duration — on a `dorm::mcp` target.
- **Untrusted data is marked as such.** Text returned from RAG and from records is
  delimited and labelled in the response, so content cannot pose as instructions —
  prompt-injection mitigation at the boundary where the data enters the model.

### HTTP transport (as built): Streamable HTTP now, OAuth 2.1 next

The HTTP transport is the MCP **Streamable HTTP** shape, hand-written on `tokio`
(no `rmcp`/axum, MSRV 1.75): one endpoint where `POST` carries a JSON-RPC message
— a request answered as `application/json` or, when the client's `Accept` prefers
it, a `text/event-stream` SSE `message` event; a notification answered `202` — and
`GET` opens the server→client SSE heartbeat. `initialize` mints an
`Mcp-Session-Id` (returned as a header); later requests must carry it or get `404`
to re-initialize. This is the **resource-server** half of the auth spec: a bearer
token maps to `{tenant, write-scope}`, loopback by default, rate-limited. The
**OAuth 2.1 authorization server** (discovery `/.well-known`, PKCE, token
issuance/refresh) is the next phase; until then a token set is configured out of
band via `DELONIX_ORM_MCP_TOKENS`.

## Bricks (the order of work)

1. **Read-only server** — `dorm-mcp` with `serve`, `dorm mcp generate`, the stdio
   transport, the registry-derived read tools (`list_models`, `describe_model`,
   `query`, `get`, `count`, `aggregate`), resources, prompts, and the JSON-Schema +
   annotation derivation. (Depends on ADR 0019 brick 5 for `semantic_search`.)
2. **Writes, HTTP, limits, audit** — the `--write` tools with `dry_run`, row caps
   and the no-unfiltered-mutation rule; HTTP transport with OAuth 2.1/bearer, the
   write scope, loopback default and rate limit; the allow/deny lists; the limits;
   and the `dorm::mcp` audit event.

## Consequences

- An agent configured with the generated binary (`"command": "cargo", "args":
  ["run","--bin","mcp","--","--transport","stdio"]`) can answer "list the open
  tickets about email problems" by calling `semantic_search` then `query`, and —
  only with `--write` — can "close those tickets" by passing through `dry_run`
  before `update`. Without `--write`, the write tools do not exist to be called.
- Because every tool runs over the dynamic compiler path, an agent cannot name a
  table or column the derive did not put in the binary, cannot splice SQL, and
  cannot cross tenants — the same guarantees the REPL has, extended to a new caller.
- The server passes the MCP Inspector with no schema errors, because the schemas
  are generated from `FieldMeta` rather than written by hand.

## What this is not

- **Not raw SQL over MCP.** `query` takes structured filters; `:raw` is not exposed.
- **Not a tenant argument.** The tenant is the credential's, always.
- **Not writable by default.** Read-only until `--write`, and even then guarded by
  the core's read-only check, row caps, and the unfiltered-mutation refusal.
- **Not a trust boundary the model owns.** Record and RAG text are delimited and
  labelled as untrusted data on the way out.
