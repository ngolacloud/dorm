# ADR 0015 — Connection resilience: health-checked checkout and acquire retry

## Status

Accepted and implemented. The third production blocker from ADR 0011's Phase 1,
and the "resilience (retry/reconnect, a pool health-check round trip)" follow-up
ADR 0004 deferred.

## Context

The pool had an *acquire* timeout but no resilience past it. Two gaps surfaced a
database blip as an error the application had to handle:

1. **Checkout handed out dead connections.** Recycling was `Fast` — a checkout
   trusted a pooled connection without a round trip. After a database restart or
   a `pg_terminate_backend`, the pooled sockets are dead, and the next query got
   one and failed with `connection closed`.
2. **A transient acquire failure was fatal.** While the database was briefly
   unreachable — restarting, or the pool momentarily exhausted — `pool.get()`
   failed and the error went straight to the caller, with no retry to ride out a
   one-second hiccup.

## Decision

### Health-checked checkout, resilient by default

Recycling defaults to `Verified`: a checkout does a liveness round trip, so a
dead connection is **detected and replaced** before it reaches a query rather
than handed to it. `DELONIX_ORM_POOL_RECYCLING` overrides it — `fast` (the former
behaviour, no round trip, for a latency-critical stable deployment) or `clean`
(`DISCARD ALL` as well). The cost is one empty round trip per checkout; the
benefit is that a restart is invisible to callers.

### Acquire retry with capped exponential backoff

`acquire` retries a transient failure with backoff — base 50 ms, doubling, capped
at 2 s, three attempts — before giving up (`DELONIX_ORM_RETRY_MAX` /
`DELONIX_ORM_RETRY_BASE_MS`). This rides out the window where a fresh connection
cannot be made yet because the server is still coming back.

### Only the *acquire* is retried — never a statement in flight

This is the correctness boundary. A `pool.get()` has sent nothing on any
connection, so retrying it cannot double-apply anything. A statement already in
flight is different: a connection that breaks *after* the server committed a
single autocommit write but *before* the client sees the acknowledgement looks
identical, from the client, to one that never ran — retrying it might apply the
write twice. DORM will not guess, so it does not retry statements. Reads are
side-effect-free and could be retried, but the far more common failure — the
database bouncing — is caught by `Verified` recycling replacing the dead
connection at the *next* acquire, which the retry already covers. (A transaction
is never retried either: a broken transaction is aborted by the server, and its
`CURRENT_TX` connection is gone.)

## Consequences

- A database restart, a failover, or an idle connection the server reaped is
  invisible to callers: the next query transparently gets a live connection.
- Resilience is the default posture; a deployment that measured the round trip
  and wants it gone opts into `fast`.
- The guarantee is deliberately bounded to what is safe: the pool heals itself at
  the connection level; it does not silently replay an in-flight write, because
  that cannot be done correctly.

## Proof

- `crates/dorm-core/tests/resilience_live.rs` —
  `the_pool_recovers_after_the_server_kills_its_connections`, live against
  PostgreSQL: it warms several pooled connections, has the server
  `pg_terminate_backend` every one of them, and then runs ten queries that all
  succeed — the dead connections are detected and replaced on checkout. Without
  `Verified` recycling those checkouts would hand out corpses and fail.
- Unit tests pin the pure halves: the backoff schedule (doubles, caps, never
  overflows into a tiny wait) and the recycling-method selection (defaults to
  `Verified`, honours `fast`/`clean`, falls back to the resilient default for
  anything unrecognised).
- CockroachDB's conformance suite (15/15) still passes with `Verified` the
  default, confirming the liveness round trip works over the PG wire there too.
