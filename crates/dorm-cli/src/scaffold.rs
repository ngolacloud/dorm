//! `dorm init` — write a new, runnable DORM project.
//!
//! The generated project is the honest answer to "what does a DORM project look
//! like": a library that defines the models, and three binaries over it — a demo
//! (`cargo run`), a migrator (`migrate`), and a REPL (`shell`). `dorm migrate`
//! and `dorm shell` are thin `cargo run --bin …` wrappers over the last two, so
//! the installed tool drives your *own* compiled models without pretending to
//! read them itself.

use std::fs;
use std::path::Path;

type CliResult = Result<(), Box<dyn std::error::Error>>;

pub fn init(name: Option<&str>) -> CliResult {
    let name = name.unwrap_or("dorm-app");
    // The crate name used in `use` statements: Cargo turns `-` into `_`.
    let krate = name.replace('-', "_");
    let root = Path::new(name);
    if root.exists()
        && root
            .read_dir()
            .map(|mut d| d.next().is_some())
            .unwrap_or(false)
    {
        return Err(format!("`{name}` already exists and is not empty").into());
    }

    let render = |t: &str| {
        t.replace("__APP_NAME__", name)
            .replace("__APP_CRATE__", &krate)
    };

    fs::create_dir_all(root.join("src/bin"))?;
    write(root.join("Cargo.toml"), &render(CARGO_TOML))?;
    write(root.join(".gitignore"), GITIGNORE)?;
    write(root.join("README.md"), &render(README))?;
    // The schema is the source of truth; `src/models.rs` is generated from it so
    // the fresh project is self-consistent and `dorm generate` is a no-op until
    // the schema changes.
    write(root.join("dorm.schema"), SCHEMA)?;
    let models_rs = dorm_schema::generate_rust(
        &dorm_schema::parse(SCHEMA)
            .map_err(|e| format!("internal: scaffold schema invalid: {e}"))?,
    );
    write(root.join("src/models.rs"), &models_rs)?;
    write(root.join("src/lib.rs"), &render(LIB_RS))?;
    write(root.join("src/main.rs"), &render(MAIN_RS))?;
    write(root.join("src/bin/migrate.rs"), &render(MIGRATE_RS))?;
    write(
        root.join("src/bin/makemigration.rs"),
        &render(MAKEMIGRATION_RS),
    )?;
    write(root.join("src/bin/sync.rs"), &render(SYNC_RS))?;
    write(root.join("src/bin/diff.rs"), &render(DIFF_RS))?;
    write(root.join("src/bin/validate.rs"), &render(VALIDATE_RS))?;
    write(root.join("src/bin/shell.rs"), &render(SHELL_RS))?;
    write(root.join("src/bin/studio.rs"), &render(STUDIO_RS))?;

    println!("created `{name}`.\n");
    println!("next:");
    println!("  cd {name}");
    println!("  # edit dorm.schema, then:");
    println!("  dorm generate                  # dorm.schema → src/models.rs");
    println!(
        "  export DELONIX_ORM_DSN='host=/var/run/postgresql user={} dbname={}'",
        whoami(),
        krate
    );
    println!("  dorm validate                  # check the models for mistakes");
    println!("  dorm doctor                    # check config and connectivity");
    println!("  dorm push                      # create tables/columns to match your models");
    println!("  dorm migrate make <name>       # or: author a versioned migration");
    println!("  dorm migrate deploy            #      and apply the recorded chain");
    println!("  dorm shell                     # a REPL over your models");
    println!("  cargo run --bin studio         # DORM Studio — a web UI over your models (--write to edit)");
    println!("  dorm studio                    # the same, native (any database, no build)");
    println!("  dorm api generate && dorm api serve   # an Axum read API + OpenAPI");
    Ok(())
}

/// `dorm generate` — compile `dorm.schema` into `src/models.rs`.
pub fn generate(schema_path: &str, out_path: &str) -> CliResult {
    let src = fs::read_to_string(schema_path)
        .map_err(|e| format!("reading {schema_path}: {e} (run `dorm init` first?)"))?;
    let schema = dorm_schema::parse(&src).map_err(|e| format!("{schema_path}: {e}"))?;
    let rust = dorm_schema::generate_rust(&schema);
    write(out_path, &rust)?;
    println!(
        "generated {out_path} from {schema_path} ({} model(s)).",
        schema.models.len()
    );
    Ok(())
}

/// `dorm api generate` — write an Axum read API (`src/bin/api.rs`) from the schema.
pub fn api_generate(schema_path: &str, out_path: &str) -> CliResult {
    let src = fs::read_to_string(schema_path)
        .map_err(|e| format!("reading {schema_path}: {e} (run `dorm init` first?)"))?;
    let schema = dorm_schema::parse(&src).map_err(|e| format!("{schema_path}: {e}"))?;
    let krate = package_name()?;
    let rust = dorm_schema::generate_axum(&schema, &krate);
    write(out_path, &rust)?;
    println!(
        "generated {out_path} from {schema_path} ({} model(s)).",
        schema.models.len()
    );
    println!(
        "run `dorm api serve` to serve it (needs `axum` in Cargo.toml — `dorm init` adds it)."
    );
    Ok(())
}

/// `dorm studio generate` — (re)write `src/bin/studio.rs`, a typed Studio over
/// the project's models. The file is fixed (all the logic is in `dorm-studio`),
/// so this just stamps the crate name in; `dorm init` already writes it.
pub fn studio_generate(out_path: &str) -> CliResult {
    let krate = package_name()?;
    let rust = STUDIO_RS.replace("__APP_CRATE__", &krate);
    if let Some(parent) = Path::new(out_path).parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    write(out_path, &rust)?;
    println!("generated {out_path}.");
    println!(
        "run `cargo run --bin studio` to open it (add `--write` to edit; needs \
         `dorm-studio` in Cargo.toml — `dorm init` adds it)."
    );
    Ok(())
}

/// The package's library crate name (underscored), read from `Cargo.toml`, so a
/// generated binary can `use <crate>::models`.
fn package_name() -> Result<String, Box<dyn std::error::Error>> {
    let toml = fs::read_to_string("Cargo.toml")
        .map_err(|e| format!("reading Cargo.toml: {e} (run inside a DORM project?)"))?;
    // The first `name = "..."` is the package name (the `[package]` section leads
    // the manifest in these projects).
    for line in toml.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("name") {
            if let Some(q1) = rest.find('"') {
                if let Some(q2) = rest[q1 + 1..].find('"') {
                    return Ok(rest[q1 + 1..q1 + 1 + q2].replace('-', "_"));
                }
            }
        }
    }
    Err("could not find the package name in Cargo.toml".into())
}

fn write(path: impl AsRef<Path>, contents: &str) -> CliResult {
    fs::write(&path, contents)
        .map_err(|e| format!("writing {}: {e}", path.as_ref().display()).into())
}

fn whoami() -> String {
    std::env::var("USER").unwrap_or_else(|_| "postgres".to_string())
}

/// `dorm new model <Name>` — append a `model` block to `dorm.schema`. The user
/// then runs `dorm generate` to update `src/models.rs`: the schema is the single
/// source of truth, so new models are added there, not to the generated Rust.
pub fn new_model(name: &str) -> CliResult {
    if !is_pascal(name) {
        return Err(format!("`{name}` is not a PascalCase type name (e.g. `Post`)").into());
    }
    let schema = Path::new("dorm.schema");
    if !schema.exists() {
        return Err("no `dorm.schema` here — run `dorm init` first".into());
    }
    let mut src = fs::read_to_string(schema)?;
    if src.contains(&format!("model {name} ")) || src.contains(&format!("model {name}{{")) {
        return Err(format!("a model `{name}` already exists in dorm.schema").into());
    }
    if !src.ends_with('\n') {
        src.push('\n');
    }
    src.push_str(&MODEL_BLOCK.replace("__NAME__", name));
    write(schema, &src)?;
    println!("added model `{name}` to dorm.schema.");
    println!("run `dorm generate` to update src/models.rs.");
    Ok(())
}

/// A type name Rust (and the derive macro) will accept: an uppercase ASCII start,
/// alphanumeric throughout.
fn is_pascal(s: &str) -> bool {
    matches!(s.chars().next(), Some(c) if c.is_ascii_uppercase())
        && s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// The `model` block `dorm new model` appends to `dorm.schema`.
const MODEL_BLOCK: &str = r#"
model __NAME__ {
  id        Int      @id @default(autoincrement())
  name      String   @db.VarChar(200)
  createdAt DateTime @default(now())
}
"#;

/// The starter `dorm.schema` `dorm init` writes — Prisma-style, the source of
/// truth `dorm generate` compiles to `src/models.rs`.
const SCHEMA: &str = r#"datasource db {
  provider = "postgresql"
  url      = env("DELONIX_ORM_DSN")
}

model Tenant {
  id        Int      @id @default(autoincrement())
  slug      String   @unique @index @db.VarChar(63)
  name      String   @db.VarChar(200)
  createdAt DateTime @default(now())
  posts     Post[]
}

model Post {
  id        Int      @id @default(autoincrement())
  tenant    Tenant   @relation(onDelete: Cascade)
  title     String   @index @db.VarChar(200)
  createdAt DateTime @default(now())
}
"#;

const CARGO_TOML: &str = r#"[package]
name = "__APP_NAME__"
version = "0.1.0"
edition = "2021"
# The binaries share the library; `cargo run` means the demo.
default-run = "__APP_NAME__"

[dependencies]
dorm-core = "0.6"
dorm-derive = "0.3"
dorm-migrations = "0.7"
tokio = { version = "1", features = ["full"] }
chrono = { version = "0.4", features = ["clock"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
# Present so a generated model can use any scalar (Decimal, Uuid) without a
# manual edit; unused ones cost nothing.
rust_decimal = "1"
uuid = { version = "1", features = ["v4"] }
# For the API `dorm api generate` writes (`src/bin/api.rs`).
axum = "0.7"
# For `cargo run --bin studio` — DORM Studio over your typed models.
dorm-studio = "0.1"
"#;

const GITIGNORE: &str = "/target\n**/*.rs.bk\n.env\n";

const README: &str = r#"# __APP_NAME__

A DORM project. Models live in `dorm.schema` (Prisma-style) and are compiled to
`src/models.rs` by `dorm generate` — edit the schema, not the generated Rust.

```sh
# edit dorm.schema, then:
dorm generate                   # dorm.schema → src/models.rs
cargo run                       # the generated schema + a sample query (no database)

export DELONIX_ORM_DSN='host=/var/run/postgresql user=me dbname=__APP_CRATE__'
dorm push                       # create tables/columns to match your models
dorm shell                      # a REPL over your models (= cargo run --bin shell)
dorm db tables                  # browse the live database
cargo run --bin studio -- --write   # DORM Studio — browse & edit in your browser
```

Add a model with `dorm new model <Name>` (appends to `dorm.schema`) or by editing
`dorm.schema` directly, then `dorm generate` and `dorm push`.

**DORM Studio** is a Prisma-Studio-style web UI over the live database: browse,
filter, search and page every table, follow relations both ways, and (with
`--write`) create, edit and delete rows. Two ways to run it:

* `cargo run --bin studio` — the **typed** Studio over this project's models, so
  it knows your relations by name (`Tenant.apps`) and your declared types. Regenerate
  the binary with `dorm studio generate`.
* `dorm studio` — the **native** Studio, which reads the live schema instead, so
  it needs no build and works against any PostgreSQL.

For versioned, reviewable migrations instead of `push`, use the history in
`migrations/`:

```sh
dorm migrate make add_body       # author the next migration from your models
dorm migrate deploy              # apply the recorded chain (or the initial schema)
dorm migrate status              # which versions the database has applied
```

`make` diffs your models against the last recorded migration and writes the new
one; `deploy` applies the chain in order and is idempotent. Like Django's
`makemigrations`/`migrate` and Prisma's `migrate dev`/`deploy`.
"#;

const LIB_RS: &str = r#"//! The project. The models are generated from `dorm.schema` into `models.rs`
//! by `dorm generate` — do not edit that file; edit the schema and regenerate.

pub mod models;

// The generated list every binary reads.
pub use models::all_models;
"#;

const MAIN_RS: &str = r#"//! The demo binary: prints the generated schema and a sample query's SQL. It
//! touches no database, so it runs anywhere.

use dorm_core::prelude::*;
use dorm_migrations as mig;
use __APP_CRATE__::models::{tenant, Tenant};

// `std::result::Result` spelled out: the ORM prelude brings its own one-argument
// `Result<T>` into scope, and `Box<dyn Error>` lets `?` box both error types.
fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    println!("== generated schema ==\n");
    println!("{}", mig::ddl::schema_for(&__APP_CRATE__::all_models())?);

    let sql = Tenant::objects()
        .filter(tenant::slug.startswith("a"))
        .order_by([tenant::created_at.desc()])
        .limit(5)
        .sql()?;
    println!("== a sample query (no database needed) ==\n{}", sql.text);
    Ok(())
}
"#;

const MIGRATE_RS: &str = r#"//! `dorm migrate deploy` runs this: apply the recorded migration chain in
//! `migrations/` to DELONIX_ORM_DSN, in order. When there is no history yet it
//! falls back to the initial schema straight from the models, so a fresh project
//! migrates out of the box. Idempotent — a second run is a no-op.

use dorm_migrations as mig;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let models = __APP_CRATE__::all_models();
    let history = mig::History::open("migrations");
    let chain = history.to_migrations()?;
    let migrations = if chain.is_empty() {
        // No authored migrations yet — deploy the initial schema from the models.
        vec![mig::initial(&models)?]
    } else {
        chain
    };
    let applied = mig::migrate(&migrations).await?;
    if applied.is_empty() {
        println!("up to date — nothing to apply.");
    } else {
        println!("applied version(s): {applied:?}");
    }
    Ok(())
}
"#;

const MAKEMIGRATION_RS: &str = r#"//! `dorm migrate make <name>` runs this: diff the models against the recorded
//! history in `migrations/` and write the next migration file. No database is
//! touched — the history's stored snapshot is what the diff runs against. The
//! backend is read from DELONIX_ORM_DSN (PostgreSQL if unset).

use dorm_migrations as mig;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let name = std::env::args().nth(1).unwrap_or_else(|| "migration".to_string());
    let models = __APP_CRATE__::all_models();
    let backend = mig::ddl::Backend::of_dsn(&std::env::var("DELONIX_ORM_DSN").unwrap_or_default());
    let history = mig::History::open("migrations");
    match history.make(&models, &name, backend)? {
        Some(m) => println!(
            "created migration {} ({}) — {} statement(s) in migrations/",
            m.version,
            m.name,
            m.statements.len()
        ),
        None => println!("no changes detected — the models match the latest migration."),
    }
    Ok(())
}
"#;

const SYNC_RS: &str = r#"//! `dorm push` runs this: bring the database in line with the models,
//! additively — create missing tables and add missing columns. No migration
//! history; the quick-iteration path, like `prisma db push`.

use dorm_migrations as mig;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let models = __APP_CRATE__::all_models();
    let changes = mig::sync(&models).await?;
    if changes.is_empty() {
        println!("database already matches the models — nothing to push.");
    } else {
        println!("pushed {} change(s):", changes.len());
        for c in &changes {
            println!("  {c}");
        }
    }
    Ok(())
}
"#;

const DIFF_RS: &str = r#"//! `dorm migrate diff` runs this: show the additive changes `push` would apply,
//! read-only. Pass `--json` for a stable object.

use dorm_migrations as mig;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let json = std::env::args().any(|a| a == "--json");
    let changes = mig::diff(&__APP_CRATE__::all_models()).await?;
    if json {
        println!("{}", serde_json::json!({ "changes": changes }));
    } else if changes.is_empty() {
        println!("no pending changes — the database matches the models.");
    } else {
        println!("{} pending change(s):", changes.len());
        for c in &changes {
            println!("  {c}");
        }
    }
    Ok(())
}
"#;

const VALIDATE_RS: &str = r#"//! `dorm validate` runs this: check the project's models for inconsistencies a
//! query or migration would otherwise trip over. Exits 3 if there are errors.
//! Pass `--json` for a stable object.

use dorm_core::registry::{self, Severity};

fn main() {
    // Link the models into the registry.
    let _ = __APP_CRATE__::all_models();
    let json = std::env::args().any(|a| a == "--json");
    let issues = registry::validate();
    let errors = issues.iter().filter(|i| i.severity == Severity::Error).count();

    if json {
        let arr: Vec<_> = issues
            .iter()
            .map(|i| {
                serde_json::json!({
                    "severity": i.severity.label(),
                    "model": i.model,
                    "message": i.message,
                })
            })
            .collect();
        println!("{}", serde_json::json!({ "ok": errors == 0, "issues": arr }));
    } else if issues.is_empty() {
        println!("validate: ok — the models are consistent.");
    } else {
        for i in &issues {
            let who = if i.model.is_empty() {
                String::new()
            } else {
                format!("{}: ", i.model)
            };
            println!("  {:<7} {who}{}", i.severity.label(), i.message);
        }
        println!("validate: {errors} error(s)");
    }
    std::process::exit(if errors > 0 { 3 } else { 0 });
}
"#;

const STUDIO_RS: &str = r#"//! `cargo run --bin studio` runs this: DORM Studio over THIS project's models —
//! a Prisma-Studio-style web UI to browse and (with `--write`) edit the live
//! database, with your models' own relations and declared types. `--port N`
//! chooses the port (default 5555); all the logic lives in `dorm-studio`.

#[tokio::main]
async fn main() {
    // Link the models into the registry so the Studio can enumerate them.
    let _ = __APP_CRATE__::all_models();
    let args: Vec<String> = std::env::args().collect();
    let writable = args.iter().any(|a| a == "--write");
    let port = args
        .iter()
        .position(|a| a == "--port")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(5555);
    if let Err(e) = dorm_studio::serve(port, writable).await {
        eprintln!("studio: {e}");
        std::process::exit(1);
    }
}
"#;

const SHELL_RS: &str = r#"//! `dorm shell` runs this: a small REPL over THIS project's models. `:models`
//! lists them; `<Model> key=value ...` runs a query; `:quit` leaves.

use std::io::{self, BufRead, Write};

use dorm_core::compiler::Plan;
use dorm_core::expr::Q;
use dorm_core::queryset::fetch_value_rows;
use dorm_core::value::Value;
use dorm_core::{db, registry};

#[tokio::main]
async fn main() {
    // Link the models into the registry.
    let _ = __APP_CRATE__::all_models();
    println!("shell — `:models`, `<Model> key=value ...`, `:quit`.");
    match db::dsn() {
        Ok(_) => {}
        Err(_) => println!("(no DELONIX_ORM_DSN set — `:models` works; a query needs one.)"),
    }
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    loop {
        print!("> ");
        let _ = io::stdout().flush();
        let Some(Ok(line)) = lines.next() else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == ":quit" || line == ":q" {
            break;
        }
        if line == ":models" {
            for m in registry::models() {
                let cols: Vec<&str> = m.fields.iter().map(|f| f.name).collect();
                println!("  {} ({}): {}", m.name, m.table, cols.join(", "));
            }
            continue;
        }
        if let Err(e) = run(line).await {
            eprintln!("error: {e}");
        }
    }
    println!("bye.");
}

async fn run(line: &str) -> dorm_core::Result<()> {
    let mut tok = line.split_whitespace();
    let model = tok.next().ok_or_else(|| dorm_core::Error::Config("empty query".into()))?;
    let meta = registry::model(model)?;
    let mut plan = Plan::new(meta);
    plan.limit = Some(20);
    for t in tok {
        let (k, v) = t
            .split_once('=')
            .ok_or_else(|| dorm_core::Error::Config(format!("`{t}` is not key=value")))?;
        plan.filters.push(Q::new(k.to_string(), parse(v)));
    }
    for row in fetch_value_rows(&plan).await? {
        let cells: Vec<String> = row.iter().map(|(k, v)| format!("{k}={v}")).collect();
        println!("  {}", cells.join("  "));
    }
    Ok(())
}

fn parse(raw: &str) -> Value {
    if let Ok(n) = raw.parse::<i64>() {
        return Value::I64(n);
    }
    match raw {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => Value::Text(raw.to_string()),
    }
}
"#;
