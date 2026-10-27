//! `dorm` — the DORM project manager, in the spirit of `prisma`.
//!
//! DORM is a **compile-time** ORM: your models are Rust structs, registered when
//! they are linked into *your* binary. So this installed tool cannot read an
//! arbitrary project's models the way `prisma` reads `schema.prisma`. It works on
//! the two things it *can* reach without your compiled code:
//!
//! * **the files** — `dorm init` scaffolds a ready-to-run project; and
//! * **the database** — `dorm db …` and `dorm migrate status` talk to a live
//!   PostgreSQL over `DELONIX_ORM_DSN` (or `--dsn`), introspecting and browsing
//!   it, Prisma-Studio-style.
//!
//! The two commands that *do* need your typed models — `dorm migrate` and
//! `dorm shell` — run them the only way that is honest: by building and running
//! your own project through `cargo`. The project `dorm init` scaffolds answers
//! both.

use std::process::{Command, ExitCode};

use clap::{Parser, Subcommand};
use dorm_core::db;

mod scaffold;

#[derive(Parser)]
#[command(
    name = "dorm",
    version,
    about = "Scaffold a DORM project and manage its PostgreSQL — migrations, schema, and a row browser."
)]
struct Cli {
    /// The database to talk to, as a libpq DSN or URL. Overrides
    /// `DELONIX_ORM_DSN` for this run. Only the live-database commands use it.
    #[arg(long, global = true)]
    dsn: Option<String>,

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
    /// Apply your project's migrations by building and running it (`cargo run`),
    /// or `dorm migrate status` to read the applied-versions ledger from the database.
    Migrate {
        #[command(subcommand)]
        action: Option<MigrateAction>,
    },
    /// Open your project's REPL by building and running it (`cargo run`).
    Shell,
    /// Inspect and browse the live database (read-only).
    Db {
        #[command(subcommand)]
        action: DbAction,
    },
}

#[derive(Subcommand)]
enum MigrateAction {
    /// Show which migration versions the database has recorded as applied.
    Status,
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
    /// Print rows from a table (read-only), to a limit.
    Rows {
        /// The table name.
        table: String,
        /// How many rows at most.
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: i64,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Some(dsn) = &cli.dsn {
        // The live-database commands read the DSN from the environment, via
        // `dorm_core::db`. Setting it here lets `--dsn` win for this process.
        std::env::set_var(db::DSN_ENV, dsn);
    }

    let result = match cli.command {
        CliCommand::Init { name } => scaffold::init(name.as_deref()),
        CliCommand::Migrate { action: None } => cargo_run(&["--bin", "migrate"]),
        CliCommand::Migrate {
            action: Some(MigrateAction::Status),
        } => on_runtime(migrate_status()),
        CliCommand::Shell => cargo_run(&["--bin", "shell"]),
        CliCommand::Db { action } => on_runtime(run_db(action)),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

type CliResult = Result<(), Box<dyn std::error::Error>>;

/// Run a closure on a fresh multi-threaded runtime — one for the whole command,
/// as `dorm_core::db` requires (the pool belongs to the runtime that built it).
fn on_runtime(fut: impl std::future::Future<Output = CliResult>) -> CliResult {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(fut)
}

/// The cargo lane: build and run the project in the current directory. Used for
/// the commands that need the project's own compiled models.
fn cargo_run(extra: &[&str]) -> CliResult {
    let mut cmd = Command::new("cargo");
    cmd.arg("run").arg("--quiet").args(extra);
    let status = cmd.status().map_err(|e| {
        format!(
            "could not run `cargo` (is it on your PATH, and are you inside a DORM project?): {e}"
        )
    })?;
    if !status.success() {
        return Err(format!("`cargo run {}` failed", extra.join(" ")).into());
    }
    Ok(())
}

/// `migrate status`: the applied-versions ledger `dorm-migrations` keeps.
async fn migrate_status() -> CliResult {
    // The ledger is `orm_schema_migrations (version, name, applied_unix)`. If it
    // does not exist yet, nothing has been migrated — say so rather than error.
    let rows = match db::query(
        "SELECT version, name, applied_unix FROM orm_schema_migrations ORDER BY version",
        &[],
    )
    .await
    {
        Ok(rows) => rows,
        Err(_) => {
            println!(
                "no migrations recorded (the ledger `orm_schema_migrations` does not exist yet)."
            );
            return Ok(());
        }
    };
    if rows.is_empty() {
        println!("the ledger is empty — no migrations applied.");
        return Ok(());
    }
    println!("applied migrations:");
    for row in &rows {
        let version: i64 = row.get(0);
        let name: String = row.get(1);
        let applied_unix: i64 = row.get(2);
        println!("  v{version:<4} {name:<24} (applied_unix={applied_unix})");
    }
    Ok(())
}

async fn run_db(action: DbAction) -> CliResult {
    match action {
        DbAction::Tables => db_tables().await,
        DbAction::Show { table } => db_show(&table).await,
        DbAction::Rows { table, limit } => db_rows(&table, limit).await,
    }
}

async fn db_tables() -> CliResult {
    let rows = db::query(
        "SELECT table_name FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_type = 'BASE TABLE' \
         ORDER BY table_name",
        &[],
    )
    .await?;
    if rows.is_empty() {
        println!("(no tables in the `public` schema)");
        return Ok(());
    }
    for row in &rows {
        let name: String = row.get(0);
        println!("  {name}");
    }
    Ok(())
}

async fn db_show(table: &str) -> CliResult {
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
    println!("{table}");
    for row in &rows {
        let col: String = row.get(0);
        let ty: String = row.get(1);
        let nullable: String = row.get(2);
        let null = if nullable == "YES" { "  null" } else { "" };
        println!("  {col:<20} {ty}{null}");
    }
    Ok(())
}

async fn db_rows(table: &str, limit: i64) -> CliResult {
    // The table name cannot be a bound parameter, so it is validated against the
    // catalogue first and only a name that is really a table is quoted into the
    // statement. Every *value* still comes back as a bound-free `::text` cast.
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
    let names: Vec<String> = cols.iter().map(|r| r.get::<_, String>(0)).collect();
    let select_list = names
        .iter()
        .map(|c| format!("{}::text", quote_ident(c)))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("SELECT {select_list} FROM {} LIMIT $1", quote_ident(table));
    let rows = db::query(&sql, &[&limit]).await?;
    if rows.is_empty() {
        println!("(0 rows)");
        return Ok(());
    }
    for row in &rows {
        let cells: Vec<String> = names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let v: Option<String> = row.get(i);
                format!("{name}={}", v.as_deref().unwrap_or("NULL"))
            })
            .collect();
        println!("  {}", cells.join("  "));
    }
    println!(
        "({} row{})",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" }
    );
    Ok(())
}

/// Quote a PostgreSQL identifier. Reached only with a name already verified to be
/// a real table or column, so the doubling of `"` is belt-and-braces.
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}
