# Connection & transactions

DORM owns a single process-global connection pool, built lazily the first time a
query runs. You configure it through the environment; you never pass a connection
to a query.

## The DSN

The connection string is read from an environment variable:

| Variable | Role |
|---|---|
| `DELONIX_ORM_DSN` | The DSN DORM uses. Checked first. |
| `PAAS_DATABASE_URL` | Accepted as a fallback when `DELONIX_ORM_DSN` is unset. |

Both the libpq keyword form and the URL form work:

```sh
$ export DELONIX_ORM_DSN='host=/var/run/postgresql user=me dbname=blog'
$ export DELONIX_ORM_DSN='postgresql://me:secret@localhost:5432/blog'
```

If neither is set, the first query returns a configuration error naming both
variables — not a panic.

### Tuning the pool

| Variable | Default | Meaning |
|---|---|---|
| `DELONIX_ORM_POOL_MAX` | 8 | Ceiling on pooled connections. |
| `DELONIX_ORM_POOL_ACQUIRE_TIMEOUT_MS` | 5000 | How long a caller waits for a connection before giving up. |
| `DELONIX_ORM_STATEMENT_TIMEOUT_MS` | 0 (off) | Server-side `statement_timeout`. When positive, PostgreSQL cancels an over-running statement and DORM returns `Error::Timeout`. Bounds DDL too, so leave it off while migrating unless you mean it. |

## TLS (ADR 0004)

TLS is chosen by the DSN's `sslmode`, or by `DELONIX_ORM_SSLMODE` /
`DELONIX_ORM_SSLROOTCERT` which override it. DORM never half-encrypts — the
opportunistic libpq modes (`prefer`, `allow`) and an absent `sslmode` mean
plaintext; ask for `require` or `verify-full` to get TLS.

| `sslmode` | Meaning | Status |
|---|---|---|
| `disable` *(default)* | Plaintext. | supported |
| `require` | Encrypt; do **not** verify the certificate. | supported |
| `verify-full` | Encrypt **and** verify the chain and hostname. Roots come from `sslrootcert` if set, else the bundled Mozilla roots. | supported |
| `verify-ca` | Verify the chain, not the hostname. | not yet — use `verify-full` or `require` |

```sh
$ export DELONIX_ORM_DSN='postgresql://me@db.internal:5432/blog?sslmode=verify-full&sslrootcert=/etc/ssl/ca.pem'
```

The backend is a Cargo feature of `dorm-core`: `tls-rustls` (default, pure Rust,
builds offline) or `tls-openssl` (system libssl). With neither compiled in, a
DSN that asks for TLS is a configuration error — never a silent downgrade.

## The one-runtime rule

`tokio-postgres` drives each connection from a background task, so a pooled
connection belongs to the runtime that created it. The pool is a process global,
which means:

> **The process must keep one tokio runtime alive for as long as it uses DORM.**

Drop that runtime and every connection in the pool dies with it; the next query
comes back `connection closed`. In an `#[tokio::main]` program this is automatic.
A synchronous program driving DORM through `block_on` must build its runtime
**once** and reuse it for the whole session — a runtime per statement works for
exactly one statement.

## Transactions: `atomic`

`db::atomic` runs a closure inside a transaction, committing on `Ok` and rolling
back on `Err`:

```rust
use dorm_core::prelude::*;

db::atomic(|_tx| async move {
    let ada = Author::objects().create(NewAuthor { name: "Ada".into() }).await?;
    Post::objects()
        .create(NewPost { author: ada.as_fk(), slug: "on-engines".into() })
        .await?;
    Ok(())
})
.await?;
```

Everything inside runs on the transaction's connection automatically — you do
**not** thread a handle through each call. DORM tracks the current transaction in
a task-local, which is also what lets a nested `atomic` find the outer one.

### Nesting is by savepoint

An `atomic` inside another becomes a `SAVEPOINT`: if the inner block fails it
undoes only its own work, and the outer transaction can still commit.

```rust
db::atomic(|_| async move {
    do_the_required_part().await?;

    // An optional part that may fail without losing the required part.
    let _ = db::atomic(|_| async move { do_the_optional_part().await }).await;

    Ok(())
})
.await?;
```

### What happens on a panic

An error return rolls back explicitly. A panic can't `.await` a `ROLLBACK`, so
DORM instead takes the connection out of the pool on the way down — it is closed
rather than recycled, and PostgreSQL rolls back a transaction whose connection
drops. That also covers the process being killed.

## Read-only mode

Mark the whole process read-only and every write refuses *before* it reaches SQL,
naming the operation it blocked:

```rust
dorm_core::db::set_read_only(true);

Post::objects().filter(post::id.eq(1)).delete().await?;
// Err: operation `delete` refused — the session is read-only
```

The session is *also* opened `READ ONLY` at the server (the real guard); the flag
exists so the refusal names the operation instead of surfacing a raw PostgreSQL
error.

## Interactive transactions (the shell)

`atomic` brackets a single future. The [shell](cli-contract.md#commands) (`dorm
shell`, `--write`) also exposes transactions **interactively**, held open across
commands:

```text
dorm*> :begin
dorm*> insert Author name=Ada
dorm*> insert Author name=Grace
dorm*> Author                 # the session sees its own uncommitted rows
dorm*> :commit                # …persisted atomically  (or :rollback to discard)
```

`:begin` opens a transaction on a dedicated connection; every following command
runs on it, so a read sees the uncommitted work and a failed statement aborts
the transaction. `:commit` persists everything at once; `:rollback` — or leaving
the shell with a transaction open — discards it. Nothing a session began but did
not commit is ever left written (ADR 0006).

## Next

- [Migrations](migrations.md) — create the schema this connection runs against.
- [Making queries](queries.md) — the full query API.
- [DORM Studio](studio.md) — browse and edit over the same connection, read-only
  unless `--write`.
