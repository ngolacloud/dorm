# Models

A model is a Rust struct with `#[derive(Model)]`. The derive reads the struct's
field types and the `#[model]` / `#[field]` attributes and emits, for a struct
`Author`:

- an `impl Model for Author` carrying its `&'static ModelMeta`;
- `Author::objects()` — the entry point for queries;
- `mod author` — one typed handle per field, so `author::name.eq("ada")`;
- `struct AuthorFields` — the accessor reached *through* a foreign key;
- `struct NewAuthor` — the insert shape, carrying only caller-supplied columns.

```rust
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
```

> A model cannot be generic, and must have named fields — its metadata is one
> `static`, and there is nothing to name a column after in a tuple struct.

## Field types

The column type is inferred from the Rust type. The closed set DORM knows:

| Rust type | `FieldType` | PostgreSQL |
|---|---|---|
| `i32` | `I32` | `INTEGER` |
| `i64` | `I64` | `BIGINT` (or `BIGSERIAL` as a primary key) |
| `String` | `Text` | `TEXT` |
| `bool` | `Bool` | `BOOLEAN` |
| `rust_decimal::Decimal` | `Decimal` | `NUMERIC` |
| `chrono::DateTime<Utc>` | `Timestamp` | `TIMESTAMPTZ` |
| `uuid::Uuid` | `Uuid` | `UUID` |
| `serde_json::Value` | `Json` | `JSONB` |
| `ForeignKey<T>` | the target's key | `BIGINT REFERENCES …` |

Wrap any of these in `Option<T>` to make the column nullable:

```rust
#[field(max_length = 200)]
bio: Option<String>,   // → bio TEXT  (nullable)
```

The set is deliberately closed: an open one would leave the migration generator
unable to name a column's type and let the dynamic API ask for a column the
compiler cannot describe.

## `#[model(...)]` attributes

| Attribute | Meaning |
|---|---|
| `table = "authors"` | The table name. Defaults to the snake-cased, pluralised struct name (`Author` → `authors`, `Policy` → `policies`). |
| `ordering = "name"` | Default `ORDER BY` when a query states none. Prefix a term with `-` for descending: `ordering = "-created_at"`. |
| `unique_together = "author, slug"` | A composite `UNIQUE (author, slug)` constraint. |
| `check = "length(title) > 0"` | A `CHECK` constraint. Repeatable — list the attribute once per check. |
| `tenant_scoped` | Marks the model as scoped to a tenant (used by multi-tenant callers; it is metadata the compiler and migrator carry, not an implicit filter). |

```rust
#[model(
    table = "posts",
    ordering = "-published_at",
    unique_together = "author, slug",
    check = "length(title) > 0",
    check = "published_at <= now()"
)]
```

## `#[field(...)]` attributes

| Attribute | Meaning |
|---|---|
| `primary_key` | This field is the primary key. An `i64` key becomes `BIGSERIAL` (database-filled, absent from `New…`); a `String`/`Uuid` key is a plain `PRIMARY KEY` you supply. |
| `unique` | A `UNIQUE` constraint on the column. |
| `index` | A non-unique index (`CREATE INDEX … ON t (col)`). |
| `max_length = 120` | For text columns: the length the schema and validation use. |
| `column = "full_name"` | Store under a different column name than the struct field. |
| `default = "now()"` | A raw SQL `DEFAULT`. |
| `auto_now_add` | Timestamp set once, on insert (`DEFAULT now()`). The column is database-filled and absent from `New…`. |
| `auto_now` | Timestamp refreshed on every write. |
| `column_type = "jsonb"` | Override the inferred column type, naming it in SQL or Rust spelling (`jsonb`, `bigint`, `timestamptz`, …). |
| `foreign_key = "Author"` | This field references model `Author`. Pair with `ForeignKey<Author>` as the Rust type. |
| `on_delete = "cascade"` | The referential action: `cascade`, `restrict`, `set_null`, or `do_nothing`. |
| `related_name = "posts"` | The name the reverse relation is counted/traversed under (e.g. `Count("posts")` on `Author`). |

## Relations

A foreign key is a field typed `ForeignKey<Target>` with `#[field(foreign_key = "Target")]`:

```rust
#[derive(Model, Debug, Clone)]
#[model(table = "posts")]
struct Post {
    #[field(primary_key)]
    id: i64,
    #[field(foreign_key = "Author", on_delete = "cascade", related_name = "posts")]
    author: ForeignKey<Author>,
    #[field(max_length = 80)]
    slug: String,
}
```

This gives you:

- **Filtering across the relation** — `post::author.author().name().eq("ada")`
  builds the path `author__name` and the compiler resolves the join.
- **Fetching the parent in the same query** — `select_related` (see
  [Making queries](queries.md#following-relations)).
- **Counting the reverse side** — `Author::objects().annotate(Aggregate::new("posts", AggKind::Count, "posts"))`.

When you insert a `Post`, reference the parent through its key:

```rust
let ada = Author::objects().create(NewAuthor { name: "Ada".into() }).await?;
Post::objects()
    .create(NewPost { author: ada.as_fk(), slug: "on-engines".into() })
    .await?;
```

`as_fk()` is generated on every model; it is shorthand for
`ForeignKey::from_id(ada.pk())`.

## The generated `New…` struct

`NewAuthor` carries only the columns a caller supplies. Columns the database
fills in — a serial primary key, anything with a `DEFAULT`, `auto_now_add` /
`auto_now` stamps — are **absent on purpose**: there is no value for you to
invent. See [Creating rows](queries.md#creating-rows).

## Next

- [Making queries](queries.md) — read and write through `QuerySet`.
- [Migrations](migrations.md) — turn these models into schema.
