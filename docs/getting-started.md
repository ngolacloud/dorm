# Getting started

This walks you from an empty crate to a working query, the way Django's tutorial
and Prisma's "Getting started" do: **define a model → generate the schema →
apply it → query**. The first three steps need no database at all.

## 1. Add DORM to your `Cargo.toml`

```toml
[dependencies]
dorm-core = { path = "../DORM/crates/dorm-core" }
dorm-derive = { path = "../DORM/crates/dorm-derive" }
dorm-migrations = { path = "../DORM/crates/dorm-migrations" }

# DORM runs on tokio; you supply the runtime.
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
chrono = { version = "0.4", features = ["clock"] }
```

## 2. Define a model

A model is a plain struct with `#[derive(Model)]`. The struct's field types are
read by the macro — you don't restate them.

```rust
use chrono::{DateTime, Utc};
use dorm_core::model::Model;
use dorm_core::prelude::*;
use dorm_derive::Model;

#[derive(Model, Debug, Clone)]
#[model(table = "authors", ordering = "name")]
struct Author {
    #[field(primary_key)]
    id: i64,
    #[field(unique, max_length = 120)]
    name: String,
}

#[derive(Model, Debug, Clone)]
#[model(table = "posts", unique_together = "author, slug", check = "length(title) > 0")]
struct Post {
    #[field(primary_key)]
    id: i64,
    #[field(foreign_key = "Author", on_delete = "cascade", related_name = "posts")]
    author: ForeignKey<Author>,
    #[field(max_length = 80)]
    slug: String,
    #[field(max_length = 200, index)]
    title: String,
    #[field(auto_now_add)]
    published_at: DateTime<Utc>,
}
```

The full list of field types and attributes is in [Models](models.md).

## 3. See the schema — before it touches a database

`dorm-migrations` generates the DDL from the same metadata the queries use.
Print it and read it:

```rust
use dorm_core::model::Model;
use dorm_migrations as mig;

let models = [<Author as Model>::meta(), <Post as Model>::meta()];
println!("{}", mig::ddl::schema_for(&models)?);
```

```sql
CREATE TABLE IF NOT EXISTS authors (
    id BIGSERIAL PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS posts (
    id BIGSERIAL PRIMARY KEY,
    author BIGINT NOT NULL REFERENCES authors (id) ON DELETE CASCADE,
    slug TEXT NOT NULL,
    title TEXT NOT NULL,
    published_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (author, slug),
    CHECK (length(title) > 0)
);
CREATE INDEX IF NOT EXISTS posts_title_idx ON posts (title);
```

A primary key typed `i64` becomes `BIGSERIAL`; a foreign key becomes a
`REFERENCES` with the `ON DELETE` you declared; `auto_now_add` becomes
`DEFAULT now()`. Nothing is implicit that you can't see here.

## 4. Inspect a query's SQL — still no database

Every `QuerySet` can show the SQL it would run and the parameters it would bind:

```rust
let query = Post::objects()
    .filter(post::title.icontains("rust"))
    .order_by([post::published_at.desc()])
    .limit(10);

let sql = query.sql()?;
println!("{}\n  ({} parameter(s))", sql.text, sql.params.len());
```

```text
SELECT "posts"."id", "posts"."author", "posts"."slug", "posts"."title", "posts"."published_at"
FROM "posts" WHERE "posts"."title" ILIKE $1 ORDER BY "posts"."published_at" DESC LIMIT 10
  (1 parameter(s))
```

The value `"rust"` is **not** in the SQL — it is parameter `$1`.

> Everything up to here is exactly what [`cargo run -p dorm-example`](../crates/dorm-example)
> prints. Run it to see steps 3 and 4 live.

## 5. Point DORM at a database and run it

DORM reads its connection string from an environment variable:

```console
$ export DELONIX_ORM_DSN='host=/var/run/postgresql user=me dbname=blog'
#   (or the libpq URL form: postgresql://me@localhost/blog)
```

Apply the schema, then run the query for real:

```rust
#[tokio::main]
async fn main() -> dorm_core::Result<()> {
    let models = [<Author as Model>::meta(), <Post as Model>::meta()];

    // Create the schema (idempotent — safe to run every start).
    mig::migrate(&[mig::initial(&models)?]).await?;

    // Insert a row.
    let ada = Author::objects()
        .create(NewAuthor { name: "Ada Lovelace".into() })
        .await?;

    // Query it back.
    let found = Author::objects()
        .filter(author::name.icontains("ada"))
        .all()
        .await?;
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, ada.id);
    Ok(())
}
```

`NewAuthor` is generated next to `Author`: it carries only the columns *you*
supply — the serial `id` is filled by the database and is absent on purpose. See
[Making queries](queries.md#creating-rows).

## Where to go next

- [Models](models.md) — every field type and attribute.
- [Making queries](queries.md) — the whole `QuerySet` API.
- [Migrations](migrations.md) — versioning and applying schema changes over time.
- [Connection & transactions](connection-and-transactions.md) — the pool, the
  DSN, read-only mode, and `atomic`.
