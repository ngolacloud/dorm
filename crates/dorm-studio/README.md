# dorm-studio

DORM Studio — a [Prisma-Studio](https://www.prisma.io/docs/studio)-style web UI
over your [DORM](https://crates.io/crates/dorm-cli) models: browse, filter,
search and page every table, follow relations both ways, and (with `--write`)
create, edit and delete rows.

This is the **typed** Studio: it reads the shape of your data from the registry
your models populate, so it knows your relations by name (`Tenant.apps`) and your
declared types, lengths and uniqueness — what a database-only introspection
cannot. Reads and writes go through the ORM's compiler and its read-only guard.

## Use

`dorm init` writes a `src/bin/studio.rs` that links your models and calls
[`serve`]; run it with:

```sh
cargo run --bin studio -- --write      # omit --write to browse read-only
```

Regenerate the binary with `dorm studio generate`. For a zero-build Studio that
works against any PostgreSQL by reading `information_schema` instead, use the
native `dorm studio` from the `dorm` CLI.

The single page it serves ([`INDEX_HTML`]) is shared with that native Studio, so
the two are one UI over one JSON API.

[`serve`]: https://docs.rs/dorm-studio/latest/dorm_studio/fn.serve.html
[`INDEX_HTML`]: https://docs.rs/dorm-studio/latest/dorm_studio/constant.INDEX_HTML.html
