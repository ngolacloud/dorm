# Start a new project

This walks you from nothing to a running project with the CLI — the quickest way
to try DORM. If you want to add DORM to a Cargo project you already have, see
[Use DORM in an existing project](existing-project.md) instead.

You will scaffold a project, edit a **Prisma-style schema**, compile it to Rust
models, create the tables, and query them — then browse the data in the browser.

## 1. Scaffold

```sh
cargo install dorm-cli     # if you haven't already
dorm init blog
cd blog
```

`dorm init` writes a complete, runnable project:

```text
blog/
├── Cargo.toml
├── dorm.schema          # your models, Prisma-style — the source of truth
├── src/
│   ├── models.rs        # generated from dorm.schema (do not edit by hand)
│   ├── lib.rs
│   ├── main.rs          # a demo: prints the schema and a sample query
│   └── bin/
│       ├── migrate.rs   studio.rs  shell.rs  validate.rs  …
└── migrations/
```

The binaries under `src/bin/` are what `dorm migrate`, `dorm shell`,
`cargo run --bin studio` and friends run — they need your compiled models, so
they live in your project.

## 2. Edit the schema

Open `dorm.schema`. It starts with a datasource and two example models:

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

This is the one file you edit to shape your data — add fields, models and
relations here, not in the generated Rust. The full syntax is in
[The schema](schema.md). Add a model with `dorm new model <Name>`, or just type
it in.

## 3. Compile the schema to Rust

```sh
dorm generate            # dorm.schema → src/models.rs
```

`src/models.rs` now has a `#[derive(Model)]` struct per model, with typed field
handles for queries. Re-run `dorm generate` whenever you change the schema.

## 4. Point at a database and create the tables

Set the connection string, then create the tables to match your models:

```sh
createdb blog
export DELONIX_ORM_DSN='host=/var/run/postgresql user='"$USER"' dbname=blog'

dorm doctor              # check config and connectivity
dorm push                # create tables/columns to match your models
```

`dorm push` is additive and non-destructive — it creates what's missing and
never drops or retypes a column. For versioned, reviewable changes over time,
use [migrations](migrations.md) instead.

## 5. Query it

The scaffolded `main.rs` prints the schema and a sample query's SQL with no
database; with the DSN set it runs for real:

```sh
cargo run
```

Or open the REPL and query by typing:

```sh
dorm shell --write
```

```text
dorm> :models
dorm> insert Tenant slug=acme name=ACME
dorm> Tenant slug=acme
dorm> :quit
```

`--write` enables the DML verbs (`insert`/`update`/`delete`) and interactive
transactions (`:begin`/`:commit`/`:rollback`); without it the shell is read-only.

## 6. Browse and edit in the browser

```sh
dorm studio --write
#  open  http://127.0.0.1:5555
```

[DORM Studio](studio.md) gives you a spreadsheet-like view of every table —
filter, search, edit a cell, follow a foreign key, add and delete rows, export
CSV. Omit `--write` to browse read-only.

## Where to go next

- **[The schema](schema.md)** — every field type and attribute.
- **[Models](models.md)** — what `dorm generate` produces, and the typed API.
- **[Making queries](queries.md)** — the whole `QuerySet` API.
- **[Migrations](migrations.md)** — version your schema changes.
- **[Managing your database](managing-the-database.md)** — inspect, migrate and
  browse day to day.
