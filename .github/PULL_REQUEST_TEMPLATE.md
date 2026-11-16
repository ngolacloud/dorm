## What does this change do, and why?

<!-- The "why" matters more than the "what" here — the diff already shows what changed. -->

## How was this tested?

<!--
- `make check` (`cargo fmt --check` + `cargo clippy -D warnings` + `cargo test`): all clean?
- If this changes emitted SQL, filtering, scoping, or a backend: what did you run against a real
  database (the opt-in `make test-live DSN=...` suite), not just the offline snapshot tests?
  Paste the command + output.
-->

## Checklist

- [ ] `make check` is clean — `cargo fmt --all --check`, `cargo clippy --workspace --all-targets
      -- -D warnings`, and `cargo test --workspace` (all run offline)
- [ ] If this touches SQL generation, `filter_str` resolution, ordering/pagination, or a backend
      adapter, the live suite passes: `make test-live DSN='host=... user=... dbname=...'`
- [ ] If this changes the SQL DORM emits, the snapshot tests were reviewed/updated (and the new
      SQL is parameterized — no user input concatenated into a statement)
- [ ] New pure functions (parsers, filter resolvers, query builders, validators) have unit tests
- [ ] If a crate's public API changed, its version was bumped per SemVer and `CHANGELOG.md` updated
- [ ] If this touches CLI output or anything `docs/` is generated from, the generator was re-run
      and its output committed

## Does this cross a security boundary?

<!--
Anything that reaches emitted SQL (identifiers, values, raw-SQL escape hatches), relaxes or
bypasses tenant/scope isolation, or changes how credentials/DSNs/query data are logged. If yes,
describe the boundary and why the change is safe. If you're not sure whether this needs security
review, say so — better to ask. See SECURITY.md.
-->
