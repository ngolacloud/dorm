//! `dorm` — the DORM project manager, in the spirit of `prisma`.
//!
//! DORM is a **compile-time** ORM: your models are Rust structs, registered when
//! they are linked into *your* binary. So this installed tool cannot read an
//! arbitrary project's models the way `prisma` reads `schema.prisma`. It works on
//! the two things it *can* reach without your compiled code:
//!
//! * **the files** — `dorm init` / `dorm new model` write the project; and
//! * **the database** — `dorm doctor` and `dorm db …` talk to a live PostgreSQL
//!   over `DELONIX_ORM_DSN` (or `--dsn`), diagnosing and browsing it.
//!
//! The commands that need your typed models — `dorm validate`, `dorm migrate`,
//! `dorm push`, `dorm shell` — run them the only honest way: by building and
//! running your own project through `cargo`. `dorm init` scaffolds the binaries
//! they drive.
//!
//! ## Exit codes
//!
//! * `0` success · `1` an error (message on stderr) · `2` a usage error (clap) ·
//!   `3` a command that checks things found problems (`doctor`, `validate`).
//!
//! ## `--json`
//!
//! Every read command takes `--json` for a stable, scriptable object. Human text
//! is the default. Credentials are never printed — a DSN is always redacted.

use std::process::{Command, ExitCode};

use clap::{CommandFactory, Parser, Subcommand};
use dorm_core::db;
use serde_json::json;

mod scaffold;

#[derive(Parser)]
#[command(
    name = "dorm",
    version,
    about = "Scaffold a DORM project and manage its PostgreSQL — migrations, schema, diagnostics and a row browser.",
    long_about = None
)]
struct Cli {
    /// The database to talk to, as a libpq DSN or URL. Overrides the
    /// `DELONIX_ORM_DSN` environment variable for this run. Only the
    /// live-database commands use it. The password is never logged.
    #[arg(long, global = true, value_name = "DSN")]
    dsn: Option<String>,

    /// Emit a stable JSON object instead of human text (read commands).
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Subcommand)]
enum CliCommand {
    /// Scaffold a new DORM project that builds, migrates and queries out of the box.
    Init {
        /// The project directory to create. Defaults to `dorm-app`.
        name: Option<String>,
    },
    /// Check your models for mistakes a query or migration would otherwise trip
    /// over — by building and running the project. (`cargo run --bin validate`.)
    Validate,
    /// Diagnose configuration and connectivity to the database (no project needed).
    Doctor,
    /// Migrations.
    Migrate {
        #[command(subcommand)]
        action: Option<MigrateAction>,
    },
    /// Push the schema to the database additively — create missing tables and
    /// columns to match your models — by building and running the project. Like
    /// `prisma db push`; no migration history, and only additive (never drops).
    Push,
    /// Open your project's REPL by building and running it. (`cargo run --bin shell`.)
    Shell,
    /// Add to the project in the current directory.
    New {
        #[command(subcommand)]
        what: NewKind,
    },
    /// Inspect and browse the live database (read-only).
    Db {
        #[command(subcommand)]
        action: DbAction,
    },
    /// Print a shell completion script to stdout (bash, zsh, fish, …).
    Completions {
        /// The shell to generate completions for.
        shell: clap_complete::Shell,
    },
}

#[derive(Subcommand)]
enum NewKind {
    /// Add a model: append a struct to `src/models.rs` and register it in the
    /// `all_models()` list in `src/lib.rs`.
    Model {
        /// The model's type name, in PascalCase, e.g. `Post`.
        name: String,
    },
}

#[derive(Subcommand)]
enum MigrateAction {
    /// Apply reviewed migrations (the initial migration). The default when
    /// `migrate` is given no subcommand. (`cargo run --bin migrate`.)
    Deploy,
    /// Show which migration versions the database has recorded as applied.
    Status,
    /// Show the additive changes `push` would apply, without touching the
    /// database. Read-only. (`cargo run --bin diff`.)
    Diff,
}

#[derive(Subcommand)]
enum DbAction {
    /// List the tables in the `public` schema.
    Tables,
    /// Show one table's columns and their types.
    Show {
        /// The table name.
        table: String,
    },
    /// Count the rows in a table.
    Count {
        /// The table name.
        table: String,
    },
    /// Print rows from a table (read-only), to a limit.
    Rows {
        /// The table name.
        table: String,
        /// How many rows at most.
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: i64,
        /// Keep only rows where a column equals a value, compared as text:
        /// `--where slug=acme`.
        #[arg(long = "where", value_name = "COL=VALUE")]
        filter: Option<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Some(dsn) = &cli.dsn {
        // The live-database commands read the DSN from the environment, via
        // `dorm_core::db`. Setting it here lets `--dsn` win for this process.
        std::env::set_var(db::DSN_ENV, dsn);
    }
    let json = cli.json;

    let result: CliResult = match cli.command {
        CliCommand::Init { name } => scaffold::init(name.as_deref()).map(|()| 0),
        CliCommand::New {
            what: NewKind::Model { name },
        } => scaffold::new_model(&name).map(|()| 0),
        CliCommand::Validate => cargo_run("validate", json),
        CliCommand::Doctor => on_runtime(doctor(json)),
        CliCommand::Migrate { action } => match action.unwrap_or(MigrateAction::Deploy) {
            MigrateAction::Deploy => cargo_run("migrate", false),
            MigrateAction::Status => on_runtime(migrate_status(json)),
            MigrateAction::Diff => cargo_run("diff", json),
        },
        CliCommand::Push => cargo_run("sync", false),
        CliCommand::Shell => cargo_run("shell", false),
        CliCommand::Db { action } => on_runtime(run_db(action, json)),
        CliCommand::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "dorm", &mut std::io::stdout());
            Ok(0)
        }
    };

    match result {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `Ok(exit_code)` — 0 for success, 3 when a check found problems — or an error.
type CliResult = Result<i32, Box<dyn std::error::Error>>;

/// Run a future on a fresh multi-threaded runtime — one for the whole command,
/// as `dorm_core::db` requires (the pool belongs to the runtime that built it).
fn on_runtime(fut: impl std::future::Future<Output = CliResult>) -> CliResult {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(fut)
}

/// The cargo lane: build and run one of the project's binaries, forwarding
/// `--json` when asked. Its exit code becomes ours, so a `validate` that exits 3
/// surfaces as 3.
fn cargo_run(bin: &str, json: bool) -> CliResult {
    let mut cmd = Command::new("cargo");
    cmd.arg("run").arg("--quiet").arg("--bin").arg(bin);
    if json {
        cmd.arg("--").arg("--json");
    }
    let status = cmd.status().map_err(|e| {
        format!(
            "could not run `cargo` (is it on your PATH, and are you inside a DORM project?): {e}"
        )
    })?;
    Ok(status.code().unwrap_or(1))
}

/// Hide the password in a DSN before echoing it. libpq spells the secret
/// `password=…`; a URL spells it `user:pass@host`.
fn redact(dsn: &str) -> String {
    // URL form: scheme://user:pass@rest  → mask between the first ':' after '//'
    // and the '@'.
    if let Some(scheme_end) = dsn.find("://") {
        let (head, rest) = dsn.split_at(scheme_end + 3);
        if let Some(at) = rest.find('@') {
            let creds = &rest[..at];
            if let Some(colon) = creds.find(':') {
                return format!("{head}{}:***{}", &creds[..colon], &rest[at..]);
            }
        }
    }
    // libpq keyword form: redact any `password=...` token.
    dsn.split_whitespace()
        .map(|t| {
            if t.starts_with("password=") {
                "password=***".to_string()
            } else {
                t.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// One line of `dorm doctor`'s report.
struct Check {
    name: &'static str,
    status: &'static str,
    detail: String,
}

/// `dorm doctor`: config → connectivity → version → read-only → ledger. Native —
/// it needs only a DSN, not your compiled models. Returns 3 if any check errored.
async fn doctor(json: bool) -> CliResult {
    let mut checks: Vec<Check> = Vec::new();

    match db::dsn() {
        Ok(dsn) => checks.push(Check {
            name: "datasource",
            status: "ok",
            detail: redact(&dsn),
        }),
        Err(e) => {
            checks.push(Check {
                name: "datasource",
                status: "error",
                detail: e.to_string(),
            });
            return report_doctor(json, &checks);
        }
    }

    match db::query("SELECT 1", &[]).await {
        Ok(_) => checks.push(Check {
            name: "connectivity",
            status: "ok",
            detail: "connected".into(),
        }),
        Err(e) => {
            // Redact in case the driver echoed the DSN in its error.
            checks.push(Check {
                name: "connectivity",
                status: "error",
                detail: redact(&e.to_string()),
            });
            return report_doctor(json, &checks);
        }
    }

    if let Ok(rows) = db::query("SELECT version()", &[]).await {
        if let Some(row) = rows.first() {
            checks.push(Check {
                name: "server",
                status: "ok",
                detail: row.get::<_, String>(0),
            });
        }
    }

    if let Ok(rows) = db::query("SELECT current_setting('transaction_read_only')", &[]).await {
        let ro: String = rows.first().map(|r| r.get(0)).unwrap_or_default();
        checks.push(Check {
            name: "read_only",
            status: "ok",
            detail: if ro == "on" {
                "on".into()
            } else {
                "off".into()
            },
        });
    }

    if let Ok(rows) = db::query(
        "SELECT to_regclass('public.orm_schema_migrations')::text",
        &[],
    )
    .await
    {
        let present = rows
            .first()
            .map(|r| r.get::<_, Option<String>>(0))
            .unwrap_or(None);
        checks.push(Check {
            name: "migrations_ledger",
            status: "ok",
            detail: if present.is_some() {
                "present".into()
            } else {
                "absent".into()
            },
        });
    }

    report_doctor(json, &checks)
}

fn report_doctor(json: bool, checks: &[Check]) -> CliResult {
    let problems = checks.iter().any(|c| c.status == "error");
    if json {
        let arr: Vec<_> = checks
            .iter()
            .map(|c| json!({ "name": c.name, "status": c.status, "detail": c.detail }))
            .collect();
        println!("{}", json!({ "ok": !problems, "checks": arr }));
    } else {
        for c in checks {
            let mark = match c.status {
                "ok" => "✓",
                "error" => "✗",
                _ => "!",
            };
            println!("  {mark} {:<18} {}", c.name, c.detail);
        }
        println!(
            "{}",
            if problems {
                "doctor: problems found"
            } else {
                "doctor: ok"
            }
        );
    }
    Ok(if problems { 3 } else { 0 })
}

async fn migrate_status(json: bool) -> CliResult {
    let rows = match db::query(
        "SELECT version, name, applied_unix FROM orm_schema_migrations ORDER BY version",
        &[],
    )
    .await
    {
        Ok(rows) => rows,
        Err(_) => {
            if json {
                println!("{}", json!({ "applied": [] }));
            } else {
                println!("no migrations recorded (the ledger does not exist yet).");
            }
            return Ok(0);
        }
    };
    if json {
        let arr: Vec<_> = rows
            .iter()
            .map(|r| {
                let version: i64 = r.get(0);
                let name: String = r.get(1);
                let applied_unix: i64 = r.get(2);
                json!({
                    "version": version,
                    "name": name,
                    "applied_unix": applied_unix,
                    "applied_at": applied_at(applied_unix),
                })
            })
            .collect();
        println!("{}", json!({ "applied": arr }));
        return Ok(0);
    }
    if rows.is_empty() {
        println!("the ledger is empty — no migrations applied.");
        return Ok(0);
    }
    println!("applied migrations:");
    for row in &rows {
        let version: i64 = row.get(0);
        let name: String = row.get(1);
        let applied_unix: i64 = row.get(2);
        println!("  v{version:<4} {name:<24} {}", applied_at(applied_unix));
    }
    Ok(0)
}

/// Render the ledger's Unix seconds as a readable UTC stamp, falling back to the
/// raw number if it is somehow out of range.
fn applied_at(unix: i64) -> String {
    match chrono::DateTime::from_timestamp(unix, 0) {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        None => format!("applied_unix={unix}"),
    }
}

async fn run_db(action: DbAction, json: bool) -> CliResult {
    match action {
        DbAction::Tables => db_tables(json).await,
        DbAction::Show { table } => db_show(&table, json).await,
        DbAction::Count { table } => db_count(&table, json).await,
        DbAction::Rows {
            table,
            limit,
            filter,
        } => db_rows(&table, limit, filter.as_deref(), json).await,
    }
}

async fn db_tables(json: bool) -> CliResult {
    let rows = db::query(
        "SELECT table_name FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_type = 'BASE TABLE' \
         ORDER BY table_name",
        &[],
    )
    .await?;
    let names: Vec<String> = rows.iter().map(|r| r.get::<_, String>(0)).collect();
    if json {
        println!("{}", json!({ "tables": names }));
        return Ok(0);
    }
    if names.is_empty() {
        println!("(no tables in the `public` schema)");
    }
    for name in &names {
        println!("  {name}");
    }
    Ok(0)
}

async fn db_show(table: &str, json: bool) -> CliResult {
    let rows = db::query(
        "SELECT column_name, data_type, is_nullable FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name = $1 \
         ORDER BY ordinal_position",
        &[&table],
    )
    .await?;
    if rows.is_empty() {
        return Err(format!("no table `{table}` in the `public` schema").into());
    }
    if json {
        let cols: Vec<_> = rows
            .iter()
            .map(|r| {
                json!({
                    "name": r.get::<_, String>(0),
                    "type": r.get::<_, String>(1),
                    "nullable": r.get::<_, String>(2) == "YES",
                })
            })
            .collect();
        println!("{}", json!({ "table": table, "columns": cols }));
        return Ok(0);
    }
    println!("{table}");
    let table_rows: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            let col: String = row.get(0);
            let ty: String = row.get(1);
            let nullable: String = row.get(2);
            let null = if nullable == "YES" {
                "null"
            } else {
                "not null"
            };
            vec![col, ty, null.to_string()]
        })
        .collect();
    print_table(
        &["column".into(), "type".into(), String::new()],
        &table_rows,
    );
    Ok(0)
}

async fn db_count(table: &str, json: bool) -> CliResult {
    table_columns(table).await?; // validates the name before it is quoted in
    let sql = format!("SELECT count(*) FROM {}", quote_ident(table));
    let rows = db::query(&sql, &[]).await?;
    let n: i64 = rows.first().map(|r| r.get(0)).unwrap_or(0);
    if json {
        println!("{}", json!({ "table": table, "count": n }));
    } else {
        println!("{n}");
    }
    Ok(0)
}

async fn db_rows(table: &str, limit: i64, filter: Option<&str>, json: bool) -> CliResult {
    // Table and column names cannot be bound parameters, so they are validated
    // against the catalogue first and only names that are really there are quoted
    // in. Every *value* — the projected cells and the `--where` operand — is a
    // bound parameter or a `::text` cast, never spliced into the statement.
    let names = table_columns(table).await?;
    let select_list = names
        .iter()
        .map(|c| format!("{}::text", quote_ident(c)))
        .collect::<Vec<_>>()
        .join(", ");

    let mut sql = format!("SELECT {select_list} FROM {}", quote_ident(table));
    let mut params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = Vec::new();
    let where_value: String;
    if let Some(f) = filter {
        let (col, val) = f
            .split_once('=')
            .ok_or_else(|| format!("`--where {f}` is not `col=value`"))?;
        if !names.iter().any(|n| n == col) {
            return Err(format!("no column `{col}` on `{table}`").into());
        }
        where_value = val.to_string();
        sql.push_str(&format!(" WHERE {}::text = $1", quote_ident(col)));
        params.push(&where_value);
    }
    sql.push_str(&format!(" LIMIT ${}", params.len() + 1));
    params.push(&limit);

    let rows = db::query(&sql, &params).await?;
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            (0..names.len())
                .map(|i| {
                    row.get::<_, Option<String>>(i)
                        .unwrap_or_else(|| "NULL".into())
                })
                .collect()
        })
        .collect();

    if json {
        let objs: Vec<_> = cells
            .iter()
            .map(|r| {
                let m: serde_json::Map<String, serde_json::Value> = names
                    .iter()
                    .zip(r)
                    .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                    .collect();
                serde_json::Value::Object(m)
            })
            .collect();
        println!(
            "{}",
            json!({ "table": table, "columns": names, "rows": objs, "count": cells.len() })
        );
        return Ok(0);
    }

    if cells.is_empty() {
        println!("(0 rows)");
        return Ok(0);
    }
    print_table(&names, &cells);
    println!(
        "({} row{})",
        cells.len(),
        if cells.len() == 1 { "" } else { "s" }
    );
    Ok(0)
}

/// The column names of a table, in declaration order; errors if the table does
/// not exist (which is also what validates the name before it is quoted in).
async fn table_columns(table: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let cols = db::query(
        "SELECT column_name FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name = $1 \
         ORDER BY ordinal_position",
        &[&table],
    )
    .await?;
    if cols.is_empty() {
        return Err(format!("no table `{table}` in the `public` schema").into());
    }
    Ok(cols.iter().map(|r| r.get::<_, String>(0)).collect())
}

/// Print a left-aligned table: each column as wide as its widest cell (header
/// included), two spaces between columns. An all-empty header row is omitted.
fn print_table(headers: &[String], rows: &[Vec<String>]) {
    let ncols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(ncols) {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let line = |cells: &[String]| {
        cells
            .iter()
            .enumerate()
            .take(ncols)
            .map(|(i, c)| format!("{c:<width$}", width = widths[i]))
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string()
    };
    if headers.iter().any(|h| !h.is_empty()) {
        println!("  {}", line(headers));
    }
    for row in rows {
        println!("  {}", line(row));
    }
}

/// Quote a PostgreSQL identifier. Reached only with a name already verified to be
/// a real table or column, so the doubling of `"` is belt-and-braces.
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}
