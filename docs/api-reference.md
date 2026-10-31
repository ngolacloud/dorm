# API reference (docs.rs)

The complete, always-current API documentation is generated from the source by
[docs.rs](https://docs.rs) on every release — one page per published crate, with
every public type, trait and method. This mdBook is the **guide**; docs.rs is the
**reference**.

| Crate | What it is | API docs |
|---|---|---|
| `dorm-core` | The ORM runtime: the lazy, chainable `QuerySet`, the `db` query layer, the connection pool and the `prelude`. | [docs.rs/dorm-core](https://docs.rs/dorm-core/latest/dorm_core/) |
| `dorm-derive` | The `#[derive(Model)]` procedural macro — it emits the per-model column constants and the `ModelMeta` both front ends read. | [docs.rs/dorm-derive](https://docs.rs/dorm-derive/latest/dorm_derive/) |
| `dorm-migrations` | The migration generator: reads the models' `ModelMeta` and produces the schema changes. | [docs.rs/dorm-migrations](https://docs.rs/dorm-migrations/latest/dorm_migrations/) |
| `dorm-schema` | The `dorm.schema` parser and code generator (Prisma-style schema → Rust models). | [docs.rs/dorm-schema](https://docs.rs/dorm-schema/latest/dorm_schema/) |
| `dorm-cli` | The installable project manager (`cargo install dorm-cli`, binary `dorm`): scaffolds a project and manages a live database. | [docs.rs/dorm-cli](https://docs.rs/dorm-cli/latest/dorm_cli/) |
| `dorm-studio` | DORM Studio: the Prisma-Studio-style web UI over the typed models (`serve`). | [docs.rs/dorm-studio](https://docs.rs/dorm-studio/latest/dorm_studio/) |

> `dorm-models`, `dorm-example` and `dorm-shell` are example/internal crates and
> are intentionally not published, so they have no docs.rs page — read their
> source in the repository.

Each link resolves to the `latest` published release; pick an older version from
the version selector at the top of any docs.rs page.
