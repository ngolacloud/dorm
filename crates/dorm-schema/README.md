# dorm-schema

The `dorm.schema` parser and code generator for **DORM**. A Prisma-style
declarative schema, compiled — because DORM is a compile-time ORM — to Rust
`#[derive(Model)]` structs. The schema is the single source of truth; you edit
it and re-run `dorm generate`, never the generated Rust.

```text
datasource db { provider = "postgresql"  url = env("DELONIX_ORM_DSN") }

model Tenant {
  id        Int      @id @default(autoincrement())
  slug      String   @unique @index @db.VarChar(63)
  name      String   @db.VarChar(200)
  createdAt DateTime @default(now())
  apps      Application[]
}
```

→

```rust
#[derive(Model, Debug, Clone)]
#[model(table = "tenants")]
pub struct Tenant {
    #[field(primary_key)]
    pub id: i64,
    #[field(unique, index, max_length = 63)]
    pub slug: String,
    #[field(max_length = 200)]
    pub name: String,
    #[field(auto_now_add)]
    pub created_at: DateTime<Utc>,
}
```

Pure: text in, text out, no I/O. The [`dorm`](https://crates.io/crates/dorm-cli)
CLI reads and writes the files. See the
[project README](https://github.com/ngolacloud/dorm).

## License

Licensed under either of Apache License 2.0 or MIT, at your option.
