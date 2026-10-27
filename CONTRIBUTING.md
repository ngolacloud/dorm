# Contributing

## The loop

```sh
make check        # cargo fmt --check + cargo clippy -D warnings + cargo test
```

Everything in `make check` runs **offline**: the compiler is pure and the
snapshot tests never open a connection, so no database has to be up. `make
check` is exactly what CI runs on the fast lane.

The live suites need a throwaway PostgreSQL and are opt-in:

```sh
make test-live DSN='host=127.0.0.1 user=me dbname=throwaway'
```

CI runs these too, against a PostgreSQL service container — see
`.github/workflows/ci.yml` (and the GitLab mirror `.gitlab-ci.yml`).

## Rules that are tests

- **The generated SQL is pinned.** `crates/dorm-models/tests/sql_snapshots.rs`
  renders each query's SQL *and its parameters in binding order* to a snapshot.
  Change the compiler and the snapshot fails until you review the diff. A new
  lookup or clause means a new snapshot: run the test, inspect the `.snap.new`,
  and promote it (rename to `.snap`, or `cargo insta accept` if you have
  `cargo-insta`).
- **No identifier comes from a caller.** `dorm_core::registry::resolve` turns a
  filter key into a `&'static FieldMeta` or an error; the compiler quotes only
  those static strings. A change that lets a caller's string reach the SQL as an
  identifier is a bug, not a feature.
- **No value is formatted into SQL.** Every value becomes a `$n` placeholder.
- **The schema generator is proven against a frozen oracle.**
  `crates/dorm-models/tests/schema_parity.rs` holds the hand-written DDL the
  generator must reproduce.

## Commit messages

[Conventional Commits](https://www.conventionalcommits.org): `feat:`, `fix:`,
`docs:`, `refactor:`, `test:`, `chore:`, `ci:`, with a `!` or a `BREAKING
CHANGE:` footer for anything that changes the public API or the generated SQL in
a way that is not backward compatible.

## Conventions

- Keep the compiler **pure** — no I/O in `dorm-core::compiler`. That is what
  makes a snapshot the right instrument.
- The two front ends (typed handles and `filter_str`) must build the *same*
  `Plan`. Parity is itself a snapshot test
  (`filter_by_string_key_compiles_the_same_as_the_typed_form`).
- A model crate should need only `dorm-core` and `dorm-derive` as dependencies.
  Do not make a model definition reach for `tokio-postgres` directly.
