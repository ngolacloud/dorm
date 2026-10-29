//! `dorm` — the ORM's dynamic front end, a line-oriented REPL.
//!
//! Everything the typed API does through generics, this does through strings the
//! user types: it enumerates the models the linker gathered, and it runs a query
//! whose model, filter keys and lookup suffixes were all unknown at compile time.
//! The safety is unchanged — every identifier is resolved against the registry
//! before any SQL exists, and every value is a bound parameter — which is the
//! whole point of routing the two front ends through one compiler.
//!
//! ## Commands
//!
//! * `:models` — list every registered model, its columns, and its relations.
//! * `:begin` / `:commit` / `:rollback` — run several writes as one transaction:
//!   `:begin`, then any number of `insert`/`update`/`delete` (and reads, which
//!   see the uncommitted work), then `:commit` to persist all atomically or
//!   `:rollback` to discard them. An error mid-transaction aborts it, so the
//!   following `:rollback` leaves nothing written. See the note on why this is
//!   held in the REPL loop rather than through `dorm_core::db::atomic`.
//! * `:help`   — the command summary.
//! * `:quit`   — leave (so does end-of-input; an open transaction is rolled back).
//! * `<Model> [field__lookup=value ...] [limit=N]` — run a query and print the
//!   rows. `Tenant slug__startswith=ac limit=5`, or `Tenant` for everything (to
//!   a default limit). Each `key=value` is one `filter_str`.
//!
//! ## Why one runtime for the whole session
//!
//! The ORM's connection pool is owned by the runtime that first built it; drop
//! that runtime and every pooled connection dies with it (see `dorm_core::db`).
//! So the REPL builds exactly one runtime and `block_on`s each command against
//! it, rather than spinning one up per line.

use std::io::{self, BufRead, Write};

use dorm_core::compiler::{self, Plan, Sql};
use dorm_core::expr::{Assignment, F, Q};
use dorm_core::meta::ModelMeta;
use dorm_core::model::Model;
use dorm_core::queryset::{
    delete_value_rows, fetch_value_rows, insert_value_rows, update_value_rows, ValueRow,
};
use dorm_core::value::Value;
use dorm_core::{db, registry};

/// Rows printed when the query does not say `limit=`. A dynamic `SELECT` with no
/// bound is a foot-gun against a real table; the ceiling is lifted the moment the
/// caller asks for one.
const DEFAULT_LIMIT: i64 = 20;

/// Touch each model's metadata so the linker keeps the crate that registers it.
///
/// `dorm-models` is a dependency whose items are otherwise never named here, and
/// a dependency with no used symbol can be dropped entirely — taking its
/// `inventory::submit!` registrations with it, so `registry::models()` would come
/// back empty. Naming each `meta()` is the reference that prevents that.
fn link_models() {
    use dorm_models::{Application, Deployment, Plan as PlanModel, Product, Tenant};
    let _linked: &[&'static ModelMeta] = &[
        <Tenant as Model>::meta(),
        <PlanModel as Model>::meta(),
        <Application as Model>::meta(),
        <Deployment as Model>::meta(),
        <Product as Model>::meta(),
    ];
}

fn main() {
    link_models();

    // Read-only unless `--write` is passed. The flag mirrors the roadmap's
    // "read-only profile by default in production": a shell that can run DML is
    // opt-in, so a careless paste into the default session cannot write. The
    // refusal is the same one the typed API gives — `db::guard_write`.
    let writable = std::env::args().any(|a| a == "--write");
    db::set_read_only(!writable);

    // One runtime, kept alive for the session — see the module note.
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("could not start the async runtime: {e}");
            std::process::exit(1);
        }
    };

    println!("dorm — the Delonix ORM shell. `:help` for commands, `:quit` to leave.");
    println!(
        "mode: {}",
        if writable {
            "read-write (--write)"
        } else {
            "read-only — start with `--write` to run insert/update/delete"
        }
    );
    match db::dsn() {
        Ok(_) => println!(
            "connected target: {}",
            redact(&db::dsn().unwrap_or_default())
        ),
        Err(_) => println!(
            "no {} set — `:models` works offline; a query needs a database.",
            db::DSN_ENV
        ),
    }

    // The transaction in scope, if `:begin` opened one. It is held here —
    // outside any `block_on` — because the task-local that routes statements onto
    // a transaction only lives for one future, and every REPL line is its own
    // `block_on`; the handle survives between them, and each line is wrapped in
    // its `scope` so the ordinary statements find it (see `dorm_core::db`).
    let mut open_tx: Option<db::InteractiveTransaction> = None;

    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    loop {
        prompt(open_tx.is_some());
        let Some(line) = lines.next() else { break };
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("input error: {e}");
                break;
            }
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match line {
            ":quit" | ":q" | ":exit" => break,
            ":help" | ":h" => print_help(),
            ":models" | ":m" => print_models(),
            ":begin" | ":begin;" => {
                if open_tx.is_some() {
                    eprintln!("error: already in a transaction; :commit or :rollback first");
                    continue;
                }
                match rt.block_on(db::begin_interactive()) {
                    Ok(tx) => {
                        open_tx = Some(tx);
                        println!("BEGIN");
                    }
                    Err(e) => eprintln!("error: {e}"),
                }
            }
            ":commit" => match open_tx.take() {
                None => eprintln!("error: no transaction open; :begin first"),
                Some(tx) => match rt.block_on(tx.commit()) {
                    Ok(()) => println!("COMMIT"),
                    Err(e) => eprintln!("error: {e}"),
                },
            },
            ":rollback" => match open_tx.take() {
                None => eprintln!("error: no transaction open; :begin first"),
                Some(tx) => match rt.block_on(tx.rollback()) {
                    Ok(()) => println!("ROLLBACK"),
                    Err(e) => eprintln!("error: {e}"),
                },
            },
            _ => {
                // Inside a transaction, the line runs with its connection in
                // scope, so the write lands on the transaction and a failure
                // leaves it open for the caller to `:rollback` — PostgreSQL has
                // already aborted it, and the rollback is what discards the work.
                let result = match &open_tx {
                    Some(tx) => rt.block_on(tx.scope(dispatch(line))),
                    None => rt.block_on(dispatch(line)),
                };
                if let Err(e) = result {
                    eprintln!("error: {e}");
                }
            }
        }
    }
    // A transaction still open at end-of-input is rolled back, not committed:
    // dropping the handle closes PostgreSQL's connection (the server rolls back)
    // or issues SQLite's `ROLLBACK`. Explicit, so the intent is on the page.
    if open_tx.take().is_some() {
        println!("(rolling back the open transaction)");
    }
    println!("bye.");
}

/// The prompt, marked `*` while a transaction is open so the session's state is
/// visible on every line.
fn prompt(in_tx: bool) {
    print!("dorm{}> ", if in_tx { "*" } else { "" });
    let _ = io::stdout().flush();
}

fn print_help() {
    println!(
        "commands:\n\
         \x20 :models                           list models, columns and relations\n\
         \x20 :sql <command>                    show a command's SQL and params, without running it\n\
         \x20 :raw <sql>                        run raw SQL (ADR 0005); writes need --write\n\
         \x20 :begin / :commit / :rollback      run several writes as one transaction\n\
         \x20 :help                             this summary\n\
         \x20 :quit                             leave\n\
         reads:\n\
         \x20 <Model> [k=v ...] [limit=N]       Tenant slug__startswith=ac limit=5\n\
         \x20                                   Tenant apps__name__icontains=api   (reverse relation)\n\
         writes (need --write):\n\
         \x20 insert <Model> col=v ...          insert one row, printed back with generated columns\n\
         \x20 update <Model> [k=v ...] set c=v  update the selected rows\n\
         \x20 delete <Model> k=v ...            delete the selected rows (`delete <Model> all` to mean no filter)\n\
         each k=v is one filter; a value parses as an integer or a boolean if it\n\
         looks like one, otherwise as text. Every value is a bound parameter."
    );
}

/// List every model the registry holds, with its columns and both directions of
/// its relations — all read back from metadata, nothing hard-coded.
fn print_models() {
    let mut any = false;
    for m in registry::models() {
        any = true;
        println!("\n{} (table \"{}\")", m.name, m.table);
        for f in m.fields {
            let mut tags = Vec::new();
            if f.primary_key {
                tags.push("pk".to_string());
            }
            if f.unique {
                tags.push("unique".to_string());
            }
            if f.nullable {
                tags.push("null".to_string());
            }
            if let Some(r) = f.relation {
                tags.push(format!("→ {}", r.model));
            }
            let suffix = if tags.is_empty() {
                String::new()
            } else {
                format!("  [{}]", tags.join(", "))
            };
            println!("  {:<14} {}{}", f.name, f.ty.describe(), suffix);
        }
        // Reverse relations: a `related_name` some other model points back with.
        // `Tenant` shows `apps ←Application.tenant`, which is exactly the path
        // `apps__…` filters travel.
        let mut reverses = Vec::new();
        for other in registry::models() {
            for f in other.fields {
                if let Some(r) = f.relation {
                    if r.model == m.name {
                        if let Some(name) = r.related_name {
                            reverses.push(format!("{name} ←{}.{}", other.name, f.name));
                        }
                    }
                }
            }
        }
        if !reverses.is_empty() {
            println!("  reverse: {}", reverses.join(", "));
        }
    }
    if !any {
        println!("no models are registered.");
    }
}

/// Route one line to a query, a DML verb, `:sql`, or `:raw`. The verb is the
/// first word; everything the typed API does through generics, this does through
/// the same compiler, model-erased — so a write is as safe as a read.
async fn dispatch(line: &str) -> dorm_core::Result<()> {
    if let Some(rest) = line.strip_prefix(":sql") {
        return preview_sql(rest.trim());
    }
    if let Some(rest) = line.strip_prefix(":raw") {
        return run_raw(rest.trim()).await;
    }
    match line
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "insert" => run_insert(line).await,
        "update" => run_update(line).await,
        "delete" => run_delete(line).await,
        _ => run_query(line).await,
    }
}

/// Parse a query line into its model and plan — shared by `run_query` and
/// `:sql`. Resolving the model here is the only place a user string becomes a
/// table, and a typo is a clean error, not a SQL failure.
fn query_plan(line: &str) -> dorm_core::Result<(&'static ModelMeta, Plan)> {
    let mut tokens = line.split_whitespace();
    let model_name = tokens
        .next()
        .ok_or_else(|| dorm_core::Error::Config("empty query".into()))?;
    let meta = registry::model(model_name)?;
    let mut plan = Plan::new(meta);
    let mut limit = Some(DEFAULT_LIMIT);
    for tok in tokens {
        let Some((key, raw)) = tok.split_once('=') else {
            return Err(dorm_core::Error::Config(format!(
                "`{tok}` is not a `key=value`; a filter is `field__lookup=value`"
            )));
        };
        if key == "limit" {
            limit = Some(raw.parse().map_err(|_| {
                dorm_core::Error::Config(format!("limit `{raw}` is not a whole number"))
            })?);
            continue;
        }
        plan.filters.push(Q::new(key.to_string(), parse_value(raw)));
    }
    plan.limit = limit;
    Ok((meta, plan))
}

/// `<Model> [field__lookup=value ...] [limit=N]` — run a read and print the rows.
async fn run_query(line: &str) -> dorm_core::Result<()> {
    let (_, plan) = query_plan(line)?;
    let rows = fetch_value_rows(&plan).await?;
    print_rows(&rows);
    Ok(())
}

/// `insert <Model> col=value ...` — insert one row, printing it back with its
/// database-generated columns (the serial key, `auto_now_add` defaults).
async fn run_insert(line: &str) -> dorm_core::Result<()> {
    let (meta, values) = insert_parts(line)?;
    let rows = insert_value_rows(meta, &[values]).await?;
    for row in &rows {
        print_one(row);
    }
    println!("(inserted {})", count(rows.len()));
    Ok(())
}

/// `update <Model> [filter ...] set col=value ...` — update the selected rows.
async fn run_update(line: &str) -> dorm_core::Result<()> {
    let (_, plan, assignments) = update_parts(line)?;
    let n = update_value_rows(&plan, &assignments).await?;
    println!("(updated {})", count(n as usize));
    Ok(())
}

/// `delete <Model> filter ...` — delete the selected rows. An unfiltered delete
/// is refused unless `all` is given, so a table is never wiped by a slip.
async fn run_delete(line: &str) -> dorm_core::Result<()> {
    let (_, plan) = delete_parts(line)?;
    let n = delete_value_rows(&plan).await?;
    println!("(deleted {})", count(n as usize));
    Ok(())
}

/// `:raw <sql>` — the ADR 0005 escape hatch. A leading `select`/`with`/… reads
/// and reports the row count; anything else is a write and reports rows
/// affected, still refused in a read-only session. Parameters are not parsed
/// here — a raw statement owns its own SQL text.
async fn run_raw(sql: &str) -> dorm_core::Result<()> {
    if sql.is_empty() {
        return Err(dorm_core::Error::Config(
            "`:raw <sql>` needs a statement".into(),
        ));
    }
    let head = sql
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let reads = matches!(
        head.as_str(),
        "select" | "with" | "show" | "explain" | "table" | "values"
    );
    if reads {
        let rows = db::sql(sql, &[]).await?;
        println!(
            "({} — use a typed query or Manager::raw to see the columns)",
            count(rows.len())
        );
    } else {
        let n = db::sql_execute(sql, &[]).await?;
        println!("({} affected)", count(n as usize));
    }
    Ok(())
}

/// `:sql <command>` — compile a query or DML line and print its SQL and bound
/// parameters, without running it. The inspection the roadmap's shell promises.
fn preview_sql(inner: &str) -> dorm_core::Result<()> {
    if inner.is_empty() {
        return Err(dorm_core::Error::Config(
            "`:sql <command>` needs a command to show".into(),
        ));
    }
    let sql = compile_line(inner)?;
    println!("{}", sql.text);
    if !sql.params.is_empty() {
        let shown: Vec<String> = sql
            .params
            .iter()
            .enumerate()
            .map(|(i, v)| format!("${}={v}", i + 1))
            .collect();
        println!("params: {}", shown.join("  "));
    }
    Ok(())
}

/// Compile a line to `Sql` without running it, dispatching on the same verb as
/// [`dispatch`]. The single place `:sql` and the runners agree on what SQL a
/// command means.
fn compile_line(line: &str) -> dorm_core::Result<Sql> {
    let dialect = db::dialect();
    match line
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "insert" => {
            let (meta, values) = insert_parts(line)?;
            compiler::compile_insert_on(meta, &[values], dialect)
        }
        "update" => {
            let (_, plan, assignments) = update_parts(line)?;
            compiler::compile_update_on(&plan, &assignments, dialect)
        }
        "delete" => {
            let (_, plan) = delete_parts(line)?;
            compiler::compile_delete_on(&plan, dialect)
        }
        _ => {
            let (_, plan) = query_plan(line)?;
            compiler::compile_select_on(&plan, dialect)
        }
    }
}

/// `insert <Model> col=value ...` → the model and its column values.
fn insert_parts(line: &str) -> dorm_core::Result<(&'static ModelMeta, Vec<(String, Value)>)> {
    let mut tokens = line.split_whitespace();
    tokens.next(); // the `insert` verb
    let model_name = tokens.next().ok_or_else(|| {
        dorm_core::Error::Config("insert needs a model: insert <Model> col=value ...".into())
    })?;
    let meta = registry::model(model_name)?;
    let mut values = Vec::new();
    for tok in tokens {
        let (col, raw) = tok
            .split_once('=')
            .ok_or_else(|| dorm_core::Error::Config(format!("`{tok}` is not a `col=value`")))?;
        values.push((col.to_string(), parse_value(raw)));
    }
    if values.is_empty() {
        return Err(dorm_core::Error::Config(
            "insert needs at least one col=value".into(),
        ));
    }
    Ok((meta, values))
}

/// `update <Model> [filter ...] set col=value ...` → the plan and assignments.
/// The `set` keyword splits the filters from the columns to write.
fn update_parts(line: &str) -> dorm_core::Result<(&'static ModelMeta, Plan, Vec<Assignment>)> {
    let mut tokens = line.split_whitespace();
    tokens.next(); // the `update` verb
    let model_name = tokens.next().ok_or_else(|| {
        dorm_core::Error::Config(
            "update needs a model: update <Model> [filter ...] set col=value ...".into(),
        )
    })?;
    let meta = registry::model(model_name)?;
    let mut plan = Plan::new(meta);
    let mut assignments = Vec::new();
    let mut in_set = false;
    for tok in tokens {
        if tok.eq_ignore_ascii_case("set") {
            in_set = true;
            continue;
        }
        let (key, raw) = tok
            .split_once('=')
            .ok_or_else(|| dorm_core::Error::Config(format!("`{tok}` is not a `key=value`")))?;
        if in_set {
            assignments.push(Assignment::new(key.to_string(), F::lit(parse_value(raw))));
        } else {
            plan.filters.push(Q::new(key.to_string(), parse_value(raw)));
        }
    }
    if !in_set || assignments.is_empty() {
        return Err(dorm_core::Error::Config(
            "update needs a `set`: update <Model> [filter ...] set col=value ...".into(),
        ));
    }
    Ok((meta, plan, assignments))
}

/// `delete <Model> filter ...` (or `delete <Model> all`) → the plan. An
/// unfiltered delete without `all` is refused, not run.
fn delete_parts(line: &str) -> dorm_core::Result<(&'static ModelMeta, Plan)> {
    let mut tokens = line.split_whitespace();
    tokens.next(); // the `delete` verb
    let model_name = tokens.next().ok_or_else(|| {
        dorm_core::Error::Config("delete needs a model: delete <Model> filter ...".into())
    })?;
    let meta = registry::model(model_name)?;
    let mut plan = Plan::new(meta);
    let (mut filtered, mut all) = (false, false);
    for tok in tokens {
        if tok.eq_ignore_ascii_case("all") {
            all = true;
            continue;
        }
        let (key, raw) = tok
            .split_once('=')
            .ok_or_else(|| dorm_core::Error::Config(format!("`{tok}` is not a `key=value`")))?;
        plan.filters.push(Q::new(key.to_string(), parse_value(raw)));
        filtered = true;
    }
    if !filtered && !all {
        return Err(dorm_core::Error::Config(
            "refusing an unfiltered delete; add a filter, or `delete <Model> all` to mean it"
                .into(),
        ));
    }
    Ok((meta, plan))
}

/// Print a list of rows and the count line, the shape a read prints.
fn print_rows(rows: &[ValueRow]) {
    if rows.is_empty() {
        println!("(0 rows)");
        return;
    }
    for row in rows {
        print_one(row);
    }
    println!("({})", count(rows.len()));
}

/// Print one row as `col=value  col=value`.
fn print_one(row: &ValueRow) {
    let cells: Vec<String> = row.iter().map(|(k, v)| format!("{k}={v}")).collect();
    println!("  {}", cells.join("  "));
}

/// `N row` / `N rows`, so the callers do not each re-spell the plural.
fn count(n: usize) -> String {
    format!("{n} row{}", if n == 1 { "" } else { "s" })
}

/// Turn a raw filter value into a typed one. An integer or a boolean is taken at
/// face value so `replicas__gte=2` and `name__isnull=true` work; everything else
/// is text, which the compiler then coerces to the column's declared type.
fn parse_value(raw: &str) -> Value {
    if let Ok(n) = raw.parse::<i64>() {
        return Value::I64(n);
    }
    match raw {
        "true" | "True" => Value::Bool(true),
        "false" | "False" => Value::Bool(false),
        _ => Value::Text(raw.to_string()),
    }
}

/// Hide the password in a DSN before echoing it. A libpq DSN carries the secret
/// as `password=…`; a URL carries it as `user:pass@`.
fn redact(dsn: &str) -> String {
    dsn.split_whitespace()
        .map(|part| {
            if let Some(rest) = part.strip_prefix("password=") {
                let _ = rest;
                "password=***".to_string()
            } else {
                part.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
