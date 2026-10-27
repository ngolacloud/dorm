# dorm-derive

The `#[derive(Model)]` macro for **DORM**. It turns a plain Rust struct into a
model: it emits the `&'static ModelMeta` and submits it into the registry, and
it generates the typed field handles (`tenant::slug`) the query API is built on.
Adding a model is a recompile, not an edit to a registration list.

```rust
use dorm_core::prelude::*;
use dorm_derive::Model;

#[derive(Model, Debug, Clone)]
#[model(table = "authors", ordering = "name")]
struct Author {
    #[field(primary_key)]
    id: i64,
    #[field(unique, max_length = 120)]
    name: String,
}
```

Use with [`dorm-core`](https://crates.io/crates/dorm-core). See the
[project README](https://github.com/ngolacloud/dorm) for the full guide.

## License

Licensed under either of Apache License 2.0 or MIT, at your option.
