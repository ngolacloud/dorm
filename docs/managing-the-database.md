# Managing your database

Day to day, you'll want to **look at** your data, **evolve** the schema, and
occasionally **reach past** the ORM. DORM gives you one tool for each, all over
the same `DELONIX_ORM_DSN` connection. Nothing here needs a project build except
where noted — the CLI's *native* commands talk to the database directly.

## Inspect

```sh
dorm doctor                  # config, connectivity, server version, read-only
dorm db tables               # list tables in the public schema
dorm db show authors         # a table's columns and types
dorm db count authors        # row count
dorm db rows authors -n 20 --where name=Ada   # rows, filtered (bound param)
```

Every read command takes `--json` for a stable, scriptable object. See
[The CLI](cli-contract.md).

## Browse and edit — Studio

For a spreadsheet-like view with editing, open [Studio](studio.md):

```sh
dorm studio --write          # omit --write to browse read-only
#  open  http://127.0.0.1:5555
```

Filter, search, edit a cell, pick a related row, follow a foreign key, add and
delete rows, and export the current view as CSV — from the browser, with every
write still passing the read-only guard.

## Evolve the schema

Two ways to bring the database in line with your models, for two situations:

```sh
# Development: create what's missing, additively (never drops or retypes).
dorm push

# Production: versioned, reviewable migrations you apply in order.
dorm migrate make add_body    # author the next migration from your models
dorm migrate deploy           # apply the recorded chain (idempotent)
dorm migrate status           # which versions the database has applied
```

`dorm migrate diff` shows what `push` would change without touching the database.
The full story is in [Migrations](migrations.md).

## Bring an existing database under DORM

Already have a schema? Introspect it into a `dorm.schema` and generate models:

```sh
dorm db pull                  # live PostgreSQL → dorm.schema (approximate)
dorm generate                 # dorm.schema → src/models.rs
```

Review the result — `pull` recovers tables, columns, types, keys and
nullability, but not every index, length or check; the file says so.

## Reach past the ORM — raw SQL

When a statement is outside what the `QuerySet` builds, run it explicitly. Every
value is still a bound parameter; the SQL text is yours:

```sh
dorm db execute "SELECT count(*) FROM posts WHERE title ILIKE $1" -p '%rust%'
# writes need --write:
dorm db execute "UPDATE posts SET title = $1 WHERE id = $2::text::int" -p 'Hi' -p 7 --write
```

Read-only by default — a statement that isn't a plain read needs `--write` and
still passes the read-only guard. From Rust, the same valve is `db::sql` /
`db::sql_execute`; see [Connection & transactions](connection-and-transactions.md).

## A query shell

To explore with typed queries (and DML with `--write`):

```sh
dorm shell --write
```

```text
dorm> Tenant slug__startswith=ac limit=5
dorm> :begin
dorm> insert Tenant slug=acme name=ACME
dorm> :commit
```

## Next

- **[Migrations](migrations.md)** — version and apply schema changes.
- **[DORM Studio](studio.md)** — the full web UI.
- **[The CLI](cli-contract.md)** — every command and its flags.
