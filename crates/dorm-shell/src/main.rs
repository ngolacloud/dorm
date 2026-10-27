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
//! * `:help`   — the command summary.
//! * `:quit`   — leave (so does end-of-input).
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

use dorm_core::compiler::Plan;
use dorm_core::expr::Q;
use dorm_core::meta::ModelMeta;
use dorm_core::model::Model;
use dorm_core::queryset::fetch_value_rows;
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

    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    loop {
        prompt();
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
            _ => {
                if let Err(e) = rt.block_on(run_query(line)) {
                    eprintln!("error: {e}");
                }
            }
        }
    }
    println!("bye.");
}

fn prompt() {
    print!("dorm> ");
    let _ = io::stdout().flush();
}

fn print_help() {
    println!(
        "commands:\n\
         \x20 :models                      list models, columns and relations\n\
         \x20 :help                        this summary\n\
         \x20 :quit                        leave\n\
         \x20 <Model> [k=v ...] [limit=N]  run a query, e.g.\n\
         \x20     Tenant\n\
         \x20     Tenant slug__startswith=ac limit=5\n\
         \x20     Application name__icontains=api\n\
         \x20     Tenant apps__name__icontains=api   (reverse relation)\n\
         each k=v is one filter_str; a value parses as an integer or a boolean if\n\
         it looks like one, otherwise as text."
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

/// Parse one query line, compile it against the named model, run it live, and
/// print the rows.
async fn run_query(line: &str) -> dorm_core::Result<()> {
    let mut tokens = line.split_whitespace();
    let model_name = tokens
        .next()
        .ok_or_else(|| dorm_core::Error::Config("empty query".into()))?;

    // Resolving the model here is what turns a typo into a clean error instead of
    // a SQL failure — and it is the only place a user string becomes a table.
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

    let rows = fetch_value_rows(&plan).await?;
    if rows.is_empty() {
        println!("(0 rows)");
        return Ok(());
    }
    for row in &rows {
        let cells: Vec<String> = row.iter().map(|(k, v)| format!("{k}={v}")).collect();
        println!("  {}", cells.join("  "));
    }
    println!(
        "({} row{})",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" }
    );
    Ok(())
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
