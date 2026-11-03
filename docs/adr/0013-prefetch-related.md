# ADR 0013 — `prefetch`: the reverse relation without N+1

## Status

Accepted and implemented. The second half of the relation-loading story whose
forward half is `select_related`; the first of the production blockers in ADR
0011's Phase 1.

## Context

`select_related(fk)` already loads a **forward** relation — a row's foreign key —
with a `JOIN` in the same statement. The **reverse** direction, a parent's *many*
children, is the one that silently becomes N+1: list 100 tenants, then read each
one's applications, and a naïve ORM issues 1 + 100 queries. It cannot be the same
`JOIN` — joining the children multiplies the parent rows — so it must be a second
query. The defect is doing that second query *per parent* instead of once for the
batch.

DORM had the pieces: the registry resolves a reverse relation
(`registry::reverse_relation`, already used by `annotate(Count(...))`), the `in`
lookup compiles to `IN ($1, $2, …)`, and every instance can report its own column
values. What was missing was the batched loader.

## Decision

A free function, `prefetch::<P, C>(parents, related_name) -> Prefetched<C>`, that
loads a reverse relation for a whole slice of parents in **one** query.

```rust
let tenants: Vec<Tenant> = Tenant::objects().all().await?;        // query 1
let apps = prefetch::<Tenant, Application>(&tenants, "apps").await?; // query 2 — once
for t in &tenants {
    for app in apps.of(t) { /* … */ }                             // no more queries
}
```

It resolves `related_name` to the child model and the foreign key pointing back
at `P`, collects the distinct parent keys, runs one
`SELECT … WHERE <fk> IN (keys)`, and groups the children under the parent their
foreign key names.

### Why a returned lookup, not children grafted onto the parent

Django caches prefetched children on the instance
(`tenant.apps.all()`), because its models are identity-mapped entities. DORM's
models are plain structs with no hidden cache, and mutating them to carry a typed
child collection would mean either a field the user must declare or
`dyn Any`-shaped storage. So `prefetch` returns a `Prefetched<C>` the caller
indexes by parent — `apps.of(&tenant)` — which is honest about the data model
(values and an index, not mutated entities) and keeps `C` fully typed. A parent
with no children gets an empty slice, never a panic.

### Why it is a read like any other

`prefetch` runs through the typed `QuerySet`, so its one statement is classified
`Read` and is replica-eligible (ADR 0010) and refused inside a CQRS query's
read-only scope only if it tried to write — which it never does (ADR 0012). The
batched query composes with the rest of the engine rather than bypassing it.

### Scope

- Reverse foreign key (one-to-many), keyed on `related_name`. Many-to-many
  prefetch (over the junction) and nested prefetch (`apps → deployments`) are
  natural follow-ups on the same shape; not in this ADR.
- Keys are grouped by a variant-tagged rendering of the key `Value`, so an
  `I64(5)` and a `Text("5")` never collide. `Value` is not `Hash`, which is why
  the grouping is by string key rather than `HashMap<Value, _>`.

## Consequences

- The list-then-children pattern costs 2 queries, not 1 + N — the N+1 blocker
  from the production audit is closed for the reverse FK direction.
- `select_related` + `prefetch` together cover both relation directions, matching
  the shape users coming from Django expect.
- No change to the model derive or the model structs; `prefetch` is an additive
  function over the existing typed query path.

## Proof

`crates/dorm-models/tests/prefetch_live.rs` —
`prefetch_loads_all_children_in_one_query`, live against PostgreSQL. Three
tenants with 3 / 1 / 0 applications; a capturing `tracing` subscriber on the
`dorm::db` target counts the statements `prefetch` issues and asserts it is
**exactly one**, regardless of parent count, and that each tenant is grouped with
precisely its own applications.
