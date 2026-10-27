# Architecture

DORM is **two front ends over one compiler**. The whole design exists to make
that single compiler the only place SQL is built, so that safety is a property
of construction rather than of discipline.

```text
  #[derive(Model)]  ──submits──▶  registry (a linker-built slice)
        │                              │
   typed handles                  model metadata
        │                              │
        ▼                              ▼
     Q / F  ──────────▶  Plan  ──────────▶  compiler  ──▶  Sql { text, params }
                           ▲                                    │
                  the dorm REPL builds                          ▼
                  the same Plan                         db::query / db::execute
```

## The pieces

- **`dorm-derive`** turns a struct into a model at compile time: it emits the
  `&'static ModelMeta`, submits it into an `inventory` slice, and generates the
  typed field handles (`tenant::slug`). Adding a model is a recompile, not an
  edit to a registration list.
- **`dorm-core::registry`** is the only place a caller's string becomes an
  identifier. `resolve` walks a filter path (`apps__name__icontains`) against the
  metadata and returns a `&'static FieldMeta` and the joins to reach it, or an
  error. Forward foreign keys and reverse relations (by `related_name`) both
  resolve here.
- **`dorm-core::compiler`** is **pure**: a `Plan` in, parameterised `Sql` out, no
  I/O. Values become `$n` placeholders; identifiers are the static strings the
  registry handed back. Purity is what lets the snapshot tests pin both the
  generated text and the parameter order.
- **`dorm-core::queryset`** is the typed, lazy, immutable `QuerySet`/`Manager`,
  plus the model-erased `fetch_value_rows` the REPL uses.
- **`dorm-core::db`** owns the pool and transactions. One rule dominates: the
  pool belongs to the runtime that built it, so a long-lived caller (the REPL)
  keeps a single runtime for its whole session.
- **`dorm-migrations`** reads the same `ModelMeta` the queries read and generates
  reviewable DDL and a versioned migrator.

## The two front ends

The typed API (`tenant::slug.eq("acme")`) and the string-keyed API
(`.filter_str("slug", "acme")`, which the `dorm` REPL is built on) converge on
the same `Q`/`Plan`. They are not two code paths that happen to agree — their
agreement is a snapshot test
(`filter_by_string_key_compiles_the_same_as_the_typed_form`). The typed API buys
compile-time type safety; the dynamic one is still safe at runtime because it
cannot name a table or column the derive macro did not put in the binary.

## Invariants, and where they are enforced

| Invariant | Enforced by |
|---|---|
| No identifier comes from a caller | `registry::resolve` → `&'static FieldMeta`; compiler quotes only static strings |
| No value is formatted into SQL | every `Value` is bound as `$n` (`value::Value`'s `ToSql`) |
| The compiler does no I/O | `dorm-core::compiler` has no DB dependency; snapshot tests |
| The generated SQL does not drift | `tests/sql_snapshots.rs` pins text **and** parameter order |
| The schema generator is correct | `tests/schema_parity.rs` holds the frozen DDL oracle |
