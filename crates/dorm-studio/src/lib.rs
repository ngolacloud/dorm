//! DORM Studio — a Prisma-Studio-style web UI over a project's **typed** models.
//!
//! This is the library a generated `src/bin/studio.rs` links: the project binary
//! links its models (so `dorm_core::registry` is populated) and calls [`serve`],
//! and everything else lives here — the server, the handlers, and the
//! single-page app.
//!
//! ## Typed, not introspected
//!
//! `dorm-cli`'s `dorm studio` is native: it reads the shape of the database from
//! `information_schema`, so it works against any PostgreSQL without a build. This
//! crate is the other half — it reads the shape from the **registry the models
//! populate**, so it knows what the catalogue cannot: a relation's *name* and its
//! reverse (`Tenant.apps`), each field's declared type, length and uniqueness.
//! Reads and writes go through the model-erased `QuerySet` terminals
//! ([`fetch_value_rows`]/[`insert_value_rows`]/…), so the compiler, the rules and
//! the read-only guard are exactly the typed API's.
//!
//! The page is the same one the native Studio serves — [`INDEX_HTML`] is
//! re-exported for `dorm-cli` to use — so the two share one UI over one JSON
//! contract: `GET /api/schema` and `GET`/`POST`/`PATCH`/`DELETE
//! /api/tables/<model>/rows`.

use std::net::SocketAddr;

use axum::extract::{Path, Query};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use dorm_core::compiler::{self, Plan};
use dorm_core::expr::{Assignment, OrderBy, F, Q};
use dorm_core::meta::{FieldMeta, ModelMeta};
use dorm_core::queryset::{
    delete_value_rows, fetch_value_rows, insert_value_rows, update_value_rows, ValueRow,
};
use dorm_core::value::{FieldType, Value};
use dorm_core::{db, registry};
use serde_json::{json, Value as JsonValue};
use std::collections::HashMap;

/// The single-page app, shared with `dorm-cli`'s native Studio.
pub const INDEX_HTML: &str = include_str!("studio.html");

/// Start the Studio over the linked models and run it until Ctrl-C.
///
/// `writable` enables the DML verbs; off, every write refuses at
/// [`db::guard_write`] and the browser sees a 403. The listener binds
/// `127.0.0.1` only.
pub async fn serve(port: u16, writable: bool) -> Result<(), String> {
    db::dsn().map_err(|e| e.to_string())?;
    if registry::models().next().is_none() {
        return Err(
            "no models are registered — the project's models must be linked \
                    into the Studio binary"
                .into(),
        );
    }
    db::set_read_only(!writable);
    // Fail fast if the database is unreachable.
    db::sql("SELECT 1", &[])
        .await
        .map_err(|e| format!("cannot reach the database: {e}"))?;

    let app = Router::new()
        .route("/", get(index))
        .route("/api/schema", get(schema))
        .route(
            "/api/tables/:model/rows",
            get(get_rows)
                .post(insert_row)
                .patch(update_row)
                .delete(delete_row),
        )
        .route("/api/tables/:model/export.csv", get(export_csv));

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("cannot bind {addr}: {e}"))?;

    println!(
        "DORM Studio (typed) — {}",
        if writable {
            "read-write (--write)"
        } else {
            "read-only — pass --write to create/edit/delete rows"
        }
    );
    println!("  open  http://{addr}");
    println!("  stop  Ctrl-C");

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|e| e.to_string())
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

/// The primary-key field of a model, if it has one.
fn pk_field(meta: &'static ModelMeta) -> Option<&'static dorm_core::meta::FieldMeta> {
    meta.fields.iter().find(|f| f.primary_key)
}

/// `GET /api/schema` — every model with its columns, declared types, keys,
/// forward relations (named, with the referenced model and its key) and reverse
/// relations (`Tenant.apps`), plus whether this session may write. The richer
/// shape the typed registry carries over bare introspection.
async fn schema() -> Result<Json<JsonValue>, StudioError> {
    let mut tables = Vec::new();
    for m in registry::models() {
        let columns: Vec<JsonValue> = m
            .fields
            .iter()
            .map(|f| {
                // A foreign key points at another model's primary key.
                let references = f.relation.as_ref().map(|r| {
                    let col = registry::model(r.model)
                        .ok()
                        .and_then(pk_field)
                        .map(|p| p.name)
                        .unwrap_or("id");
                    json!({ "table": r.model, "column": col })
                });
                json!({
                    "name": f.name,
                    "type": f.ty.describe(),
                    "pk": f.primary_key,
                    "unique": f.unique,
                    "nullable": f.nullable,
                    "maxLength": f.max_length,
                    "references": references,
                })
            })
            .collect();

        // Reverse relations: another model whose foreign key points back here,
        // exposed under the `related_name` the schema declared (`apps`).
        let mut reverse = Vec::new();
        for other in registry::models() {
            for of in other.fields {
                if let Some(r) = &of.relation {
                    if r.model == m.name {
                        if let Some(name) = r.related_name {
                            reverse.push(json!({
                                "name": name,
                                "model": other.name,
                                "column": of.name,
                            }));
                        }
                    }
                }
            }
        }

        tables.push(json!({
            "name": m.name,
            "table": m.table,
            "columns": columns,
            "hasPk": m.fields.iter().any(|f| f.primary_key),
            "reverse": reverse,
        }));
    }
    Ok(Json(
        json!({ "writable": !db::is_read_only(), "typed": true, "tables": tables }),
    ))
}

/// `GET /api/tables/<model>/rows` — a page of rows with its matching total.
///
/// Query: `limit` (1..=1000, 50), `offset`, `orderBy` (a field), `dir`
/// (`asc`/`desc`), an exact `filterCol`+`filterVal`, and a `search` term matched
/// (case-insensitively) against every text field.
async fn get_rows(
    Path(model): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<JsonValue>, StudioError> {
    let meta = model_of(&model)?;
    let fields: Vec<&str> = meta.fields.iter().map(|f| f.name).collect();

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

    // Filters are shared by the page and the count, so the total matches the
    // view. Every value is a bound parameter; each field is checked against the
    // model before it is used.
    let filters = list_filters(meta, &model, &q)?;

    // Count with the same filters, ordering suppressed.
    let mut cplan = Plan::new(meta);
    cplan.filters = filters.clone();
    cplan.unordered = true;
    let csql = compiler::compile_count_on(&cplan, db::dialect())?;
    let total: i64 = match db::sql(&csql.text, &csql.params).await?.first() {
        Some(r) => r.try_get::<i64>(0)?,
        None => 0,
    };

    // The page.
    let mut plan = Plan::new(meta);
    plan.filters = filters;
    plan.order = order_terms(&fields, &q);
    plan.limit = Some(limit);
    plan.offset = Some(offset);
    let rows = fetch_value_rows(&plan).await?;
    let data: Vec<JsonValue> = rows.iter().map(|r| value_row_to_json(r, meta)).collect();

    Ok(Json(json!({
        "columns": fields,
        "rows": data,
        "total": total,
        "limit": limit,
        "offset": offset,
    })))
}

/// The most rows a CSV export streams, so a huge table cannot pull unbounded.
const CSV_MAX: i64 = 100_000;

/// The request's exact filter and global search as `Q`s — shared by the paged
/// read, its count, and the CSV export, so all three select the same rows.
fn list_filters(
    meta: &'static ModelMeta,
    model: &str,
    q: &HashMap<String, String>,
) -> Result<Vec<Q>, StudioError> {
    let mut filters: Vec<Q> = Vec::new();
    if let (Some(col), Some(val)) = (q.get("filterCol"), q.get("filterVal")) {
        let f = field_of(meta, col, model)?;
        filters.push(Q::new(
            col.clone(),
            coerce(f.ty, &JsonValue::String(val.clone()))?,
        ));
    }
    if let Some(term) = q.get("search").filter(|s| !s.is_empty()) {
        // Only text fields: `icontains` is an `ILIKE`, which the other scalar
        // types do not take without a cast the typed path does not emit.
        let mut any: Option<Q> = None;
        for f in meta.fields.iter().filter(|f| f.ty == FieldType::Text) {
            let cond = Q::new(format!("{}__icontains", f.name), Value::Text(term.clone()));
            any = Some(match any {
                Some(acc) => acc | cond,
                None => cond,
            });
        }
        if let Some(search) = any {
            filters.push(search);
        }
    }
    Ok(filters)
}

/// The request's sort as an `OrderBy`, if `orderBy` names a field.
fn order_terms(fields: &[&str], q: &HashMap<String, String>) -> Vec<OrderBy> {
    match q.get("orderBy").filter(|c| fields.contains(&c.as_str())) {
        Some(col) => {
            let desc = q.get("dir").map(String::as_str) == Some("desc");
            vec![OrderBy::parse(&format!(
                "{}{}",
                if desc { "-" } else { "" },
                col
            ))]
        }
        None => Vec::new(),
    }
}

/// `GET /api/tables/<model>/export.csv` — the current view as a CSV download,
/// with the same filter, search and sort as the grid, capped at [`CSV_MAX`] rows.
async fn export_csv(
    Path(model): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, StudioError> {
    let meta = model_of(&model)?;
    let fields: Vec<&str> = meta.fields.iter().map(|f| f.name).collect();
    let mut plan = Plan::new(meta);
    plan.filters = list_filters(meta, &model, &q)?;
    plan.order = order_terms(&fields, &q);
    plan.limit = Some(CSV_MAX);
    let rows = fetch_value_rows(&plan).await?;

    let mut out = String::new();
    out.push_str(&csv_line(
        &fields.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
    ));
    for row in &rows {
        let cells: Vec<String> = meta
            .fields
            .iter()
            .map(|f| {
                row.get(f.name)
                    .or_else(|| row.get(f.column))
                    .map(value_to_cell)
                    .unwrap_or_default()
            })
            .collect();
        out.push_str(&csv_line(&cells));
    }

    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{model}.csv\""),
            ),
        ],
        out,
    )
        .into_response())
}

/// An ORM [`Value`] as a CSV cell: a `NULL` is an empty field, scalars their
/// text, and the rest (decimal, timestamp, uuid, json) their string form.
fn value_to_cell(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::I32(n) => n.to_string(),
        Value::I64(n) => n.to_string(),
        Value::Text(s) => s.clone(),
        Value::Decimal(d) => d.to_string(),
        Value::Timestamp(t) => t.to_rfc3339(),
        Value::Uuid(u) => u.to_string(),
        Value::Json(j) => j.to_string(),
    }
}

/// One CSV record (RFC 4180): a field with a comma, quote or newline is wrapped
/// in quotes with its own quotes doubled; the line ends `\r\n`.
fn csv_line(cells: &[String]) -> String {
    let escaped: Vec<String> = cells
        .iter()
        .map(|c| {
            if c.contains([',', '"', '\n', '\r']) {
                format!("\"{}\"", c.replace('"', "\"\""))
            } else {
                c.clone()
            }
        })
        .collect();
    let mut line = escaped.join(",");
    line.push_str("\r\n");
    line
}

/// `POST /api/tables/<model>/rows` — insert one row from `{ "values": {field: …} }`
/// through the typed insert, returning it with its generated columns.
async fn insert_row(
    Path(model): Path<String>,
    Json(body): Json<JsonValue>,
) -> Result<Json<JsonValue>, StudioError> {
    db::guard_write("insert")?;
    let meta = model_of(&model)?;
    let values = object(&body, "values")?;
    if values.is_empty() {
        return Err(StudioError::bad("`values` is empty"));
    }
    let mut row: Vec<(String, Value)> = Vec::new();
    for (field, v) in values {
        let f = field_of(meta, field, &model)?;
        row.push((field.clone(), coerce(f.ty, v)?));
    }
    let returned = insert_value_rows(meta, &[row]).await?;
    let first = returned
        .first()
        .ok_or_else(|| StudioError::internal("the insert returned no row"))?;
    Ok(Json(json!({ "row": value_row_to_json(first, meta) })))
}

/// `PATCH /api/tables/<model>/rows` — update the row the key identifies, from
/// `{ "pk": {field: …}, "values": {field: …} }`. Refused with no key.
async fn update_row(
    Path(model): Path<String>,
    Json(body): Json<JsonValue>,
) -> Result<Json<JsonValue>, StudioError> {
    db::guard_write("update")?;
    let meta = model_of(&model)?;
    let values = object(&body, "values")?;
    if values.is_empty() {
        return Err(StudioError::bad("`values` is empty"));
    }
    let pk = object(&body, "pk")?;
    if pk.is_empty() {
        return Err(StudioError::bad("refusing an update with no key"));
    }
    let mut plan = Plan::new(meta);
    for (field, v) in pk {
        let f = field_of(meta, field, &model)?;
        plan.filters.push(Q::new(field.clone(), coerce(f.ty, v)?));
    }
    let mut assignments = Vec::new();
    for (field, v) in values {
        let f = field_of(meta, field, &model)?;
        assignments.push(Assignment::new(field.clone(), F::lit(coerce(f.ty, v)?)));
    }
    let affected = update_value_rows(&plan, &assignments).await?;
    Ok(Json(json!({ "affected": affected })))
}

/// `DELETE /api/tables/<model>/rows` — delete the row(s) a key identifies, from
/// `{ "pk": {…} }` or `{ "keys": [{…}, …] }`. A bulk delete runs in one
/// [`db::atomic`] transaction: all go or none do.
async fn delete_row(
    Path(model): Path<String>,
    Json(body): Json<JsonValue>,
) -> Result<Json<JsonValue>, StudioError> {
    db::guard_write("delete")?;
    let meta = model_of(&model)?;

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

    // Validate and build every plan up front, so a bad key is a 400 before any
    // row is touched; the transaction then runs only database work.
    let mut plans: Vec<Plan> = Vec::new();
    for key in &keys {
        if key.is_empty() {
            return Err(StudioError::bad("refusing a delete with no key"));
        }
        let mut plan = Plan::new(meta);
        for (field, v) in *key {
            let f = field_of(meta, field, &model)?;
            plan.filters.push(Q::new(field.clone(), coerce(f.ty, v)?));
        }
        plans.push(plan);
    }

    let affected = db::atomic(|_tx| async move {
        let mut total = 0u64;
        for plan in &plans {
            total += delete_value_rows(plan).await?;
        }
        Ok(total)
    })
    .await?;
    Ok(Json(json!({ "affected": affected })))
}

// ---------------------------------------------------------------------- helpers

/// Resolve a model by name, turning the unknown-model error into a 400.
fn model_of(name: &str) -> Result<&'static ModelMeta, StudioError> {
    registry::model(name).map_err(|_| StudioError::bad(format!("no model `{name}`")))
}

/// Resolve `field` on `meta`, or a 400 naming it — the validation every column a
/// request names passes through.
fn field_of<'a>(
    meta: &'a ModelMeta,
    field: &str,
    model: &str,
) -> Result<&'a FieldMeta, StudioError> {
    meta.fields
        .iter()
        .find(|f| f.name == field)
        .ok_or_else(|| StudioError::bad(format!("no field `{field}` on `{model}`")))
}

/// Convert a JSON value into the ORM [`Value`] a field expects, **directed by the
/// field's declared type**. The compiler binds the value straight onto a typed
/// column, so an integer column needs an `I64` — unlike the native Studio, which
/// casts text in SQL. Text, decimal, timestamp, uuid and json take a `Text` the
/// compiler then coerces; a `null` is `Value::Null`.
fn coerce(ty: FieldType, v: &JsonValue) -> Result<Value, StudioError> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    match ty {
        FieldType::I32 | FieldType::I64 => {
            let n = match v {
                JsonValue::Number(n) => n.as_i64(),
                JsonValue::String(s) => s.trim().parse::<i64>().ok(),
                _ => None,
            }
            .ok_or_else(|| StudioError::bad(format!("`{v}` is not a whole number")))?;
            Ok(Value::I64(n))
        }
        FieldType::Bool => {
            let b = match v {
                JsonValue::Bool(b) => Some(*b),
                JsonValue::String(s) => match s.as_str() {
                    "true" | "True" => Some(true),
                    "false" | "False" => Some(false),
                    _ => None,
                },
                _ => None,
            }
            .ok_or_else(|| StudioError::bad(format!("`{v}` is not a boolean")))?;
            Ok(Value::Bool(b))
        }
        // Text, Decimal, Timestamp, Uuid, Json: a `Text` (or the JSON itself) that
        // the compiler coerces onto the column's type.
        _ => Ok(json_to_value(v)),
    }
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

/// A `ValueRow` as a JSON object, in the model's declared field order so the
/// grid's columns and cells line up.
fn value_row_to_json(row: &ValueRow, meta: &ModelMeta) -> JsonValue {
    let mut m = serde_json::Map::new();
    for f in meta.fields {
        let v = row
            .get(f.name)
            .or_else(|| row.get(f.column))
            .map(value_to_json)
            .unwrap_or(JsonValue::Null);
        m.insert(f.name.to_string(), v);
    }
    JsonValue::Object(m)
}

/// An ORM [`Value`] as JSON. Scalars keep their natural JSON kind; the rest
/// (decimal, timestamp, uuid) are strings, which is what the grid shows and what
/// comes back on an edit.
fn value_to_json(v: &Value) -> JsonValue {
    match v {
        Value::Null => JsonValue::Null,
        Value::Bool(b) => JsonValue::Bool(*b),
        Value::I32(n) => JsonValue::from(*n),
        Value::I64(n) => JsonValue::from(*n),
        Value::Text(s) => JsonValue::String(s.clone()),
        Value::Decimal(d) => JsonValue::String(d.to_string()),
        Value::Timestamp(t) => JsonValue::String(t.to_rfc3339()),
        Value::Uuid(u) => JsonValue::String(u.to_string()),
        Value::Json(j) => j.clone(),
    }
}

/// A JSON value from a request as an ORM [`Value`]. Numbers and strings become
/// `I64`/`Text` and the compiler coerces them to the field's declared type — the
/// same path the shell's `insert`/`update` take; objects and arrays are `jsonb`.
fn json_to_value(v: &JsonValue) -> Value {
    match v {
        JsonValue::Null => Value::Null,
        JsonValue::Bool(b) => Value::Bool(*b),
        JsonValue::Number(n) => match n.as_i64() {
            Some(i) => Value::I64(i),
            None => Value::Text(n.to_string()),
        },
        JsonValue::String(s) => Value::Text(s.clone()),
        other => Value::Json(other.clone()),
    }
}

// ------------------------------------------------------------------------ error

/// A handler error carrying the HTTP status the browser should see: a write
/// refused by the read-only guard is a 403, a bad request a 400, anything else a
/// 500. Every one is returned as `{ "error": "…" }`.
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
            dorm_core::Error::ReadOnly(op) => {
                StudioError::Forbidden(format!("this Studio is read-only; `{op}` needs `--write`"))
            }
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
