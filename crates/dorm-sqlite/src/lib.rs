//! `dorm-sqlite` — run DORM's compiled SQL against SQLite (ADR 0002, brick 2).
//!
//! DORM's query compiler is pure: a `Plan` in, a `Sql { text, params, columns }`
//! out. Compiled for the SQLite dialect
//! ([`dorm_core::compiler::compile_select_on`] &co. with
//! [`dorm_core::compiler::Dialect::Sqlite`]), the statement uses `?n`
//! placeholders — exactly what `rusqlite` binds. This crate executes that `Sql`:
//! it binds each ORM [`Value`] to SQLite, runs the statement, and decodes the
//! result rows back to [`ValueRow`]s using the column plan the compiler produced.
//!
//! It is the SQLite half of the driver seam. It is deliberately **not** wired
//! into `QuerySet`'s async terminals (`.all()`/`.rows()`), which still go through
//! the PostgreSQL pool in `dorm-core::db`; that integration — one `db` over a
//! backend trait — is the remaining step of brick 2. Here, you build a `Plan`
//! with the typed `QuerySet`, compile it for SQLite, and run it with [`query`] /
//! [`execute`]. SQLite is synchronous, so these are plain functions.
//!
//! ```no_run
//! # use dorm_core::compiler::{compile_select_on, Dialect};
//! # fn demo(conn: &rusqlite::Connection, plan: &dorm_core::compiler::Plan) -> dorm_core::Result<()> {
//! let sql = compile_select_on(plan, Dialect::Sqlite)?;
//! let rows = dorm_sqlite::query(conn, &sql)?;
//! # let _ = rows; Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

use std::str::FromStr;

use dorm_core::compiler::Sql;
use dorm_core::error::{Error, Result};
use dorm_core::queryset::ValueRow;
use dorm_core::value::{FieldType, Value};

/// Run DDL (one or more statements) that takes no parameters — `CREATE TABLE`,
/// `CREATE INDEX`. Use `dorm_migrations::ddl::schema_for_on` with
/// `Backend::Sqlite` to produce it.
pub fn execute_ddl(conn: &rusqlite::Connection, ddl: &str) -> Result<()> {
    conn.execute_batch(ddl).map_err(sqlite_err)
}

/// Run a compiled statement that returns rows (`SELECT`, or an `INSERT …
/// RETURNING *`), decoding each row to a [`ValueRow`] by the compiler's column
/// plan.
pub fn query(conn: &rusqlite::Connection, sql: &Sql) -> Result<Vec<ValueRow>> {
    let mut stmt = conn.prepare(&sql.text).map_err(sqlite_err)?;
    let params: Vec<rusqlite::types::Value> = sql.params.iter().map(to_sqlite).collect();
    let mut rows = stmt
        .query(rusqlite::params_from_iter(params))
        .map_err(sqlite_err)?;

    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(sqlite_err)? {
        let mut map = ValueRow::new();
        for (i, spec) in sql.columns.iter().enumerate() {
            map.insert(spec.name.clone(), decode(row, i, spec.ty, &spec.name)?);
        }
        out.push(map);
    }
    Ok(out)
}

/// Run a compiled statement for its row count only (`INSERT`/`UPDATE`/`DELETE`
/// without `RETURNING`).
pub fn execute(conn: &rusqlite::Connection, sql: &Sql) -> Result<usize> {
    let params: Vec<rusqlite::types::Value> = sql.params.iter().map(to_sqlite).collect();
    conn.execute(&sql.text, rusqlite::params_from_iter(params))
        .map_err(sqlite_err)
}

/// One ORM [`Value`] as a SQLite value. Exact and temporal types travel as text
/// so nothing is lost to SQLite's loose typing — the same choice the JSON API
/// makes.
fn to_sqlite(v: &Value) -> rusqlite::types::Value {
    use rusqlite::types::Value as S;
    match v {
        Value::Null => S::Null,
        Value::Bool(b) => S::Integer(i64::from(*b)),
        Value::I32(n) => S::Integer(i64::from(*n)),
        Value::I64(n) => S::Integer(*n),
        Value::Text(s) => S::Text(s.clone()),
        Value::Decimal(d) => S::Text(d.to_string()),
        Value::Timestamp(t) => S::Text(t.to_rfc3339()),
        Value::Uuid(u) => S::Text(u.to_string()),
        Value::Json(j) => S::Text(j.to_string()),
    }
}

/// Decode one column into the [`Value`] its declared [`FieldType`] expects. A
/// SQL `NULL` is [`Value::Null`] regardless of the declared type.
fn decode(row: &rusqlite::Row, i: usize, ty: FieldType, column: &str) -> Result<Value> {
    let raw: rusqlite::types::Value = row.get(i).map_err(sqlite_err)?;
    if matches!(raw, rusqlite::types::Value::Null) {
        return Ok(Value::Null);
    }
    let mismatch = |want: &'static str| Error::UnexpectedColumnType {
        column: column.to_string(),
        want,
    };
    let as_i64 = |r: &rusqlite::types::Value| match r {
        rusqlite::types::Value::Integer(n) => Some(*n),
        _ => None,
    };
    let as_text = |r: &rusqlite::types::Value| match r {
        rusqlite::types::Value::Text(s) => Some(s.clone()),
        _ => None,
    };
    Ok(match ty {
        FieldType::I32 => Value::I32(as_i64(&raw).ok_or(mismatch("integer"))? as i32),
        FieldType::I64 => Value::I64(as_i64(&raw).ok_or(mismatch("integer"))?),
        FieldType::Bool => Value::Bool(as_i64(&raw).ok_or(mismatch("integer"))? != 0),
        FieldType::Text => Value::Text(as_text(&raw).ok_or(mismatch("text"))?),
        FieldType::Decimal => Value::Decimal(
            rust_decimal::Decimal::from_str(&as_text(&raw).ok_or(mismatch("decimal text"))?)
                .map_err(|_| mismatch("decimal"))?,
        ),
        FieldType::Timestamp => Value::Timestamp(
            chrono::DateTime::parse_from_rfc3339(&as_text(&raw).ok_or(mismatch("timestamp text"))?)
                .map_err(|_| mismatch("RFC 3339 timestamp"))?
                .with_timezone(&chrono::Utc),
        ),
        FieldType::Uuid => Value::Uuid(
            as_text(&raw)
                .ok_or(mismatch("uuid text"))?
                .parse()
                .map_err(|_| mismatch("uuid"))?,
        ),
        FieldType::Json => Value::Json(
            serde_json::from_str(&as_text(&raw).ok_or(mismatch("json text"))?)
                .map_err(|_| mismatch("json"))?,
        ),
    })
}

fn sqlite_err(e: rusqlite::Error) -> Error {
    Error::Db(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dorm_core::compiler::{
        compile_count_on, compile_delete_on, compile_insert_on, compile_select_on,
        compile_update_on, Dialect, Plan,
    };
    use dorm_core::expr::{Assignment, F, Q};
    use dorm_core::meta::{FieldMeta, ModelMeta};

    const FIELDS: &[FieldMeta] = &[
        FieldMeta {
            name: "id",
            column: "id",
            ty: FieldType::I64,
            nullable: false,
            primary_key: true,
            unique: false,
            index: false,
            max_length: None,
            default: None,
            auto_now_add: false,
            auto_now: false,
            relation: None,
        },
        FieldMeta {
            name: "name",
            column: "name",
            ty: FieldType::Text,
            nullable: false,
            primary_key: false,
            unique: false,
            index: false,
            max_length: None,
            default: None,
            auto_now_add: false,
            auto_now: false,
            relation: None,
        },
        FieldMeta {
            name: "qty",
            column: "qty",
            ty: FieldType::I32,
            nullable: false,
            primary_key: false,
            unique: false,
            index: false,
            max_length: None,
            default: None,
            auto_now_add: false,
            auto_now: false,
            relation: None,
        },
    ];
    const WIDGET: ModelMeta = ModelMeta {
        name: "Widget",
        table: "widgets",
        fields: FIELDS,
        ordering: &[],
        tenant_scoped: false,
        unique_together: &[],
        checks: &[],
    };

    #[test]
    fn insert_and_select_round_trip_through_sqlite() {
        let conn = rusqlite::Connection::open_in_memory().expect("open");
        execute_ddl(
            &conn,
            "CREATE TABLE widgets (id INTEGER PRIMARY KEY AUTOINCREMENT, \
             name TEXT NOT NULL, qty INTEGER NOT NULL);",
        )
        .expect("ddl");

        // INSERT … RETURNING *, compiled for SQLite (`?n` placeholders).
        let row = vec![
            ("name".to_string(), Value::Text("widget".into())),
            ("qty".to_string(), Value::I32(5)),
        ];
        let insert = compile_insert_on(&WIDGET, std::slice::from_ref(&row), Dialect::Sqlite)
            .expect("compile insert");
        assert!(
            insert.text.contains("?1"),
            "sqlite placeholders: {}",
            insert.text
        );
        let created = query(&conn, &insert).expect("insert");
        assert_eq!(created.len(), 1);
        assert_eq!(created[0]["id"], Value::I64(1), "serial key assigned");
        assert_eq!(created[0]["name"], Value::Text("widget".into()));
        assert_eq!(created[0]["qty"], Value::I32(5));

        // A typed SELECT built by the same QuerySet plan, compiled for SQLite.
        let plan = Plan::new(&WIDGET);
        let select = compile_select_on(&plan, Dialect::Sqlite).expect("compile select");
        let rows = query(&conn, &select).expect("select");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["name"], Value::Text("widget".into()));
        assert_eq!(rows[0]["qty"], Value::I32(5));

        // UPDATE … WHERE pk IN (subquery): the SET placeholder and the subquery's
        // are renumbered as `?1`, `?2` for SQLite.
        let mut filtered = Plan::new(&WIDGET);
        filtered.filters.push(Q::new("id", Value::I64(1)));
        let update = compile_update_on(
            &filtered,
            &[Assignment::new("qty", F::lit(Value::I64(99)))],
            Dialect::Sqlite,
        )
        .expect("compile update");
        assert!(
            update.text.contains("?1") && update.text.contains("?2"),
            "{}",
            update.text
        );
        assert_eq!(execute(&conn, &update).expect("update"), 1);
        let after = query(&conn, &select).expect("re-select");
        assert_eq!(after[0]["qty"], Value::I32(99), "update applied");

        // COUNT, then DELETE, then COUNT again.
        let count = compile_count_on(&plan, Dialect::Sqlite).expect("compile count");
        assert_eq!(
            query(&conn, &count).expect("count")[0]["count"],
            Value::I64(1)
        );
        let delete = compile_delete_on(&filtered, Dialect::Sqlite).expect("compile delete");
        assert_eq!(execute(&conn, &delete).expect("delete"), 1);
        assert_eq!(
            query(&conn, &count).expect("count 0")[0]["count"],
            Value::I64(0)
        );
    }
}
