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
| `<Model>` | `ForeignKey<Model>` | a to-one relation; needs `@relation` |
| `<Model>[]` | *(no column)* | a to-many back-relation; names the `related_name` |
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

## Not yet (roadmap)

Composite primary keys, `@@unique`/`@@index` emission, table-level `@@check`,
enums, default-ordering, and multi-file schemas. All additive — see
[roadmap.md](roadmap.md). Writing Rust models by hand still works; the schema is
the tooled path.
