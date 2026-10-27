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

```console
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

## Next

- [Migrations](migrations.md) — create the schema this connection runs against.
- [Making queries](queries.md) — the full query API.
