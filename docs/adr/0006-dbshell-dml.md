# ADR 0006 — The dbshell gains DML

## Status

Accepted. Brick 1 (this ADR): the `dorm` shell runs `insert` / `update` /
`delete` through the compiler, previews SQL with `:sql`, and runs the ADR 0005
escape hatch with `:raw` — read-only by default, `--write` to opt in.
Interactive multi-line transactions (`:begin`/`:commit`/`:rollback`) are brick 2.

## Context

`dorm shell` was read-only: it enumerated the linked models and ran string-typed
`SELECT`s, and that was all. The roadmap's shell (Phase 4) promises "CRUD,
explicit transactions, SQL inspection, a read-only profile by default" — the DML
half of what a Prisma/Django user reaches a shell *for*. The question was how to
add writes without giving the dynamic front end a weaker safety story than the
typed one.

## Decision

### DML goes through the same compiler, model-erased

The write verbs do not build SQL. They build a `Plan` and column/assignment
lists from the typed-string input and hand them to the **same** compiler the
typed API uses, which already validates every column name against the model's
metadata (`Error::UnknownField`) and quotes only the `&'static` column strings.
So a shell `update` is exactly as injection-proof as `Manager::update`: the only
identifiers that reach SQL are ones the derive macro put in the binary, and every
value is a bound parameter.

The plumbing lives in `dorm-core` next to `fetch_value_rows`, not in the shell:
`insert_value_rows`, `update_value_rows`, `delete_value_rows`. The shell stays a
thin parser, and the forthcoming `dorm db execute` CLI reuses the same three
functions rather than re-deriving them.

The surface:

| Command | Does |
|---|---|
| `insert <Model> col=value …` | Insert one row; prints it back with generated columns (serial key, `auto_now_add`). |
| `update <Model> [filter …] set col=value …` | Update the selected rows. `set` splits filters from assignments. |
| `delete <Model> filter …` | Delete the selected rows. |
| `:sql <command>` | Compile any of the above (or a query) and print its SQL + bound params, without running. |
| `:raw <sql>` | Run raw SQL (ADR 0005): a leading `select`/`with`/… reads; anything else writes. |

### Three guardrails, because writes are new here

- **Read-only by default.** The shell calls `set_read_only(true)` unless started
  with `--write`. A careless paste into a default session cannot write; the
  refusal is `db::guard_write`'s, the same one the typed API gives.
- **No unfiltered delete by accident.** `delete <Model>` with no filter is
  refused; `delete <Model> all` is the explicit way to mean the whole table —
  the roadmap's "destructive operations require an explicit opt-in."
- **Inspect before you run.** `:sql` shows the exact statement and its bound
  parameters, so a write can be read before it is run.

## Consequences

- The dynamic front end now covers CRUD, with the typed front end's safety intact
  — the agreement of the two is still "one compiler," now for writes too.
- A read-only session is genuinely read-only: DML, `:raw` writes, and the typed
  path all route through `guard_write`.
- The model-erased write helpers are the reuse seam for `dorm db execute`.

## Not yet (brick 2 and beyond)

- **Interactive transactions.** `:begin … :commit` cannot use today's
  closure-scoped `atomic()`: the `CURRENT_TX` task-local does not survive across
  the separate `block_on` the REPL does per line. The two honest options are (a)
  buffer the statements between `:begin` and `:commit` and run them in one
  `atomic()` closure, or (b) hold a dedicated connection outside the pool across
  lines with a manual `BEGIN`/`COMMIT`. That decision is brick 2.
- Rich display for `:raw` reads (no column metadata is available for arbitrary
  SQL — use a typed query or `Manager::raw` to see columns), autocomplete,
  history, and the accounts shell (roadmap Phase 5).
