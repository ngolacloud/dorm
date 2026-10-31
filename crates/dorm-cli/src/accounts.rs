//! Application-account administration — `createsuperuser`, `changepassword` and
//! `user list|create|delete` (ADR 0009).
//!
//! These are **native** commands: they talk to the configured PostgreSQL
//! directly over `db::sql` / `db::sql_execute`, needing no compiled project
//! models. The account rows live in a conventional table, [`ACCOUNTS_TABLE`],
//! created on demand (`CREATE TABLE IF NOT EXISTS`) exactly like the migration
//! ledger — so the first `createsuperuser` on a fresh database just works.
//!
//! ## The two invariants
//!
//! * **Every value is a bound parameter.** The only identifiers that reach SQL
//!   are the `&'static` column names written in this file; every username,
//!   email and hash travels as `$1`, `$2`, … and is never formatted into the
//!   statement. This is the ADR 0005 rule, kept here too.
//! * **Passwords are never stored in the clear and never logged.** A password is
//!   read without echo ([`rpassword`]), hashed with Argon2id into a
//!   self-describing PHC string (`$argon2id$v=19$m=…`), and only the hash is
//!   written. The plaintext lives in one local variable for the length of the
//!   hash call. The roadmap's "never writes incompatible hashes" is met by the
//!   PHC format: the algorithm, version and parameters travel inside the stored
//!   value, so any Argon2 verifier reads it back.

use std::io::{BufRead, IsTerminal, Write};

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHasher, SaltString};
use argon2::Argon2;
use dorm_core::db;
use dorm_core::row::Row;
use dorm_core::value::Value;
use serde_json::json;

use crate::CliResult;

/// The conventional accounts table. A single-word, `orm_`-prefixed name keeps it
/// clear of a project's own tables, matching the migration ledger's convention.
pub(crate) const ACCOUNTS_TABLE: &str = "orm_accounts";

/// Stand the accounts table up if it is not there yet — idempotent, like the
/// ledger. Called at the start of every account command so a fresh database
/// needs no separate migration step.
///
/// The shape: a surrogate `bigserial` key; `username` unique and not null (the
/// login identifier; uniqueness is DB-enforced, not checked in Rust); an
/// optional `email`; the Argon2 PHC `password_hash`, never null; `is_superuser`
/// and `is_active` flags; and `created_at` / `updated_at` timestamps the server
/// fills with `now()`.
async fn ensure_table() -> Result<(), Box<dyn std::error::Error>> {
    db::run_ddl(
        "CREATE TABLE IF NOT EXISTS orm_accounts (\
             id            bigserial PRIMARY KEY, \
             username      text NOT NULL UNIQUE, \
             email         text, \
             password_hash text NOT NULL, \
             is_superuser  boolean NOT NULL DEFAULT false, \
             is_active     boolean NOT NULL DEFAULT true, \
             created_at    timestamptz NOT NULL DEFAULT now(), \
             updated_at    timestamptz NOT NULL DEFAULT now()\
         )",
    )
    .await?;
    Ok(())
}

/// Hash a password into an Argon2id PHC string with a fresh random salt.
///
/// The output is self-describing — `$argon2id$v=19$m=19456,t=2,p=1$<salt>$<hash>`
/// — so the stored value carries everything a verifier needs. Defaults are the
/// `argon2` crate's recommended Argon2id parameters.
fn hash_password(password: &str) -> Result<String, Box<dyn std::error::Error>> {
    let salt = SaltString::generate(&mut OsRng);
    let phc = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| format!("hashing password: {e}"))?
        .to_string();
    Ok(phc)
}

/// How a password reaches the command, in order of preference:
///
/// 1. `--password <PW>` — scriptable, but visible to `ps`/shell history; the help
///    says so.
/// 2. `--password-stdin` — read one line from stdin (Django's non-interactive
///    form); the trailing newline is stripped. Best for pipelines and tests.
/// 3. an interactive no-echo prompt via [`rpassword`], confirmed twice on create.
///
/// An empty password is refused. When no source is given and stdin is not a
/// terminal, the error names the three ways rather than hanging on a prompt that
/// cannot be answered.
fn resolve_password(
    explicit: Option<&str>,
    stdin: bool,
    confirm: bool,
) -> Result<String, Box<dyn std::error::Error>> {
    let password = if let Some(p) = explicit {
        p.to_string()
    } else if stdin {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        line.trim_end_matches(['\n', '\r']).to_string()
    } else {
        if !std::io::stdin().is_terminal() {
            return Err("no password supplied and stdin is not a terminal; pass \
                        --password, pipe one with --password-stdin, or run interactively"
                .into());
        }
        let first = rpassword::prompt_password("Password: ")?;
        if confirm {
            let again = rpassword::prompt_password("Password (again): ")?;
            if first != again {
                return Err("the two passwords did not match".into());
            }
        }
        first
    };
    if password.is_empty() {
        return Err("the password must not be empty".into());
    }
    Ok(password)
}

/// Turn a PostgreSQL unique-violation on the username into a human message; pass
/// any other error through unchanged.
fn friendly_insert_error(
    username: &str,
    e: Box<dyn std::error::Error>,
) -> Box<dyn std::error::Error> {
    let text = e.to_string();
    if text.contains("orm_accounts_username_key") || text.contains("duplicate key") {
        return format!("a user named `{username}` already exists").into();
    }
    e
}

/// `dorm createsuperuser` — create an administrator (`is_superuser = true`).
pub(crate) async fn createsuperuser(
    username: Option<String>,
    email: Option<String>,
    password: Option<String>,
    password_stdin: bool,
    json_out: bool,
) -> CliResult {
    create_account(username, email, password, password_stdin, true, json_out).await
}

/// `dorm user create` — create an account. A plain user unless `superuser`.
pub(crate) async fn user_create(
    username: Option<String>,
    email: Option<String>,
    password: Option<String>,
    password_stdin: bool,
    superuser: bool,
    json_out: bool,
) -> CliResult {
    create_account(
        username,
        email,
        password,
        password_stdin,
        superuser,
        json_out,
    )
    .await
}

/// The shared create path behind `createsuperuser` and `user create`. Prompts for
/// the username interactively when it was not given; hashes the password; inserts
/// one row and reports the generated id.
async fn create_account(
    username: Option<String>,
    email: Option<String>,
    password: Option<String>,
    password_stdin: bool,
    superuser: bool,
    json_out: bool,
) -> CliResult {
    db::guard_write("createsuperuser")?;
    ensure_table().await?;

    let username = match username {
        Some(u) => u,
        None => prompt_line("Username: ")?,
    };
    if username.is_empty() {
        return Err("the username must not be empty".into());
    }
    let email = email.filter(|e| !e.is_empty());
    let password = resolve_password(password.as_deref(), password_stdin, true)?;
    let hash = hash_password(&password)?;

    // Only the column list and placeholders are chosen here — both `&'static`.
    // Every value is bound. Email is omitted (left NULL) when absent rather than
    // bound as an empty string, since `Value` has no NULL variant.
    let (columns, placeholders, params): (&str, &str, Vec<Value>) = if let Some(email) = email {
        (
            "username, email, password_hash, is_superuser",
            "$1, $2, $3, $4",
            vec![
                Value::Text(username.clone()),
                Value::Text(email),
                Value::Text(hash),
                Value::Bool(superuser),
            ],
        )
    } else {
        (
            "username, password_hash, is_superuser",
            "$1, $2, $3",
            vec![
                Value::Text(username.clone()),
                Value::Text(hash),
                Value::Bool(superuser),
            ],
        )
    };
    let sql =
        format!("INSERT INTO {ACCOUNTS_TABLE} ({columns}) VALUES ({placeholders}) RETURNING id");

    let rows = db::sql(&sql, &params)
        .await
        .map_err(|e| friendly_insert_error(&username, e.into()))?;
    let id = rows.first().and_then(row_i64).unwrap_or_default();

    let role = if superuser { "superuser" } else { "user" };
    if json_out {
        println!(
            "{}",
            json!({ "id": id, "username": username, "is_superuser": superuser })
        );
    } else {
        println!("created {role} `{username}` (id {id})");
    }
    Ok(0)
}

/// `dorm changepassword <username>` — replace a user's password hash.
pub(crate) async fn changepassword(
    username: String,
    password: Option<String>,
    password_stdin: bool,
    json_out: bool,
) -> CliResult {
    db::guard_write("changepassword")?;
    ensure_table().await?;

    let password = resolve_password(password.as_deref(), password_stdin, true)?;
    let hash = hash_password(&password)?;

    let affected = db::sql_execute(
        &format!("UPDATE {ACCOUNTS_TABLE} SET password_hash = $1, updated_at = now() WHERE username = $2"),
        &[Value::Text(hash), Value::Text(username.clone())],
    )
    .await?;

    if affected == 0 {
        return Err(format!("no user named `{username}`").into());
    }
    if json_out {
        println!(
            "{}",
            json!({ "username": username, "password_changed": true })
        );
    } else {
        println!("password changed for `{username}`");
    }
    Ok(0)
}

/// `dorm user delete <username>` — remove an account.
///
/// In a terminal, confirms first unless `yes`; when stdin is not a terminal it
/// requires `--yes` rather than deleting on an unanswerable prompt.
pub(crate) async fn user_delete(username: String, yes: bool, json_out: bool) -> CliResult {
    db::guard_write("user delete")?;
    ensure_table().await?;

    if !yes {
        if !std::io::stdin().is_terminal() {
            return Err("refusing to delete without confirmation; pass --yes".into());
        }
        let answer = prompt_line(&format!("Delete user `{username}`? [y/N]: "))?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("cancelled");
            return Ok(0);
        }
    }

    let affected = db::sql_execute(
        &format!("DELETE FROM {ACCOUNTS_TABLE} WHERE username = $1"),
        &[Value::Text(username.clone())],
    )
    .await?;

    if affected == 0 {
        return Err(format!("no user named `{username}`").into());
    }
    if json_out {
        println!("{}", json!({ "username": username, "deleted": true }));
    } else {
        println!("deleted `{username}`");
    }
    Ok(0)
}

/// `dorm user list` — the accounts, newest-id last. A read: honours `--json`, and
/// never prints a hash.
pub(crate) async fn user_list(json_out: bool) -> CliResult {
    ensure_table().await?;

    let columns = [
        "id",
        "username",
        "email",
        "is_superuser",
        "is_active",
        "created_at",
    ];
    let rows = db::sql(
        &format!(
            "SELECT {} FROM {ACCOUNTS_TABLE} ORDER BY id",
            columns.join(", ")
        ),
        &[],
    )
    .await?;

    let headers: Vec<String> = columns.iter().map(|c| c.to_string()).collect();
    let mut cells: Vec<Vec<String>> = Vec::with_capacity(rows.len());
    for row in &rows {
        let mut line = Vec::with_capacity(columns.len());
        for idx in 0..columns.len() {
            line.push(crate::render_pg_cell(row, idx)?);
        }
        cells.push(line);
    }

    if json_out {
        let objs: Vec<_> = cells
            .iter()
            .map(|r| {
                let m: serde_json::Map<String, serde_json::Value> = headers
                    .iter()
                    .zip(r)
                    .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                    .collect();
                serde_json::Value::Object(m)
            })
            .collect();
        println!("{}", json!({ "users": objs, "count": cells.len() }));
        return Ok(0);
    }

    if cells.is_empty() {
        println!("(no users)");
        return Ok(0);
    }
    crate::print_table(&headers, &cells);
    println!(
        "({} user{})",
        cells.len(),
        if cells.len() == 1 { "" } else { "s" }
    );
    Ok(0)
}

/// Read the first column of a row as an `i64` (the `bigserial` id), if it is one.
fn row_i64(row: &Row) -> Option<i64> {
    match row {
        Row::Pg(pg) => pg.try_get::<_, i64>(0).ok(),
        #[cfg(feature = "sqlite")]
        _ => None,
    }
}

/// Prompt on stderr and read one line from stdin, trimmed of its newline. Used
/// for the username and the delete confirmation — never for a password (that is
/// [`resolve_password`]'s no-echo path).
fn prompt_line(prompt: &str) -> Result<String, Box<dyn std::error::Error>> {
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}
