//! `dorm studio` — a browser UI over the live database, in the spirit of Prisma
//! Studio, with the DML the roadmap's Studio promises: browse, filter and page
//! every table, and (with `--write`) create, edit and delete rows.
//!
//! ## Native, like `dorm db`
//!
//! DORM is a compile-time ORM, so the installed `dorm` binary has no project's
//! models linked. The Studio therefore reads the shape of the database from the
//! live catalogue — `information_schema` and the primary-/foreign-key
//! constraints — exactly as `dorm db` does, so it works against any PostgreSQL
//! without building the project. The cost is that it knows only what the
//! catalogue exposes (columns, types, keys), not the richer model metadata.
//!
//! ## Safety
//!
//! * **Read-only by default**, like the shell: writes are enabled only with
//!   `--write`. Every write goes through [`db::guard_write`]/[`db::sql_execute`],
//!   so a read-only session refuses an edit by name (HTTP 403), never silently.
//! * **Every identifier is validated** against the catalogue before it is quoted
//!   into a statement; **every value is a bound parameter** (`$n`), cast into its
//!   column's type — never formatted into the SQL. The one thing interpolated is
//!   a parsed integer `LIMIT`/`OFFSET`, which cannot carry an injection.
//! * **Loopback only**: the listener binds `127.0.0.1`, so the Studio is not
//!   reachable off the machine it runs on.
//! * An update or delete with no key filter is refused, so the grid cannot wipe
//!   a table by a slip.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;

use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use dorm_core::db;
use dorm_core::value::Value;
use serde_json::{json, Value as JsonValue};
use tokio_postgres::types::ToSql;

/// Start the Studio server and run it until Ctrl-C.
pub async fn serve(port: u16, writable: bool) -> crate::CliResult {
    // PostgreSQL only, like every `dorm db` lane — the introspection reads
    // `information_schema`. A SQLite DSN gets a clear refusal, not a driver error.
    let dsn = db::dsn()?;
    if db::backend_of(&dsn) != db::Backend::Postgres {
        return Err(
            "`dorm studio` supports PostgreSQL only (the DSN selects another backend)".into(),
        );
    }

    // The read-only gate: off unless `--write`. Mirrors the shell exactly — the
    // refusal is `db::guard_write`, surfaced to the browser as a 403.
    db::set_read_only(!writable);

    // Fail fast with a friendly message if the database is unreachable, rather
    // than letting the first browser request discover it.
    db::query("SELECT 1", &[])
        .await
        .map_err(|e| format!("cannot reach the database ({}): {e}", redact(&dsn)))?;

    let app = Router::new()
        .route("/", get(index))
        .route("/api/schema", get(schema))
        .route(
            "/api/tables/:table/rows",
            get(get_rows)
                .post(insert_row)
                .patch(update_row)
                .delete(delete_row),
        );

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("cannot bind {addr}: {e}"))?;

    let mode = if writable {
        "read-write (--write)"
    } else {
        "read-only — pass --write to create/edit/delete rows"
    };
    println!("DORM Studio — {mode}");
    println!("  open  http://{addr}");
    println!("  stop  Ctrl-C");

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|e| format!("server error: {e}"))?;

    println!("\nbye.");
    Ok(0)
}

// --------------------------------------------------------------- introspection

/// One column's shape, as the catalogue reports it.
struct ColMeta {
    name: String,
    data_type: String,
    nullable: bool,
    default: Option<String>,
    pk: bool,
    /// The `(table, column)` a foreign key points at, so the UI can jump to the
    /// referenced row. `None` when the column is not a foreign key.
    references: Option<(String, String)>,
}

/// The base tables of the `public` schema, minus the migration ledger — the
/// tables worth browsing.
async fn list_tables() -> dorm_core::Result<Vec<String>> {
    Ok(db::query(
        "SELECT table_name FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_type = 'BASE TABLE' \
         AND table_name <> 'orm_schema_migrations' ORDER BY table_name",
        &[],
    )
    .await?
    .iter()
    .map(|r| r.get::<_, String>(0))
    .collect())
}

/// A table's columns with their type, nullability, default, and whether each is
/// part of the primary key or a foreign key. `None` if the table does not exist,
/// which doubles as the name validation before the name is ever quoted in.
async fn introspect_table(table: &str) -> dorm_core::Result<Option<Vec<ColMeta>>> {
    let cols = db::query(
        "SELECT column_name, data_type, is_nullable, column_default \
         FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name = $1 ORDER BY ordinal_position",
        &[&table],
    )
    .await?;
    if cols.is_empty() {
        return Ok(None);
    }

    let pk: HashSet<String> = db::query(
        "SELECT kcu.column_name FROM information_schema.table_constraints tc \
         JOIN information_schema.key_column_usage kcu ON tc.constraint_name = kcu.constraint_name \
         AND tc.table_schema = kcu.table_schema \
         WHERE tc.table_schema = 'public' AND tc.table_name = $1 AND tc.constraint_type = 'PRIMARY KEY'",
        &[&table],
    )
    .await?
    .iter()
    .map(|r| r.get::<_, String>(0))
    .collect();

    let mut fk: HashMap<String, (String, String)> = HashMap::new();
    for r in db::query(
        "SELECT kcu.column_name, ccu.table_name, ccu.column_name \
         FROM information_schema.table_constraints tc \
         JOIN information_schema.key_column_usage kcu ON tc.constraint_name = kcu.constraint_name \
         JOIN information_schema.constraint_column_usage ccu ON ccu.constraint_name = tc.constraint_name \
         WHERE tc.table_schema = 'public' AND tc.table_name = $1 AND tc.constraint_type = 'FOREIGN KEY'",
        &[&table],
    )
    .await?
    {
        fk.insert(
            r.get::<_, String>(0),
            (r.get::<_, String>(1), r.get::<_, String>(2)),
        );
    }

    Ok(Some(
        cols.iter()
            .map(|r| {
                let name: String = r.get(0);
                ColMeta {
                    data_type: r.get::<_, String>(1),
                    nullable: r.get::<_, String>(2) == "YES",
                    default: r.get::<_, Option<String>>(3),
                    pk: pk.contains(&name),
                    references: fk.get(&name).cloned(),
                    name,
                }
            })
            .collect(),
    ))
}

// --------------------------------------------------------------------- handlers

/// `GET /api/schema` — every table with its columns, types and keys, plus whether
/// this session may write.
async fn schema() -> Result<Json<JsonValue>, StudioError> {
    let mut tables = Vec::new();
    for t in list_tables().await? {
        if let Some(cols) = introspect_table(&t).await? {
            let columns: Vec<JsonValue> = cols
                .iter()
                .map(|c| {
                    json!({
                        "name": c.name,
                        "type": c.data_type,
                        "nullable": c.nullable,
                        "default": c.default,
                        "pk": c.pk,
                        "references": c.references.as_ref()
                            .map(|(t, col)| json!({ "table": t, "column": col })),
                    })
                })
                .collect();
            let has_pk = cols.iter().any(|c| c.pk);
            tables.push(json!({ "name": t, "columns": columns, "hasPk": has_pk }));
        }
    }
    Ok(Json(
        json!({ "writable": !db::is_read_only(), "tables": tables }),
    ))
}

/// `GET /api/tables/:table/rows` — a page of rows, with the matching total.
///
/// Query: `limit` (1..=1000, default 50), `offset`, `orderBy` (a column),
/// `dir` (`asc`/`desc`), and an exact-match filter `filterCol`+`filterVal`.
async fn get_rows(
    Path(table): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<JsonValue>, StudioError> {
    let cols = require_table(&table).await?;
    let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();

    let limit = q
        .get("limit")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(50)
        .clamp(1, 1000);
    let offset = q
        .get("offset")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0)
        .max(0);

    // A WHERE shared by the page query and its count, so the total matches the
    // filtered view. Values are the only bound parameters; a column is quoted
    // only after being checked against the table's own columns.
    let mut conds: Vec<String> = Vec::new();
    let mut params: Vec<String> = Vec::new();
    // An exact-match column filter (the filter box / a foreign-key jump).
    if let (Some(col), Some(val)) = (q.get("filterCol"), q.get("filterVal")) {
        if !names.contains(&col.as_str()) {
            return Err(StudioError::bad(format!("no column `{col}` on `{table}`")));
        }
        params.push(val.clone());
        conds.push(format!("{}::text = ${}", quote_ident(col), params.len()));
    }
    // A global search: a case-insensitive substring match against every column,
    // the Studio's "search any field". The term is escaped so its own `%`/`_`
    // are literal, then bound once and reused across the columns' `ILIKE`s.
    if let Some(term) = q.get("search").filter(|s| !s.is_empty()) {
        let esc = term
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        params.push(format!("%{esc}%"));
        let n = params.len();
        let ors: Vec<String> = names
            .iter()
            .map(|c| format!("{}::text ILIKE ${n} ESCAPE '\\'", quote_ident(c)))
            .collect();
        conds.push(format!("({})", ors.join(" OR ")));
    }
    let where_clause = if conds.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conds.join(" AND "))
    };

    let order_clause = match q.get("orderBy").filter(|c| names.contains(&c.as_str())) {
        Some(col) => {
            let dir = if q.get("dir").map(String::as_str) == Some("desc") {
                "DESC"
            } else {
                "ASC"
            };
            format!(" ORDER BY {} {}", quote_ident(col), dir)
        }
        None => String::new(),
    };

    let select = names
        .iter()
        .map(|c| format!("{}::text", quote_ident(c)))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT {select} FROM {}{}{} LIMIT {} OFFSET {}",
        quote_ident(&table),
        where_clause,
        order_clause,
        limit,
        offset
    );

    let bound: Vec<&(dyn ToSql + Sync)> = params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
    let rows = db::query(&sql, &bound).await?;
    let data: Vec<JsonValue> = rows.iter().map(|row| row_to_json(row, &names)).collect();

    let count_sql = format!(
        "SELECT count(*) FROM {}{}",
        quote_ident(&table),
        where_clause
    );
    let total: i64 = db::query(&count_sql, &bound)
        .await?
        .first()
        .map(|r| r.get(0))
        .unwrap_or(0);

    Ok(Json(json!({
        "columns": names,
        "rows": data,
        "total": total,
        "limit": limit,
        "offset": offset,
    })))
}

/// `POST /api/tables/:table/rows` — insert one row from `{ "values": {col: …} }`
/// and return it with its generated columns. A `null` writes SQL `NULL`; an
/// omitted column takes its database default.
async fn insert_row(
    Path(table): Path<String>,
    Json(body): Json<JsonValue>,
) -> Result<Json<JsonValue>, StudioError> {
    db::guard_write("insert")?;
    let cols = require_table(&table).await?;
    let by_name = index_by_name(&cols);
    let values = object(&body, "values")?;
    if values.is_empty() {
        return Err(StudioError::bad("`values` is empty"));
    }

    let mut collist = Vec::new();
    let mut placeholders = Vec::new();
    let mut params: Vec<String> = Vec::new();
    for (col, val) in values {
        let meta = *by_name
            .get(col.as_str())
            .ok_or_else(|| StudioError::bad(format!("no column `{col}` on `{table}`")))?;
        collist.push(quote_ident(col));
        if val.is_null() {
            placeholders.push("NULL".to_string());
        } else {
            params.push(scalar_text(val)?);
            placeholders.push(format!("${}{}", params.len(), cast_suffix(&meta.data_type)));
        }
    }

    let returning = cols
        .iter()
        .map(|c| format!("{}::text", quote_ident(&c.name)))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({}) RETURNING {}",
        quote_ident(&table),
        collist.join(", "),
        placeholders.join(", "),
        returning
    );

    // `db::query` is the unguarded path; the `guard_write` above is what keeps a
    // read-only session from reaching it. Using it (not `sql_execute`) is what
    // lets the insert come back with its serial key and defaults filled in.
    let bound: Vec<&(dyn ToSql + Sync)> = params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
    let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
    let rows = db::query(&sql, &bound).await?;
    let row = rows
        .first()
        .ok_or_else(|| StudioError::internal("the insert returned no row"))?;
    Ok(Json(json!({ "row": row_to_json(row, &names) })))
}

/// `PATCH /api/tables/:table/rows` — update the row the key identifies, from
/// `{ "pk": {col: …}, "values": {col: …} }`. Refused with no key.
async fn update_row(
    Path(table): Path<String>,
    Json(body): Json<JsonValue>,
) -> Result<Json<JsonValue>, StudioError> {
    db::guard_write("update")?;
    let cols = require_table(&table).await?;
    let by_name = index_by_name(&cols);

    let values = object(&body, "values")?;
    if values.is_empty() {
        return Err(StudioError::bad("`values` is empty"));
    }
    let pk = object(&body, "pk")?;
    if pk.is_empty() {
        return Err(StudioError::bad("refusing an update with no key"));
    }

    let mut params: Vec<Value> = Vec::new();
    let mut sets = Vec::new();
    for (col, val) in values {
        let meta = *by_name
            .get(col.as_str())
            .ok_or_else(|| StudioError::bad(format!("no column `{col}` on `{table}`")))?;
        if val.is_null() {
            sets.push(format!("{} = NULL", quote_ident(col)));
        } else {
            params.push(Value::Text(scalar_text(val)?));
            sets.push(format!(
                "{} = ${}{}",
                quote_ident(col),
                params.len(),
                cast_suffix(&meta.data_type)
            ));
        }
    }

    let where_clause = key_filter(&table, &by_name, pk, &mut params)?;
    let sql = format!(
        "UPDATE {} SET {} WHERE {}",
        quote_ident(&table),
        sets.join(", "),
        where_clause
    );
    let affected = db::sql_execute(&sql, &params).await?;
    Ok(Json(json!({ "affected": affected })))
}

/// `DELETE /api/tables/:table/rows` — delete the row(s) a key identifies. Takes
/// either one key, `{ "pk": {col: …} }`, or many, `{ "keys": [{col: …}, …] }`
/// for a bulk delete. A key with no columns is refused, so a table is never
/// wiped by a slip.
///
/// A bulk delete runs inside a single [`db::atomic`] transaction: either every
/// selected row goes or none does, so a failure partway (a foreign-key
/// restriction on one row) leaves the set untouched rather than half-deleted.
async fn delete_row(
    Path(table): Path<String>,
    Json(body): Json<JsonValue>,
) -> Result<Json<JsonValue>, StudioError> {
    db::guard_write("delete")?;
    let cols = require_table(&table).await?;
    let by_name = index_by_name(&cols);

    // One `pk` or many `keys` — normalised to a list either way.
    let keys: Vec<&serde_json::Map<String, JsonValue>> =
        if let Some(arr) = body.get("keys").and_then(JsonValue::as_array) {
            arr.iter().filter_map(JsonValue::as_object).collect()
        } else if let Some(pk) = body.get("pk").and_then(JsonValue::as_object) {
            vec![pk]
        } else {
            return Err(StudioError::bad(
                "body needs a `pk` object or a `keys` array",
            ));
        };
    if keys.is_empty() {
        return Err(StudioError::bad("no rows to delete"));
    }

    // Build (and validate) every statement up front, so a bad key is a 400
    // before any row is touched; only database errors remain for the transaction.
    let mut stmts: Vec<(String, Vec<Value>)> = Vec::new();
    for &key in &keys {
        if key.is_empty() {
            return Err(StudioError::bad("refusing a delete with no key"));
        }
        let mut params: Vec<Value> = Vec::new();
        let where_clause = key_filter(&table, &by_name, key, &mut params)?;
        stmts.push((
            format!("DELETE FROM {} WHERE {}", quote_ident(&table), where_clause),
            params,
        ));
    }

    let affected = db::atomic(|_tx| async move {
        let mut total = 0u64;
        for (sql, params) in &stmts {
            total += db::sql_execute(sql, params).await?;
        }
        Ok(total)
    })
    .await?;
    Ok(Json(json!({ "affected": affected })))
}

/// `GET /` — the single-page Studio.
async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

// ---------------------------------------------------------------------- helpers

/// Introspect `table`, turning "no such table" into a 400 rather than an empty
/// success — the shared front of every table-scoped handler.
async fn require_table(table: &str) -> Result<Vec<ColMeta>, StudioError> {
    introspect_table(table)
        .await?
        .ok_or_else(|| StudioError::bad(format!("no table `{table}` in the `public` schema")))
}

/// A `name → &ColMeta` index for validating the columns a request names.
fn index_by_name(cols: &[ColMeta]) -> HashMap<&str, &ColMeta> {
    cols.iter().map(|c| (c.name.as_str(), c)).collect()
}

/// Build a `col::text = $n AND …` filter from a key object, pushing each value as
/// a bound text parameter and validating every column against the table. The
/// `::text` comparison keys on any column type without the caller knowing it.
fn key_filter(
    table: &str,
    by_name: &HashMap<&str, &ColMeta>,
    pk: &serde_json::Map<String, JsonValue>,
    params: &mut Vec<Value>,
) -> Result<String, StudioError> {
    let mut parts = Vec::new();
    for (col, val) in pk {
        if !by_name.contains_key(col.as_str()) {
            return Err(StudioError::bad(format!("no column `{col}` on `{table}`")));
        }
        if val.is_null() {
            parts.push(format!("{} IS NULL", quote_ident(col)));
        } else {
            params.push(Value::Text(scalar_text(val)?));
            parts.push(format!("{}::text = ${}", quote_ident(col), params.len()));
        }
    }
    Ok(parts.join(" AND "))
}

/// Read a required JSON object field from a request body.
fn object<'a>(
    body: &'a JsonValue,
    field: &str,
) -> Result<&'a serde_json::Map<String, JsonValue>, StudioError> {
    body.get(field)
        .and_then(JsonValue::as_object)
        .ok_or_else(|| StudioError::bad(format!("body needs a `{field}` object")))
}

/// Turn a PostgreSQL row of `::text` cells into a JSON object keyed by the column
/// names, a SQL `NULL` becoming JSON `null`. Read by index, so a cast that drops
/// the column label does not matter.
fn row_to_json(row: &tokio_postgres::Row, names: &[&str]) -> JsonValue {
    let mut m = serde_json::Map::new();
    for (i, name) in names.iter().enumerate() {
        let cell: Option<String> = row.get(i);
        m.insert(
            (*name).to_string(),
            cell.map(JsonValue::String).unwrap_or(JsonValue::Null),
        );
    }
    JsonValue::Object(m)
}

/// A scalar JSON value as the text a bound parameter carries. Objects and arrays
/// become their JSON text, which is what a `jsonb` column wants.
fn scalar_text(v: &JsonValue) -> Result<String, StudioError> {
    match v {
        JsonValue::String(s) => Ok(s.clone()),
        JsonValue::Number(n) => Ok(n.to_string()),
        JsonValue::Bool(b) => Ok(b.to_string()),
        JsonValue::Object(_) | JsonValue::Array(_) => {
            serde_json::to_string(v).map_err(|e| StudioError::bad(e.to_string()))
        }
        JsonValue::Null => Err(StudioError::bad("unexpected null")),
    }
}

/// The cast that puts a bound text parameter into its column's type, routed
/// **through `text`** (`$n::text::<type>`). The detour is load-bearing: a bare
/// `$n::boolean` makes PostgreSQL infer `$n` as `boolean` and then expect a
/// boolean-encoded parameter, but every value here is bound as text — the
/// mismatch is a "incorrect binary data format" error. `$n::text` fixes `$n` as
/// text first, matching what is sent, before the value is cast on to its column
/// type. This is the same pattern `dorm db execute` documents.
///
/// `ARRAY` and `USER-DEFINED` (enums, composites) have no simple cast target, so
/// the value is bound as text and left to the server to assign — the honest
/// limit of a catalogue-only Studio.
fn cast_suffix(data_type: &str) -> String {
    match data_type {
        "ARRAY" | "USER-DEFINED" => "::text".to_string(),
        other => format!("::text::{other}"),
    }
}

/// Quote a PostgreSQL identifier. Reached only with a name already checked against
/// the catalogue, so doubling `"` is belt-and-braces.
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Hide the password in a DSN before it is echoed in an error.
fn redact(dsn: &str) -> String {
    dsn.split_whitespace()
        .map(|part| {
            if part.starts_with("password=") {
                "password=***".to_string()
            } else {
                part.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ------------------------------------------------------------------------ error

/// A handler error, carrying the HTTP status the browser should see. A write
/// refused by the read-only guard becomes a 403; a bad request a 400; anything
/// else a 500. Every one is returned to the page as `{ "error": "…" }`.
enum StudioError {
    Bad(String),
    Forbidden(String),
    Internal(String),
}

impl StudioError {
    fn bad(m: impl Into<String>) -> Self {
        StudioError::Bad(m.into())
    }
    fn internal(m: impl Into<String>) -> Self {
        StudioError::Internal(m.into())
    }
}

impl From<dorm_core::Error> for StudioError {
    fn from(e: dorm_core::Error) -> Self {
        match e {
            dorm_core::Error::ReadOnly(op) => StudioError::Forbidden(format!(
                "this Studio is read-only; `{op}` needs `dorm studio --write`"
            )),
            other => StudioError::Internal(other.to_string()),
        }
    }
}

impl IntoResponse for StudioError {
    fn into_response(self) -> Response {
        let (code, msg) = match self {
            StudioError::Bad(m) => (StatusCode::BAD_REQUEST, m),
            StudioError::Forbidden(m) => (StatusCode::FORBIDDEN, m),
            StudioError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        };
        (code, Json(json!({ "error": msg }))).into_response()
    }
}

/// The single-page app: all HTML, CSS and JS inline, served from `/`.
const INDEX_HTML: &str = include_str!("studio.html");
