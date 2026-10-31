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

use std::fmt::Write as _;
use std::process::{Command, ExitCode};

use clap::{CommandFactory, Parser, Subcommand};
use dorm_core::db;
use dorm_core::row::Row;
use dorm_core::value::Value;
use serde_json::json;

mod scaffold;
mod studio;

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

    /// Answer "yes" to the confirmation on a destructive command, without
    /// prompting. For scripts and `--yes`-style automation; the destructive
    /// command still runs on the write path (a read-only session refuses it).
    #[arg(short = 'y', long, global = true)]
    yes: bool,

    /// Run without ever prompting. Equivalent to `--yes` for the confirmation on
    /// a destructive command: it proceeds without asking. A piped/scripted run
    /// (no terminal) never blocks on a prompt regardless; this makes the intent
    /// explicit and proceeds rather than refusing.
    #[arg(long, global = true)]
    non_interactive: bool,

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
    /// Compile `dorm.schema` to Rust models (`src/models.rs`). Re-run after
    /// editing the schema. Native — no project build, no database.
    Generate {
        /// The schema file to read.
        #[arg(long, default_value = "dorm.schema")]
        schema: String,
        /// Where to write the generated Rust.
        #[arg(long, default_value = "src/models.rs")]
        out: String,
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
    /// Browse and edit the live database in your browser — DORM's Studio, in the
    /// spirit of Prisma Studio. With no subcommand it serves the native Studio
    /// (reads `information_schema`; no project build). `dorm studio generate`
    /// writes a typed Studio (`src/bin/studio.rs`) over your models instead.
    /// Read-only unless `--write`.
    Studio {
        #[command(subcommand)]
        action: Option<StudioAction>,
        /// TCP port to serve on. Binds `127.0.0.1` only. Defaults to 5555.
        #[arg(long, default_value_t = 5555)]
        port: u16,
        /// Allow writes — create, edit and delete rows. Off by default, mirroring
        /// the shell; without it the Studio browses but refuses every edit.
        #[arg(long)]
        write: bool,
    },
    /// Generate and serve a read API (Axum + OpenAPI) over your models.
    Api {
        #[command(subcommand)]
        action: ApiAction,
    },
    /// Print a shell completion script to stdout (bash, zsh, fish, …).
    Completions {
        /// The shell to generate completions for.
        shell: clap_complete::Shell,
    },
}

#[derive(Subcommand)]
enum ApiAction {
    /// Generate `src/bin/api.rs` from `dorm.schema` — an Axum read API with an
    /// OpenAPI document. Native; re-run after changing the schema.
    Generate {
        /// The schema file to read.
        #[arg(long, default_value = "dorm.schema")]
        schema: String,
        /// Where to write the generated API binary.
        #[arg(long, default_value = "src/bin/api.rs")]
        out: String,
    },
    /// Build and run the generated API. (`cargo run --bin api`.)
    Serve,
}

#[derive(Subcommand)]
enum StudioAction {
    /// Write `src/bin/studio.rs` — a typed Studio over your models, run with
    /// `cargo run --bin studio`. Native; `dorm init` already writes it, so this
    /// is for regenerating it.
    Generate {
        /// Where to write the Studio binary.
        #[arg(long, default_value = "src/bin/studio.rs")]
        out: String,
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
    /// Author the next migration: diff your models against the recorded history
    /// in `migrations/` and write the new migration file. Like `prisma migrate
    /// dev`/Django `makemigrations`. (`cargo run --bin makemigration`.)
    Make {
        /// A short name for the migration, e.g. `add_body`.
        #[arg(default_value = "migration")]
        name: String,
    },
    /// Apply the recorded migration chain in `migrations/` (or the initial
    /// schema when there is none yet). The default when `migrate` is given no
    /// subcommand. Like `prisma migrate deploy`. (`cargo run --bin migrate`.)
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
    /// Introspect a live PostgreSQL schema into a `dorm.schema` (the reverse of
    /// `dorm generate`). Approximate — review the result.
    Pull {
        /// Where to write the schema.
        #[arg(long, default_value = "dorm.schema")]
        out: String,
    },
    /// Run explicit SQL over the ADR 0005 raw-SQL valve. The statement text is
    /// yours; every `--param` value is bound as `$1`, `$2`, … in the order given
    /// and is never spliced into the SQL.
    ///
    /// Read-only by default. Without `--write` the statement must begin with a
    /// read keyword (SELECT, WITH, VALUES, TABLE, SHOW, EXPLAIN); anything else is
    /// refused before the database is touched, so a forgotten `--write` cannot
    /// mutate. With `--write` the statement runs on the write path and reports the
    /// rows affected — and is still refused by a read-only session with the same
    /// `Error::ReadOnly` every compiled write gets.
    ///
    /// Parameters bind as text. PostgreSQL uses a text parameter directly wherever
    /// it is compared against text (`WHERE name = $1`) or a column is cast to text
    /// for the comparison (`WHERE id::text = $1`). For a non-text target the
    /// parameter must be routed through text explicitly — `$1::text::int`,
    /// `$1::text::uuid`, `VALUES ($1::text::int, $2)`; a bare `$1` into a non-text
    /// column is rejected by the server. This is the honest limit of binding every
    /// value as text, named rather than hidden.
    ///
    /// Only the closed set of column types the ORM knows is rendered — bool,
    /// int2/int4/int8, text/varchar/char, numeric, timestamp(tz), uuid, json(b);
    /// a column of any other type asks to be projected as text (`col::text`).
    ///
    /// The SQL is yours to own: DORM does not parse it and never quotes into it. A
    /// string that reached this command from an untrusted source is your injection
    /// to own, exactly as in hand-written Rust (ADR 0005).
    Execute {
        /// The SQL statement to run. A single statement; its text is used as-is.
        sql: String,
        /// A value to bind, repeatable: the first `--param` is `$1`, the second
        /// `$2`, and so on. Bound as text — route it through text for a non-text
        /// column (`$1::text::int`) or compare the column as text (`id::text = $1`).
        #[arg(short = 'p', long = "param", value_name = "VALUE")]
        params: Vec<String>,
        /// Run a write (INSERT/UPDATE/DELETE/DDL). Required for any statement that
        /// is not a plain read; honoured against the read-only guard.
        #[arg(long)]
        write: bool,
    },
    /// Open the native PostgreSQL client (`psql`) on the configured database.
    /// Hands you straight to `psql` with the DSN; everything after is `psql`.
    Shell,
    /// Run a SQL seed file against the database (every statement in it). The
    /// file is yours — plain `INSERT`s, or any setup you want applied.
    Seed {
        /// The SQL file to run. Defaults to `seeds.sql`.
        #[arg(long, default_value = "seeds.sql")]
        file: String,
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
    // `--yes`/`--non-interactive` both mean "do not prompt me; proceed". They
    // reach only the destructive commands, which pass this to `confirm`.
    let assume_yes = cli.yes || cli.non_interactive;

    let result: CliResult = match cli.command {
        CliCommand::Init { name } => scaffold::init(name.as_deref()).map(|()| 0),
        CliCommand::New {
            what: NewKind::Model { name },
        } => scaffold::new_model(&name).map(|()| 0),
        CliCommand::Generate { schema, out } => scaffold::generate(&schema, &out).map(|()| 0),
        CliCommand::Validate => cargo_run("validate", json),
        CliCommand::Doctor => on_runtime(doctor(json)),
        CliCommand::Migrate { action } => match action.unwrap_or(MigrateAction::Deploy) {
            MigrateAction::Make { name } => cargo_run_args("makemigration", &[&name], false),
            MigrateAction::Deploy => cargo_run("migrate", false),
            MigrateAction::Status => on_runtime(migrate_status(json)),
            MigrateAction::Diff => cargo_run("diff", json),
        },
        CliCommand::Push => cargo_run("sync", false),
        CliCommand::Shell => cargo_run("shell", false),
        CliCommand::Db { action } => on_runtime(run_db(action, json, assume_yes)),
        CliCommand::Studio {
            action,
            port,
            write,
        } => match action {
            None => on_runtime(studio::serve(port, write)),
            Some(StudioAction::Generate { out }) => scaffold::studio_generate(&out).map(|()| 0),
        },
        CliCommand::Api { action } => match action {
            ApiAction::Generate { schema, out } => {
                scaffold::api_generate(&schema, &out).map(|()| 0)
            }
            ApiAction::Serve => cargo_run("api", false),
        },
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
    cargo_run_args(bin, &[], json)
}

/// Build and run a project binary, forwarding `args` to it after `--`. Used by
/// `migrate make`, which passes the migration name to the `makemigration` bin.
fn cargo_run_args(bin: &str, args: &[&str], json: bool) -> CliResult {
    let mut cmd = Command::new("cargo");
    cmd.arg("run").arg("--quiet").arg("--bin").arg(bin);
    if json || !args.is_empty() {
        cmd.arg("--");
        cmd.args(args);
        if json {
            cmd.arg("--json");
        }
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

async fn run_db(action: DbAction, json: bool, assume_yes: bool) -> CliResult {
    match action {
        DbAction::Tables => db_tables(json).await,
        DbAction::Show { table } => db_show(&table, json).await,
        DbAction::Count { table } => db_count(&table, json).await,
        DbAction::Rows {
            table,
            limit,
            filter,
        } => db_rows(&table, limit, filter.as_deref(), json).await,
        DbAction::Pull { out } => db_pull(&out).await,
        DbAction::Execute { sql, params, write } => {
            db_execute(&sql, &params, write, json, assume_yes).await
        }
        DbAction::Shell => db_shell(),
        DbAction::Seed { file } => db_seed(&file, json, assume_yes).await,
    }
}

/// Whether the confirmation can be settled without an interactive prompt — true
/// when `--yes`/`--non-interactive` was passed, or stdin is not a terminal (a
/// piped or scripted run we must never block waiting for an answer). When this is
/// true, [`confirm`] does not prompt; it proceeds iff the user asked to (`--yes`),
/// so a scripted run never hangs.
///
/// Kept pure (no I/O) so its logic is unit-tested directly.
fn settled_without_prompt(assume_yes: bool, stdin_is_tty: bool) -> bool {
    assume_yes || !stdin_is_tty
}

/// The confirmation decision, factored out of all I/O so it can be tested.
/// `read_line` is called only when an interactive prompt is warranted (a terminal
/// and no `--yes`), so a non-tty run never reads and never blocks.
///
/// * `--yes`/`--non-interactive` → proceed (`true`), no read.
/// * no terminal, no `--yes` → do not proceed (`false`): a destructive action
///   nobody consented to is refused rather than run silently — and without
///   blocking, since `read_line` is not called.
/// * a terminal, no `--yes` → read the answer; `y`/`yes` (any case) proceeds.
fn confirm_decision(
    assume_yes: bool,
    stdin_is_tty: bool,
    read_line: impl FnOnce() -> std::io::Result<String>,
) -> std::io::Result<bool> {
    if settled_without_prompt(assume_yes, stdin_is_tty) {
        return Ok(assume_yes);
    }
    let answer = read_line()?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Ask the user to confirm a destructive action, returning whether to proceed.
///
/// On a terminal it prints `{question} [y/N] ` to stderr (so `--json` on stdout
/// stays clean) and reads one line. With `--yes`/`--non-interactive` it proceeds
/// without asking; with neither and no terminal (piped/scripted) it refuses
/// without blocking — the honest "never run a destructive statement nobody could
/// confirm" default. See [`confirm_decision`] for the full truth table.
fn confirm(question: &str, assume_yes: bool) -> std::io::Result<bool> {
    use std::io::{IsTerminal, Write};
    let stdin_is_tty = std::io::stdin().is_terminal();
    confirm_decision(assume_yes, stdin_is_tty, || {
        let mut err = std::io::stderr();
        write!(err, "{question} [y/N] ")?;
        err.flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        Ok(line)
    })
}

/// The database a destructive command will touch, redacted, for the confirmation
/// message. Falls back to a neutral phrase when no DSN is configured (the command
/// itself will then fail with the real "no database configured" error).
fn confirm_target() -> String {
    db::dsn().map_or_else(|_| "the database".to_string(), |d| redact(&d))
}

/// `dorm db shell`: open the native PostgreSQL client on the configured database.
/// We resolve the DSN and hand it to `psql` as a conninfo argument — it accepts
/// both the libpq keyword form and the URL form — then inherit its streams so the
/// session is `psql`'s. The exit code is `psql`'s.
fn db_shell() -> CliResult {
    let dsn = db::dsn()?;
    let status = Command::new("psql").arg(&dsn).status().map_err(|e| {
        format!("could not run `psql`: {e} (is PostgreSQL's client installed and on PATH?)")
    })?;
    Ok(status.code().unwrap_or(1))
}

/// `dorm db seed`: run every statement in a SQL file against the database. The
/// file is applied as one batch (so it may contain multiple statements, a
/// function body, a transaction). Writes — it is a seed — so it runs on the write
/// path; a read-only session still refuses it.
async fn db_seed(file: &str, json: bool, assume_yes: bool) -> CliResult {
    let sql = std::fs::read_to_string(file)
        .map_err(|e| format!("reading {file}: {e} (create it, or pass --file)"))?;
    // A seed file is arbitrary SQL applied as one batch — genuinely destructive
    // (it can DROP/TRUNCATE/overwrite). Confirm before the write path. `--yes`
    // proceeds; a non-interactive run with neither flag refuses without blocking.
    if !confirm(
        &format!(
            "This will run the seed file `{file}` (arbitrary writes) against {target}. Continue?",
            target = confirm_target()
        ),
        assume_yes,
    )? {
        eprintln!("aborted; nothing was run (pass --yes to proceed non-interactively).");
        return Ok(1);
    }
    db::guard_write("seed")?;
    db::run_ddl(&sql).await?;
    if json {
        println!("{}", json!({ "seeded": file }));
    } else {
        println!("seeded from {file}");
    }
    Ok(0)
}

/// `dorm db execute`: the CLI surface over the ADR 0005 raw-SQL valve. Reads run
/// through `db::sql` and print a table; writes run through `db::sql_execute`,
/// which calls `guard_write` first — so a read-only session refuses them — and
/// print the affected-row count. Every `--param` is a bound `$n`; the SQL text is
/// never parsed, and no value is ever formatted into it.
async fn db_execute(
    sql: &str,
    params: &[String],
    write: bool,
    json: bool,
    assume_yes: bool,
) -> CliResult {
    // Every user value binds as a text parameter. This is the whole point of the
    // valve: the statement is opaque and the values travel as `$1`, `$2`, …,
    // never spliced into the SQL. A text parameter is used directly where it meets
    // text (a text column, or `col::text = $1`); for a non-text target the caller
    // routes it through text (`$1::text::int`) — the limit the help names.
    let bound: Vec<Value> = params.iter().map(|p| Value::Text(p.clone())).collect();

    if write {
        // Guard the clearly irreversible statements — DROP, TRUNCATE, and an
        // unqualified DELETE (no WHERE) — behind a confirmation. `--yes` proceeds;
        // a non-interactive run with neither flag refuses without blocking. A
        // targeted DELETE/UPDATE/INSERT is not prompted: the gate is for the
        // bulk-irreversible slip, not every write.
        if let Some(kind) = destructive_kind(sql) {
            if !confirm(
                &format!(
                    "This will run a destructive statement ({kind}) against {target}. Continue?",
                    target = confirm_target()
                ),
                assume_yes,
            )? {
                eprintln!("aborted; nothing was run (pass --yes to proceed non-interactively).");
                return Ok(1);
            }
        }
        // The write path. `sql_execute` runs `guard_write` first, so a read-only
        // session refuses this with `Error::ReadOnly`, exactly like a compiled
        // write — the escape hatch does not escape the read-only rule.
        let affected = db::sql_execute(sql, &bound).await?;
        if json {
            println!("{}", json!({ "affected": affected }));
        } else {
            println!(
                "{affected} row{} affected",
                if affected == 1 { "" } else { "s" }
            );
        }
        return Ok(0);
    }

    // The read path. Refuse an obvious write *before* touching the database: the
    // read path (`db::sql`) runs a `DELETE` just as readily as a `SELECT`, so a
    // forgotten `--write` must be caught here, not discovered after it mutated.
    // The check is lexical — see `looks_like_read` for what it does and does not
    // promise.
    if !looks_like_read(sql) {
        return Err(format!(
            "`{}` is not a read; re-run with --write to execute a write statement",
            leading_keyword(sql)
        )
        .into());
    }

    let rows = db::sql(sql, &bound).await?;
    if rows.is_empty() {
        // With no rows there is no PostgreSQL metadata to name the columns from,
        // so an empty read reports an empty result rather than inventing a header.
        if json {
            println!("{}", json!({ "columns": [], "rows": [], "count": 0 }));
        } else {
            println!("(0 rows)");
        }
        return Ok(0);
    }

    // Column names come from the PostgreSQL row itself. Only PostgreSQL carries
    // this on the row; the other `dorm db` commands are PostgreSQL-only too (they
    // read `information_schema`), so rendering PostgreSQL alone is consistent.
    let names: Vec<String> = as_pg(&rows[0])?
        .columns()
        .iter()
        .map(|c| c.name().to_string())
        .collect();

    let mut cells: Vec<Vec<String>> = Vec::with_capacity(rows.len());
    for row in &rows {
        let mut line = Vec::with_capacity(names.len());
        for idx in 0..names.len() {
            line.push(render_pg_cell(row, idx)?);
        }
        cells.push(line);
    }

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
            json!({ "columns": names, "rows": objs, "count": cells.len() })
        );
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

/// The leading SQL keyword, upper-cased — the first run of ASCII letters after any
/// leading whitespace or `(`. Used only to classify a statement for the `--write`
/// gate and to name it in the refusal message.
fn leading_keyword(sql: &str) -> String {
    sql.trim_start()
        .trim_start_matches('(')
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase()
}

/// Whether a statement reads, by its leading keyword alone: SELECT, WITH, VALUES,
/// TABLE, SHOW and EXPLAIN are reads; everything else is treated as a write and
/// needs `--write`.
///
/// This is a lexical convenience, not a parser, and it is honest about its limit.
/// A write smuggled behind a read-shaped keyword — `WITH x AS (DELETE …) SELECT …`
/// or `EXPLAIN ANALYZE DELETE …` — reads as a "read" here and would run on the
/// read path. Per ADR 0005 the SQL text is the caller's to own; this guard catches
/// the everyday `dorm db execute "DELETE …"` slip, it is not a safety boundary.
fn looks_like_read(sql: &str) -> bool {
    matches!(
        leading_keyword(sql).as_str(),
        "SELECT" | "WITH" | "VALUES" | "TABLE" | "SHOW" | "EXPLAIN"
    )
}

/// Classify a write statement as irreversible-at-a-stroke, for the destructive
/// confirmation — `Some("DROP")`, `Some("TRUNCATE")`, or `Some("DELETE without
/// WHERE")` for a `DELETE` with no `WHERE` clause; `None` for a targeted write
/// (an `INSERT`, `UPDATE`, or a `DELETE … WHERE …`) that is not prompted.
///
/// Lexical by the same honesty as [`looks_like_read`]: it reads the leading
/// keyword, and for `DELETE` scans for a `WHERE` token. It can miss a destructive
/// write hidden behind another keyword (`WITH … AS (DELETE …)`), and a `WHERE` in
/// a string literal would make an unqualified `DELETE` look qualified — it errs
/// toward *not* over-prompting a targeted write, never toward silently skipping a
/// bare `DROP`/`TRUNCATE`. It is a fat-finger guard, not a safety boundary.
fn destructive_kind(sql: &str) -> Option<&'static str> {
    match leading_keyword(sql).as_str() {
        "DROP" => Some("DROP"),
        "TRUNCATE" => Some("TRUNCATE"),
        "DELETE" if !has_where_token(sql) => Some("DELETE without WHERE"),
        _ => None,
    }
}

/// Whether the statement has a bare `WHERE` keyword token (case-insensitive),
/// split on non-alphanumeric boundaries so `nowhere`/`somewhere` do not match.
fn has_where_token(sql: &str) -> bool {
    sql.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|w| w.eq_ignore_ascii_case("where"))
}

/// Render one PostgreSQL cell as a string, over the closed set of column types the
/// ORM knows (the `Value` enum): bool, int2/int4/int8, text/varchar/char, numeric,
/// timestamp(tz), uuid and json(b). A NULL in any of them is `NULL`.
///
/// The decode is the ORM's own (`value::read_column`), so a cell reads back exactly
/// as the typed API would — except `int2`, which `read_column` does not cover (it
/// reads i32/i64) and which is read here directly through `i16`'s built-in decode.
/// A column of any other type is an error that asks the caller to project it as
/// text (`col::text`): honest about the set the ORM actually supports, and never a
/// silent wrong answer.
/// The PostgreSQL row behind a [`Row`], or a clear error for a SQLite one. The
/// `match` stays exhaustive whichever backends are compiled in: with the `sqlite`
/// feature off `Row` has only the one variant, and the cfg'd arm is removed — so
/// no irrefutable-pattern warning, and the code is ready if SQLite is ever linked.
fn as_pg(row: &Row) -> Result<&tokio_postgres::Row, Box<dyn std::error::Error>> {
    match row {
        Row::Pg(pg) => Ok(pg),
        #[cfg(feature = "sqlite")]
        _ => Err("`dorm db execute` renders PostgreSQL results only".into()),
    }
}

fn render_pg_cell(full: &Row, idx: usize) -> Result<String, Box<dyn std::error::Error>> {
    use dorm_core::value::FieldType;
    use tokio_postgres::types::Type;

    let pg = as_pg(full)?;
    let col = &pg.columns()[idx];
    let name = col.name();
    let ty = col.type_();

    // `int2` is the one integer width `read_column` does not decode. `i16`'s
    // `FromSql` is a tokio-postgres built-in, so read it straight.
    if *ty == Type::INT2 {
        let v: Option<i16> = pg.try_get(idx)?;
        return Ok(v.map_or_else(|| "NULL".to_string(), |n| n.to_string()));
    }

    let field = match *ty {
        Type::BOOL => FieldType::Bool,
        Type::INT4 => FieldType::I32,
        Type::INT8 => FieldType::I64,
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME => FieldType::Text,
        Type::NUMERIC => FieldType::Decimal,
        Type::TIMESTAMP | Type::TIMESTAMPTZ => FieldType::Timestamp,
        Type::UUID => FieldType::Uuid,
        Type::JSON | Type::JSONB => FieldType::Json,
        _ => {
            // Outside the ORM's set. Text is the escape a SELECT can always take,
            // and the error names how: `col::text`.
            return match pg.try_get::<_, Option<String>>(idx) {
                Ok(v) => Ok(v.unwrap_or_else(|| "NULL".to_string())),
                Err(_) => Err(format!(
                    "column `{name}` has SQL type `{ty}`, which `dorm db execute` does not \
                     render; project it as text in your SQL, e.g. `{name}::text`"
                )
                .into()),
            };
        }
    };

    let value = dorm_core::value::read_column(full, idx, field, name)?;
    Ok(if value.is_null() {
        "NULL".to_string()
    } else {
        value.to_string()
    })
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

/// `dorm db pull`: introspect the live PostgreSQL `public` schema into a
/// `dorm.schema`. Approximate by design — it recovers tables, columns, types,
/// primary keys (serial ones become `@id @default(autoincrement())`), nullability
/// and foreign keys; it does not yet recover unique/index/length/check/ordering,
/// and relation `onDelete` is a guess. The header says so.
async fn db_pull(out: &str) -> CliResult {
    let tables: Vec<String> = db::query(
        "SELECT table_name FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_type = 'BASE TABLE' \
         AND table_name <> 'orm_schema_migrations' ORDER BY table_name",
        &[],
    )
    .await?
    .iter()
    .map(|r| r.get::<_, String>(0))
    .collect();

    let mut doc = String::from(
        "// Pulled by `dorm db pull` — approximate; review before use.\n\
         // Not recovered: unique/index, column lengths, checks, ordering, and\n\
         // relation onDelete (defaulted). Re-run `dorm generate` after editing.\n\n\
         datasource db {\n  provider = \"postgresql\"\n  url      = env(\"DELONIX_ORM_DSN\")\n}\n",
    );

    for table in &tables {
        // Primary-key columns.
        let pk: std::collections::HashSet<String> = db::query(
            "SELECT kcu.column_name FROM information_schema.table_constraints tc \
             JOIN information_schema.key_column_usage kcu ON tc.constraint_name = kcu.constraint_name \
             AND tc.table_schema = kcu.table_schema \
             WHERE tc.table_schema='public' AND tc.table_name=$1 AND tc.constraint_type='PRIMARY KEY'",
            &[table],
        )
        .await?
        .iter()
        .map(|r| r.get::<_, String>(0))
        .collect();

        // Foreign keys: column → referenced table.
        let mut fks: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for r in db::query(
            "SELECT kcu.column_name, ccu.table_name FROM information_schema.table_constraints tc \
             JOIN information_schema.key_column_usage kcu ON tc.constraint_name = kcu.constraint_name \
             JOIN information_schema.constraint_column_usage ccu ON ccu.constraint_name = tc.constraint_name \
             WHERE tc.table_schema='public' AND tc.table_name=$1 AND tc.constraint_type='FOREIGN KEY'",
            &[table],
        )
        .await?
        {
            fks.insert(r.get::<_, String>(0), r.get::<_, String>(1));
        }

        let cols = db::query(
            "SELECT column_name, data_type, is_nullable, column_default \
             FROM information_schema.columns WHERE table_schema='public' AND table_name=$1 \
             ORDER BY ordinal_position",
            &[table],
        )
        .await?;

        let _ = write!(doc, "\nmodel {} {{\n", singularize_pascal(table));
        for r in &cols {
            let name: String = r.get(0);
            let data_type: String = r.get(1);
            let nullable = r.get::<_, String>(2) == "YES";
            let default: Option<String> = r.get(3);
            let null = if nullable { "?" } else { "" };

            if let Some(target) = fks.get(&name) {
                // A relation: this column holds the foreign key.
                let _ = writeln!(
                    doc,
                    "  {name} {}{null} @relation(onDelete: Cascade)",
                    singularize_pascal(target)
                );
                continue;
            }

            let serial = default
                .as_deref()
                .is_some_and(|d| d.starts_with("nextval("));
            let is_pk = pk.contains(&name);
            if is_pk && serial {
                let _ = writeln!(doc, "  {name} Int @id @default(autoincrement())");
            } else {
                let mut attrs = String::new();
                if is_pk {
                    attrs.push_str(" @id");
                }
                if default
                    .as_deref()
                    .is_some_and(|d| d.contains("now()") || d.contains("CURRENT_TIMESTAMP"))
                {
                    attrs.push_str(" @default(now())");
                }
                let _ = writeln!(
                    doc,
                    "  {name} {}{null}{attrs}",
                    pg_to_schema_type(&data_type)
                );
            }
        }
        let _ = writeln!(doc, "  @@map(\"{table}\")\n}}");
    }

    std::fs::write(out, &doc).map_err(|e| format!("writing {out}: {e}"))?;
    println!("pulled {} table(s) into {out}.", tables.len());
    Ok(0)
}

/// A PostgreSQL `data_type` as the nearest `dorm.schema` scalar.
fn pg_to_schema_type(data_type: &str) -> &'static str {
    match data_type {
        "bigint" => "BigInt",
        "integer" | "smallint" => "Int",
        "boolean" => "Boolean",
        "numeric" => "Decimal",
        "uuid" => "Uuid",
        "json" | "jsonb" => "Json",
        d if d.starts_with("timestamp") => "DateTime",
        _ => "String", // text, character varying, and anything unmapped
    }
}

/// A table name as a model type name: a best-effort singular, PascalCased. The
/// `@@map` keeps the real table name, so an imperfect singular is cosmetic.
fn singularize_pascal(table: &str) -> String {
    let singular = if let Some(stem) = table.strip_suffix("ies") {
        format!("{stem}y")
    } else if table.ends_with("ses")
        || table.ends_with("xes")
        || table.ends_with("zes")
        || table.ends_with("ches")
        || table.ends_with("shes")
    {
        table[..table.len() - 2].to_string()
    } else if let Some(stem) = table.strip_suffix('s') {
        stem.to_string()
    } else {
        table.to_string()
    };
    singular
        .split('_')
        .filter(|s| !s.is_empty())
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
                None => String::new(),
            }
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settled_without_prompt_truth_table() {
        // `--yes` (or `--non-interactive`, which maps to the same flag) → settled.
        assert!(settled_without_prompt(true, true));
        assert!(settled_without_prompt(true, false));
        // No flag but a terminal → must prompt (not settled).
        assert!(!settled_without_prompt(false, true));
        // No flag and no terminal (piped/scripted) → settled; never block on a tty.
        assert!(settled_without_prompt(false, false));
    }

    #[test]
    fn confirm_decision_yes_flag_proceeds_without_reading() {
        // `--yes`: proceed, and the reader must never be consulted.
        let r =
            confirm_decision(true, true, || panic!("must not prompt when --yes is set")).unwrap();
        assert!(r);
    }

    #[test]
    fn confirm_decision_non_interactive_proceeds() {
        // `--non-interactive` reaches here as `assume_yes = true`; proceeds, no read.
        let r = confirm_decision(true, false, || {
            panic!("must not prompt when non-interactive")
        })
        .unwrap();
        assert!(r);
    }

    #[test]
    fn confirm_decision_non_tty_without_yes_refuses_without_blocking() {
        // A mocked non-tty with no `--yes`: do not proceed, and never read stdin
        // (so a scripted run cannot block). This is the "refuse" default.
        let r =
            confirm_decision(false, false, || panic!("must not read stdin on a non-tty")).unwrap();
        assert!(!r);
    }

    #[test]
    fn confirm_decision_tty_reads_the_answer() {
        // A terminal with no `--yes`: the answer decides, case/whitespace-insensitive.
        assert!(confirm_decision(false, true, || Ok("y\n".into())).unwrap());
        assert!(confirm_decision(false, true, || Ok("  YES ".into())).unwrap());
        assert!(!confirm_decision(false, true, || Ok("n\n".into())).unwrap());
        assert!(!confirm_decision(false, true, || Ok("\n".into())).unwrap()); // default No
        assert!(!confirm_decision(false, true, || Ok(String::new())).unwrap()); // EOF → No
    }

    #[test]
    fn destructive_kind_flags_the_irreversible_only() {
        assert_eq!(destructive_kind("DROP TABLE users"), Some("DROP"));
        assert_eq!(destructive_kind("  truncate posts"), Some("TRUNCATE"));
        assert_eq!(
            destructive_kind("DELETE FROM users"),
            Some("DELETE without WHERE")
        );
        // A targeted write is not prompted.
        assert_eq!(destructive_kind("DELETE FROM users WHERE id = $1"), None);
        assert_eq!(destructive_kind("delete from users where id=1"), None);
        assert_eq!(destructive_kind("UPDATE users SET name = $1"), None);
        assert_eq!(destructive_kind("INSERT INTO users VALUES ($1)"), None);
        // `WHERE` as a substring of an identifier must not count as a clause.
        assert_eq!(
            destructive_kind("DELETE FROM nowhere_logs"),
            Some("DELETE without WHERE")
        );
    }
}
