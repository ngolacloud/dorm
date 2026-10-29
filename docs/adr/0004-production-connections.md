# ADR 0004 — Production-grade connections: TLS and statement timeout

## Status

Accepted. Implemented incrementally; this ADR covers the first two bricks —
**TLS** and the **statement timeout**. Resilience (retry/reconnect, a pool
health-check round trip) and the widened `Value` type set are follow-ups under
the same ADR number.

## Context

DORM's query construction is safe by design (ADR's elsewhere): every value is a
bound `$n`, every identifier is a `&'static str` the derive macro emitted. But
the *connection* was not production-grade. Two gaps blocked a high-criticality
deployment:

1. **No TLS.** `dorm-core::db` built its pool with `tokio_postgres::NoTls`, hard
   coded. A database reachable only over an untrusted network could not be used
   safely — the one gap that, alone, keeps DORM to "trusted network only".
2. **No statement timeout.** The pool had an *acquire* timeout, but once a
   connection was held, a runaway query had no ceiling. A single pathological
   statement could pin a connection indefinitely.

## Decision

### TLS is feature-gated, rustls by default

`dorm-core` grows two optional TLS backends, selected by Cargo feature:

- **`tls-rustls`** (in the default set) — `tokio-postgres-rustls` over rustls
  with the **ring** crypto provider. Pure Rust, no system library, so it builds
  offline and links statically — the constraint the rest of the NgolaCloud
  tree already lives under.
- **`tls-openssl`** — `postgres-openssl`. Uses the system OpenSSL, which some
  DBAs prefer; it is opt-in precisely because it drags `libssl`.

When **both** features are on (e.g. `--all-features`, which docs.rs uses),
rustls wins deterministically. With **neither**, a DSN that asks for TLS gets an
explicit configuration error — never a silent downgrade to plaintext.

The backend and verification strictness come from the DSN's `sslmode`, parsed by
`db::ssl_requirement`:

| `sslmode` | Meaning | This release |
|---|---|---|
| `disable` | Plaintext. | `NoTls`, whatever the features. |
| `require` | Encrypt; do **not** verify the certificate. | Supported (rustls: a no-verification verifier). |
| `verify-full` | Verify the chain **and** the hostname. | Supported (rustls: the default web-PKI verifier; roots from `sslrootcert` if given, else the bundled Mozilla roots). |
| `verify-ca` | Verify the chain, **not** the hostname. | **Explicit error** for now — use `verify-full` or `require`. Documented, not faked (the roadmap's rule: unsupported returns an error, never a wrong result). |
| `prefer`/`allow`/absent | libpq's opportunistic modes. | Treated as `disable` here: DORM does not silently half-encrypt. Ask for `require`+ explicitly. |

`disable` is also the answer when the chosen feature set has no TLS backend and
the DSN names no mode, so the historical NoTls behaviour is unchanged for a
caller who has not opted into TLS.

### The statement timeout is server-side, not a client race

`DELONIX_ORM_STATEMENT_TIMEOUT_MS`, when set to a positive value, is injected
into the connection as `options=-c statement_timeout=<ms>`. The **server**
enforces it and cancels the offending statement, raising SQLSTATE `57014`
(`query_canceled`), which `error::from_pg` maps to a named `Error::Timeout`.

This is deliberately *not* `tokio::time::timeout` around the future: aborting the
future client-side leaves the query running on the server, holding its locks. A
server-side `statement_timeout` actually cancels the work.

The default is **off** (`0`/unset), so existing behaviour and long legitimate
operations — notably migrations, which share this pool — are unchanged until an
operator opts in. An operator who sets it accepts that it also bounds DDL.

## Consequences

- A DORM client can now talk to PostgreSQL over an untrusted network with
  `sslmode=verify-full`, and bound any statement's runtime, without leaving the
  typed API.
- Three build shapes are supported and tested: `--no-default-features`
  (NoTls-only), default (rustls), and `tls-openssl`.
- `verify-ca` and the opportunistic modes are explicit gaps, named here and in
  the error text rather than approximated.
- Follow-ups under ADR 0004: retry/reconnect with backoff, a recycle-time health
  check (today's pool recycles `Fast`, with no round trip), and the wider
  `Value` set (`f64`, `bytea`, `date`/`time`, arrays).
