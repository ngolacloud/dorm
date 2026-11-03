# `dorm.schema` reference (v0.1)

`dorm.schema` is DORM's declarative schema — Prisma-faithful syntax, **compiled**
to Rust `#[derive(Model)]` structs by `dorm generate` (see
[ADR 0001](adr/0001-declarative-schema.md)). The schema is the source of truth;
`src/models.rs` is generated and not edited by hand.

## A complete example

```dorm
datasource db {
  provider = "postgresql"
  url      = env("DELONIX_ORM_DSN")
}

model Tenant {
  id        Int      @id @default(autoincrement())
  slug      String   @unique @index @db.VarChar(63)
  name      String   @db.VarChar(200)
  createdAt DateTime @default(now())
  posts     Post[]
}

model Post {
  id        Int      @id @default(autoincrement())
  tenant    Tenant   @relation(onDelete: Cascade)
  title     String   @index @db.VarChar(200)
  createdAt DateTime @default(now())
}
```

`dorm generate` turns this into two `#[derive(Model)]` structs: `Post.tenant`
becomes a `ForeignKey<Tenant>` whose `related_name` is `posts` (read off
`Tenant.posts Post[]`), and `Tenant.posts` produces no column.

## Blocks

| Block | Meaning |
|---|---|
| `datasource <name> { provider = "…"  url = env("VAR") \| "…" }` | The backend and connection. Today `provider` must be `postgresql`; `url` is read by the runtime, not baked in. |
| `model <Name> { … }` | One table / one Rust struct. |
| `enum <Name> { v1 v2 … }` | A named set of string variants, compiled to a native database enum ([see below](#enums)). |
| `generator … { }` | Accepted and ignored (eases paste-in from Prisma). |

## Field types

| Schema | Rust | Notes |
|---|---|---|
| `Int` | `i32` | `@id @default(autoincrement())` → `i64` (BIGSERIAL) |
| `BigInt` | `i64` | |
| `String` | `String` | `@db.VarChar(n)` sets `max_length` |
| `Boolean` | `bool` | |
| `DateTime` | `DateTime<Utc>` | `@default(now())`→`auto_now_add`, `@updatedAt`→`auto_now` |
| `Decimal` | `Decimal` | |
| `Json` | `serde_json::Value` | |
| `Uuid` | `uuid::Uuid` | |
| `Inet` | `std::net::IpAddr` | `INET`, PostgreSQL only (ADR 0040) |
| `Cidr` | `cidr::IpCidr` | `CIDR`, PostgreSQL only; needs the `cidr` crate (ADR 0048) |
| `MacAddr` | `eui48::MacAddress` | `MACADDR`, PostgreSQL only; needs the `eui48` crate (ADR 0048) |
| `<Model>` | `ForeignKey<Model>` | a to-one relation; needs `@relation` |
| `<Model>[]` | *(no column)* | a to-many back-relation; names the `related_name` |
| `<Enum>` | `String` | a field typed as a top-level `enum`; a native database enum ([see below](#enums)) |
| `T?` | `Option<T>` | nullable column |

## Attributes

| Attribute | Effect |
|---|---|
| `@id` | primary key |
| `@default(autoincrement())` | serial key (`i64`/BIGSERIAL) |
| `@default(now())` | `auto_now_add` (set on insert) |
| `@updatedAt` | `auto_now` (set on every write) |
| `@default(<literal>)` | a raw SQL default (`'active'`, `0`, `true`) |
| `@unique` | unique column |
| `@index` | a secondary index |
| `@db.VarChar(n)` | `max_length = n` |
| `@db.Text` | accepted, no length |
| `@map("col")` / `@@map("table")` | rename a column / table |
| `@relation(onDelete: Cascade\|Restrict\|SetNull\|NoAction)` | the FK's delete action |

## Block attributes

| Attribute | Effect |
|---|---|
| `@@map("table")` | rename the table |
| `@@id([a, b])` | a composite primary key — both columns become `primary_key` (ADR 0043) |
| `@@unique([a, b])` | a composite unique constraint — one `unique_together` group per attribute (ADR 0043) |
| `@@index([a, b])` | a composite index — built `CONCURRENTLY` by `migrate`/`sync` (ADR 0044) |
| `@@rag(…)` | opt the model into RAG (ADR 0019) |

A column named by `@@id`/`@@unique`/`@@index` must be a real column of the model,
or `dorm validate` names the typo.

## Enums

A top-level `enum` block declares a named set of string variants
([ADR 0047](adr/0047-schema-enum-block.md)):

```dorm
enum OrderStatus {
  pending
  paid
  shipped
}

model Order {
  id     Int         @id @default(autoincrement())
  status OrderStatus
  note   OrderStatus?
}
```

Grammar: `enum <Name> { v1 v2 … }`. The variants are bare identifiers separated
by whitespace or newlines (Prisma style), up to the closing `}`. An `enum` may be
declared before or after the model that uses it.

A field typed as an enum (`status OrderStatus`) compiles to a **native database
enum** — a `String` column backed by a `CREATE TYPE <name> AS ENUM (…)`
([ADR 0046](adr/0046-native-enum-types.md)):

```rust
#[field(enum_type = "order_status", enum_variants = "pending, paid, shipped")]
pub status: String,
```

The SQL type name is the snake_case of the enum name (`OrderStatus` →
`order_status`); the variants are emitted exactly as declared. A nullable field
(`OrderStatus?`) becomes `Option<String>`. The value is a plain `String` in Rust,
but the database rejects any value outside the declared set.

`dorm validate` rejects an enum with no variants, with a duplicate variant, or
whose name collides with a model. Adding a new variant to a live enum (Postgres
`ALTER TYPE … ADD VALUE`) is not yet generated — see
[ADR 0047](adr/0047-schema-enum-block.md).

## Not yet (roadmap)

Table-level `@@check`, default-ordering, partial/expression indexes, and
multi-file schemas. A composite `@@index` added to an *existing* table is built
by `sync` but not yet emitted as a versioned incremental migration step (ADR
0044). `CIDR`/`MACADDR` columns work on a hand-written model (ADR 0048), not yet
from a schema type. All additive — see
[roadmap.md](roadmap.md). Writing Rust models by hand still works; the schema is
the tooled path.
