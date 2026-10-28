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
    write(root.join("src/lib.rs"), &render(LIB_RS))?;
    write(root.join("src/models.rs"), MODELS_RS)?;
    write(root.join("src/main.rs"), &render(MAIN_RS))?;
    write(root.join("src/bin/migrate.rs"), &render(MIGRATE_RS))?;
    write(root.join("src/bin/sync.rs"), &render(SYNC_RS))?;
    write(root.join("src/bin/shell.rs"), &render(SHELL_RS))?;

    println!("created `{name}`.\n");
    println!("next:");
    println!("  cd {name}");
    println!("  cargo run                      # the schema + a sample query, no database");
    println!(
        "  export DELONIX_ORM_DSN='host=/var/run/postgresql user={} dbname={}'",
        whoami(),
        krate
    );
    println!("  dorm push                      # create tables/columns to match your models");
    println!("  dorm shell                     # a REPL over your models");
    Ok(())
}

fn write(path: impl AsRef<Path>, contents: &str) -> CliResult {
    fs::write(&path, contents)
        .map_err(|e| format!("writing {}: {e}", path.as_ref().display()).into())
}

fn whoami() -> String {
    std::env::var("USER").unwrap_or_else(|_| "postgres".to_string())
}

/// `dorm new model <Name>` — append a model struct to `src/models.rs` and
/// register it in `all_models()` in `src/lib.rs`.
pub fn new_model(name: &str) -> CliResult {
    if !is_pascal(name) {
        return Err(format!("`{name}` is not a PascalCase type name (e.g. `Post`)").into());
    }
    let models_path = Path::new("src/models.rs");
    let lib_path = Path::new("src/lib.rs");
    if !models_path.exists() || !lib_path.exists() {
        return Err("not inside a DORM project — `src/models.rs` / `src/lib.rs` are missing (run `dorm init` first)".into());
    }

    // Append the struct, refusing a duplicate.
    let mut models = fs::read_to_string(models_path)?;
    if models.contains(&format!("struct {name} ")) || models.contains(&format!("struct {name}\n")) {
        return Err(format!("a model `{name}` is already defined in src/models.rs").into());
    }
    let table = pluralize(&snake(name));
    let stub = MODEL_STRUCT
        .replace("__NAME__", name)
        .replace("__TABLE__", &table);
    if !models.ends_with('\n') {
        models.push('\n');
    }
    models.push_str(&stub);
    write(models_path, &models)?;

    // Register it: insert the meta line above the marker the scaffold left.
    const MARKER: &str = "// dorm:models";
    let mut lib = fs::read_to_string(lib_path)?;
    let entry = format!("        <models::{name} as Model>::meta(),\n");
    if let Some(pos) = lib.find(MARKER) {
        let line_start = lib[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
        lib.insert_str(line_start, &entry);
        write(lib_path, &lib)?;
        println!("added model `{name}` (table `{table}`):");
        println!("  src/models.rs  — the struct");
        println!("  src/lib.rs     — registered in all_models()");
    } else {
        // An older project without the marker: do the safe half, tell the user the rest.
        println!("added model `{name}` (table `{table}`) to src/models.rs.");
        println!("src/lib.rs has no `{MARKER}` marker — add this line to all_models() yourself:");
        println!("    <models::{name} as Model>::meta(),");
    }
    Ok(())
}

/// A type name Rust (and the derive macro) will accept: an uppercase ASCII start,
/// alphanumeric throughout.
fn is_pascal(s: &str) -> bool {
    matches!(s.chars().next(), Some(c) if c.is_ascii_uppercase())
        && s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// `PascalCase` → `snake_case`, for the default table name.
fn snake(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i != 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// A good-enough English pluraliser for the default table name: `category` →
/// `categories`, `box` → `boxes`, `post` → `posts`. Rename the table in the
/// `#[model(table = …)]` attribute if the guess is wrong.
fn pluralize(word: &str) -> String {
    let consonant_y = word.ends_with('y')
        && word
            .chars()
            .rev()
            .nth(1)
            .map(|c| !"aeiou".contains(c))
            .unwrap_or(false);
    if consonant_y {
        format!("{}ies", &word[..word.len() - 1])
    } else if word.ends_with('s')
        || word.ends_with('x')
        || word.ends_with('z')
        || word.ends_with("ch")
        || word.ends_with("sh")
    {
        format!("{word}es")
    } else {
        format!("{word}s")
    }
}

const MODEL_STRUCT: &str = r#"
#[derive(Model, Debug, Clone)]
#[model(table = "__TABLE__", ordering = "-created_at")]
pub struct __NAME__ {
    #[field(primary_key)]
    pub id: i64,
    #[field(max_length = 200)]
    pub name: String,
    #[field(auto_now_add)]
    pub created_at: DateTime<Utc>,
}
"#;

const CARGO_TOML: &str = r#"[package]
name = "__APP_NAME__"
version = "0.1.0"
edition = "2021"
# The binaries share the library; `cargo run` means the demo.
default-run = "__APP_NAME__"

[dependencies]
dorm-core = "0.2"
dorm-derive = "0.2"
dorm-migrations = "0.3"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
chrono = { version = "0.4", features = ["clock"] }
"#;

const GITIGNORE: &str = "/target\n**/*.rs.bk\n.env\n";

const README: &str = r#"# __APP_NAME__

A DORM project.

```sh
cargo run                       # the generated schema + a sample query (no database)

export DELONIX_ORM_DSN='host=/var/run/postgresql user=me dbname=__APP_CRATE__'
dorm push                       # create tables/columns to match your models
dorm shell                      # a REPL over your models (= cargo run --bin shell)
dorm db tables                  # browse the live database
```

Add a model with `dorm new model <Name>` (or by hand in `src/models.rs`, then
list it in `all_models()` in `src/lib.rs`), then `dorm push` to create its table.
`dorm migrate` is the versioned alternative to `dorm push`.
"#;

const LIB_RS: &str = r#"//! The project's models, and the one list the tools read.

pub mod models;

use dorm_core::meta::ModelMeta;
use dorm_core::model::Model;

/// Every model in this project. The migrator and the demo read this; add a model
/// in `models.rs`, then add it here.
pub fn all_models() -> Vec<&'static ModelMeta> {
    vec![
        <models::Tenant as Model>::meta(),
        // dorm:models — `dorm new model <Name>` inserts new models above this line.
    ]
}
"#;

const MODELS_RS: &str = r#"//! Your models. Each `#[derive(Model)]` registers itself and generates its
//! typed field handles (e.g. `tenant::slug`).

use chrono::{DateTime, Utc};
use dorm_derive::Model;

#[derive(Model, Debug, Clone)]
#[model(table = "tenants", ordering = "-created_at")]
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

const MIGRATE_RS: &str = r#"//! `dorm migrate` runs this: build the initial migration from the models and
//! apply it to DELONIX_ORM_DSN. Idempotent — a second run is a no-op.

use dorm_migrations as mig;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let models = __APP_CRATE__::all_models();
    let initial = mig::initial(&models)?;
    let applied = mig::migrate(std::slice::from_ref(&initial)).await?;
    if applied.is_empty() {
        println!("up to date — nothing to apply.");
    } else {
        println!("applied version(s): {applied:?}");
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
