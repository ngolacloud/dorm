# ADR 0014 — Reversible migrations: `down`, `revert`, and honest irreversibility

## Status

Accepted and implemented. The second production blocker from ADR 0011's Phase 1
(after `prefetch`, ADR 0013).

## Context

DORM's migrations applied forward only. A `Migration` was `{ version, name,
statements }` — the SQL to apply — with no inverse, so a mistaken or
experimental migration could not be rolled back without hand-written SQL. Django
(`migrate <app> <version>` backwards) and Alembic (`downgrade`) both reverse
migrations; DORM could not, which is a real gap for anyone iterating on a schema
or needing to undo a bad deploy.

The pieces to reverse already existed: the autodetector has **both** snapshots
(the previous schema and the current models), and `ddl` already builds the
inverse statements (`drop_table`, `drop_column`, `drop_index`, `rename_column`).
What was missing was recording the inverse and a path to run it.

## Decision

### A migration carries its inverse

`Migration` gains a `down: Vec<String>` — the statements that undo it. It is
persisted in the on-disk history (`StoredMigration`, `#[serde(default)]` so a
pre-reversibility history loads as irreversible, the safe reading).

- **`initial`** — up is "create every table"; down is "drop every table", in
  reverse declaration order so a child goes before the parent it references.
- **`autodetect`** — the inverse is computed from the two snapshots directly
  (`down_for`): an additive change (new tables, columns, indexes, renames) is
  inverted by dropping what it added and renaming back what it renamed.

### Irreversible migrations record *no* `down` — and say so

A migration that drops a column or table, changes a type, or removes an index
cannot be undone without the data or definition it discarded. Rather than
fabricate a wrong inverse, `down_for` returns **empty** for any migration that is
not losslessly reversible — and it is all-or-nothing: a migration mixing an add
and a drop is irreversible as a whole, never partially reversed into a
half-state. This is the same honesty as Django's non-reversible migrations, and
the same rule ADR 0011 sets for capabilities: never a silently-wrong result.

### `revert(migrations, to_version)`

Rolls the schema back to `to_version`, undoing every applied migration above it
**newest-first, in one transaction**, under the same advisory lock as `migrate`.
Each crossed migration's `down` runs and its ledger row is deleted. At the first
migration whose `down` is empty (an irreversible one), `revert` **stops with a
named error before undoing anything past it** — the database is left exactly
where it was, not in a partial state. `0` undoes everything. Idempotent: a
version already absent from the ledger is skipped.

The `down` comes from the in-memory migration chain (reconstructed from the
on-disk history), not from the ledger — the ledger stays `{version, name,
applied_unix}`, unchanged, so no ledger migration is needed and the two migrators
ADR 0072 will fold stay compatible.

### `dorm migrate down <to>`

The CLI verb: load `migrations/` (which carries each `down`), convert, and call
`revert`. Reports the versions undone, or the named refusal at an irreversible
one. `migrate down 0` tears the schema down to empty.

## Consequences

- An additive migration — the common iteration — is now reversible: `migrate
  down` undoes it and `migrate` re-applies it, repeatably.
- A destructive migration is **explicitly** irreversible and refuses by name,
  rather than losing data or leaving a half-reverted schema. If a project needs
  to reverse past one, that is a deliberate, hand-written step — which is correct,
  because the data to restore is gone.
- The on-disk history is now the source of truth for reversal, keeping `revert`
  able to run without the compiled models (only the `migrations/` directory),
  the same property `make`/`deploy` already have.

## Proof

`crates/dorm-migrations/tests/revert_sqlite.rs` —
`an_additive_migration_reverts_and_an_irreversible_one_is_refused`, on
`sqlite::memory:` (no server): applying `add_body` creates the column, `revert`
to the previous version drops it and forgets the ledger row, re-applying brings
it back; then a `drop_body` migration records an empty `down`, and reverting
across it is refused by name with the ledger left untouched. The migrator is
backend-agnostic (the SQLite half is what `migrate_deploy_sqlite` also pins); the
only backend difference — the advisory lock and the `$n`/`?n` placeholder — is
handled exactly as `migrate` handles it.
