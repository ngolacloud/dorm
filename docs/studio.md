# DORM Studio

A **Prisma-Studio-style web UI** over your live database: browse, filter, search
and page every table, follow relations both ways, and — with `--write` — create,
edit and delete rows, all from the browser.

Studio comes in two flavours that share **one page** and **one JSON contract**,
so they look and behave identically; they differ only in where they learn the
shape of your data.

| | **Native** (`dorm studio`) | **Typed** (`cargo run --bin studio`) |
|---|---|---|
| Ships in | the `dorm` CLI | the `dorm-studio` crate, linked by your project |
| Reads the schema from | the live database (`information_schema`) | the **registry your models populate** |
| Needs a project build | no — runs against any PostgreSQL | yes — it links your models |
| Knows relation **names** | no (foreign-key columns only) | yes (`Tenant.apps`, reverse too) |
| Knows declared type/length/unique | from the catalogue | from the model |

Use the **native** Studio for a zero-setup look at any database; use the
**typed** Studio inside a DORM project to get your models' own relation names,
reverse relations and declared types.

## Running it

### Native

```sh
export DELONIX_ORM_DSN='host=/var/run/postgresql dbname=blog'
dorm studio --write          # omit --write to browse read-only
#  open  http://127.0.0.1:5555
```

`--port N` chooses the port (default `5555`). The listener binds `127.0.0.1`
only, so Studio is never reachable off the machine it runs on.

### Typed

`dorm init` already writes `src/bin/studio.rs`; run it:

```sh
cargo run --bin studio -- --write      # --port N to choose the port
```

Regenerate the binary at any time with `dorm studio generate`. It is a few
lines — it links your models and calls `dorm_studio::serve` — so all the logic
lives in the published `dorm-studio` crate.

## Read-only by default

Like the shell, Studio opens **read-only** and enables writes only with
`--write`. Without it the grid browses but refuses every edit: the write goes
through the same [`guard_write`](connection-and-transactions.md#read-only-mode)
the typed API uses, and the browser shows a `403` naming the operation. A
careless session cannot mutate your data.

## What you can do

### Browse, filter, search, sort

- Pick a table in the sidebar to see its rows, paged.
- **Filter** a column to an exact value, or jump there by following a foreign
  key (below).
- **Search all columns** with the search box — a case-insensitive substring
  match across every (text) column.
- Click a **column header** to sort; click again to reverse.

### Edit a cell

**Double-click** any non-key cell to edit it in place. `Enter` or clicking away
saves; `Escape` reverts. Clearing a nullable field writes `NULL`. Every edit is
a `PATCH` keyed on the row's primary key.

### Edit a relation

Double-clicking a **foreign-key** cell opens a **dropdown of the referenced
table's rows** — value is the referenced key, label is the key plus a readable
column — instead of making you type a raw id. Choosing a row sets the key. This
is Prisma Studio's relation editor.

### Follow relations

- Every foreign-key cell carries a **↗** that jumps to the referenced row
  (the referenced table, filtered to that key).
- In the **typed** Studio, a row also shows its **reverse relations** in a
  `related` column — e.g. a `Tenant` row links to `apps ↴`, which opens its
  `Application`s filtered to that tenant. This is the "view related records"
  Studio promises, and it needs the relation *names* only the registry carries.

### Create a row

Click **+ New row** to add an inline editable row at the top. Leave a column
blank to take its database default (a serial key, a `now()` timestamp); fill the
rest and save.

### Delete rows

Select rows with the **checkboxes** (or the header's select-all) and click
**🗑 Delete**. A bulk delete runs inside **one transaction** — every selected row
goes or none does, so a foreign-key restriction on one row leaves the whole
selection untouched rather than half-deleted.

### Export CSV

The **⬇ CSV** button downloads the **current view** — the same filter, search and
sort as the grid — as an [RFC 4180](https://www.rfc-editor.org/rfc/rfc4180) CSV
(fields with a comma, quote or newline are quoted, quotes doubled), served as a
file attachment. The export is capped at 100 000 rows.

## Safety

Studio keeps the ORM's guarantees end to end:

- **Every identifier** — table, column, order field — is validated against the
  catalogue (native) or the registry (typed) before it is quoted into a
  statement; an unknown name is a `400`, never interpolated.
- **Every value** is a bound parameter. The native Studio casts text through
  SQL (`$n::text::<type>`); the typed Studio coerces each value to its field's
  declared type and binds it. Only a parsed integer `LIMIT`/`OFFSET` is ever
  formatted in.
- **Loopback only** — the server binds `127.0.0.1`.
- **No key, no write** — an update or delete with no key filter is refused, so
  the grid cannot wipe a table by a slip.

## The JSON API

Both Studios serve the same single page from `/` and the same API, so the UI is
one file over one contract:

| Method & path | Does |
|---|---|
| `GET /api/schema` | Every table/model: columns, types, keys, relations (and, typed, reverse relations), plus whether the session may write. |
| `GET /api/tables/<t>/rows` | A page of rows with the matching total. Query: `limit`, `offset`, `orderBy`, `dir`, `filterCol`+`filterVal`, `search`. |
| `POST /api/tables/<t>/rows` | Insert one row from `{ "values": { … } }`; returns it with generated columns. |
| `PATCH /api/tables/<t>/rows` | Update the row a key identifies: `{ "pk": { … }, "values": { … } }`. |
| `DELETE /api/tables/<t>/rows` | Delete one (`{ "pk": … }`) or many (`{ "keys": [ … ] }`, atomic). |
| `GET /api/tables/<t>/export.csv` | The current view as a CSV download. |

## See also

- [The CLI](cli-contract.md) — where `dorm studio` sits among the commands.
- [Connection & transactions](connection-and-transactions.md) — the read-only
  guard Studio shares, and the transaction a bulk delete runs in.
