//! A backend-agnostic result row — the central seam of [ADR 0003].
//!
//! The typed `QuerySet`'s async terminals, `Model::from_row`, and the dynamic
//! [`crate::value::read_column`] all decode out of a [`Row`]. A `Row` is one of
//! two things: a `tokio_postgres::Row` straight off the pool, or — behind the
//! `sqlite` feature — a `SqliteRow`, the owned cells of one `rusqlite` result
//! row. Decoding is therefore written once against [`Row`] and works on either
//! backend; which one a process talks to is the DSN's choice, resolved in
//! [`crate::db`].
//!
//! ## Why SQLite rows are materialised
//!
//! A `rusqlite::Row` borrows its statement and cannot outlive the `query` call,
//! so it cannot be collected into a `Vec` the way a `tokio_postgres::Row` can.
//! The SQLite executor therefore reads every column of each row into an owned
//! `Vec<rusqlite::types::Value>` up front; `SqliteRow` holds that.
//!
//! ## Why the exact/temporal types travel as text
//!
//! `Decimal`, `DateTime`, `Uuid` and `Json` are bound to SQLite as text and read
//! back from text, so nothing is lost to SQLite's loose typing. This mirrors
//! `dorm-sqlite` exactly, which is the low-level executor this logic is lifted
//! from — `dorm-sqlite` depends on `dorm-core`, so the shared logic lives here
//! and cannot be depended on back.
//!
//! [ADR 0003]: https://github.com/ngolacloud/dorm/blob/main/docs/adr/0003-backend-generic-db.md

use crate::error::{Error, Result};

/// One row of a result set, from whichever backend produced it.
pub enum Row {
    /// A PostgreSQL row, decoded through `tokio_postgres`' `FromSql`.
    Pg(tokio_postgres::Row),
    /// A SQLite row, its columns already read into owned values.
    #[cfg(feature = "sqlite")]
    Sqlite(SqliteRow),
}

impl Row {
    /// Read column `idx` as `T`, the typed-decode entry point the generated
    /// `Model::from_row` calls.
    ///
    /// The error is deliberately plain: every caller wraps it in a column-named
    /// [`Error::UnexpectedColumnType`] of its own, so repeating the name here
    /// would be noise.
    pub fn try_get<T: Decode>(&self, idx: usize) -> Result<T> {
        match self {
            Row::Pg(row) => T::from_pg(row, idx),
            #[cfg(feature = "sqlite")]
            Row::Sqlite(row) => T::from_sqlite(row.cell(idx)),
        }
    }
}

/// The owned cells of one SQLite result row, in column order.
#[cfg(feature = "sqlite")]
pub struct SqliteRow {
    cells: Vec<rusqlite::types::Value>,
}

#[cfg(feature = "sqlite")]
impl SqliteRow {
    /// Build from the columns read out of a `rusqlite::Row`.
    pub fn new(cells: Vec<rusqlite::types::Value>) -> Self {
        SqliteRow { cells }
    }

    /// The cell at `idx`, or `NULL` if the plan asked for a column the row does
    /// not have — which the column plan makes impossible, but a missing cell is
    /// a `NULL`, never a panic.
    pub(crate) fn cell(&self, idx: usize) -> &rusqlite::types::Value {
        self.cells.get(idx).unwrap_or(&rusqlite::types::Value::Null)
    }
}

/// A Rust type the typed decoder can read out of a [`Row`].
///
/// Implemented for the closed set of column types (and their `Option`s) that the
/// ORM's [`crate::value::Value`] covers, so the derive macro can emit
/// `row.try_get::<T>(i)` for any model field.
pub trait Decode: Sized {
    /// Decode from a PostgreSQL row via `tokio_postgres`' `FromSql`.
    fn from_pg(row: &tokio_postgres::Row, idx: usize) -> Result<Self>;

    /// Decode from one already-read SQLite cell.
    #[cfg(feature = "sqlite")]
    fn from_sqlite(cell: &rusqlite::types::Value) -> Result<Self>;
}

/// Impl [`Decode`] for a scalar and its `Option`. `$from_cell` turns a non-NULL
/// SQLite cell into the value; a NULL is an error for the scalar and `None` for
/// the `Option`, which is exactly PostgreSQL's `try_get::<_, T>` vs
/// `try_get::<_, Option<T>>` split.
macro_rules! impl_decode {
    ($t:ty, $from_cell:expr) => {
        impl Decode for $t {
            fn from_pg(row: &tokio_postgres::Row, idx: usize) -> Result<Self> {
                row.try_get::<usize, $t>(idx)
                    .map_err(|e| Error::Db(e.to_string()))
            }

            #[cfg(feature = "sqlite")]
            fn from_sqlite(cell: &rusqlite::types::Value) -> Result<Self> {
                match cell {
                    rusqlite::types::Value::Null => Err(Error::Db(
                        concat!("unexpected NULL reading a non-null ", stringify!($t)).into(),
                    )),
                    other => ($from_cell)(other),
                }
            }
        }

        impl Decode for Option<$t> {
            fn from_pg(row: &tokio_postgres::Row, idx: usize) -> Result<Self> {
                row.try_get::<usize, Option<$t>>(idx)
                    .map_err(|e| Error::Db(e.to_string()))
            }

            #[cfg(feature = "sqlite")]
            fn from_sqlite(cell: &rusqlite::types::Value) -> Result<Self> {
                match cell {
                    rusqlite::types::Value::Null => Ok(None),
                    other => ($from_cell)(other).map(Some),
                }
            }
        }
    };
}

// The `$from_cell` closures are compiled only under `sqlite`; with the feature
// off they are dead tokens inside the cfg'd `from_sqlite` body and reference no
// `rusqlite` item.
impl_decode!(i32, |c| sqlite_int(c).map(|n| n as i32));
impl_decode!(i64, sqlite_int);
impl_decode!(bool, |c| sqlite_int(c).map(|n| n != 0));
impl_decode!(String, sqlite_text);
impl_decode!(rust_decimal::Decimal, |c| {
    use std::str::FromStr as _;
    rust_decimal::Decimal::from_str(&sqlite_text(c)?).map_err(|_| Error::Db("not a decimal".into()))
});
impl_decode!(chrono::DateTime<chrono::Utc>, |c| sqlite_timestamp(
    &sqlite_text(c)?
));
impl_decode!(uuid::Uuid, |c| sqlite_text(c)?
    .parse()
    .map_err(|_| Error::Db("not a uuid".into())));
impl_decode!(serde_json::Value, |c| serde_json::from_str(&sqlite_text(
    c
)?)
.map_err(|_| Error::Db("not json".into())));

/// A SQLite integer cell, or a type error.
#[cfg(feature = "sqlite")]
fn sqlite_int(cell: &rusqlite::types::Value) -> Result<i64> {
    match cell {
        rusqlite::types::Value::Integer(n) => Ok(*n),
        _ => Err(Error::Db("expected an integer".into())),
    }
}

/// A SQLite text cell, or a type error.
#[cfg(feature = "sqlite")]
fn sqlite_text(cell: &rusqlite::types::Value) -> Result<String> {
    match cell {
        rusqlite::types::Value::Text(s) => Ok(s.clone()),
        _ => Err(Error::Db("expected text".into())),
    }
}

/// Parse a timestamp out of SQLite text, accepting both what the ORM writes and
/// what SQLite's own `CURRENT_TIMESTAMP` default produces.
///
/// The ORM binds a `DateTime` as RFC 3339, so a value it wrote round-trips
/// exactly. But an `auto_now_add` column left to its SQLite column default is
/// stamped by `CURRENT_TIMESTAMP`, whose format is the space-separated
/// `YYYY-MM-DD HH:MM:SS` in UTC — not RFC 3339. Both are UTC, so both are
/// accepted here rather than failing to decode a row the schema itself created.
#[cfg(feature = "sqlite")]
fn sqlite_timestamp(s: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t.with_timezone(&Utc));
    }
    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            return Ok(Utc.from_utc_datetime(&naive));
        }
    }
    Err(Error::Db("not a recognised timestamp".into()))
}

/// Bind one ORM [`Value`](crate::value::Value) as a SQLite parameter. The
/// counterpart of the `$from_cell` decoders, and identical to `dorm-sqlite`'s
/// own binding so the two round-trip the same way.
#[cfg(feature = "sqlite")]
pub(crate) fn value_to_sqlite(v: &crate::value::Value) -> rusqlite::types::Value {
    use crate::value::Value;
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

/// Decode one SQLite cell into the [`Value`](crate::value::Value) its declared
/// [`FieldType`](crate::value::FieldType) expects — the dynamic (model-erased)
/// path that [`crate::value::read_column`] takes for a SQLite row. A `NULL` is
/// [`Value::Null`](crate::value::Value::Null) whatever the declared type.
#[cfg(feature = "sqlite")]
pub(crate) fn cell_to_value(
    cell: &rusqlite::types::Value,
    ty: crate::value::FieldType,
    column: &str,
) -> Result<crate::value::Value> {
    use crate::value::{FieldType, Value};
    if matches!(cell, rusqlite::types::Value::Null) {
        return Ok(Value::Null);
    }
    let mismatch = |want: &'static str| Error::UnexpectedColumnType {
        column: column.to_string(),
        want,
    };
    Ok(match ty {
        FieldType::I32 => Value::I32(sqlite_int(cell).map_err(|_| mismatch("integer"))? as i32),
        FieldType::I64 => Value::I64(sqlite_int(cell).map_err(|_| mismatch("bigint"))?),
        FieldType::Bool => Value::Bool(sqlite_int(cell).map_err(|_| mismatch("boolean"))? != 0),
        FieldType::Text => Value::Text(sqlite_text(cell).map_err(|_| mismatch("text"))?),
        FieldType::Decimal => {
            use std::str::FromStr as _;
            Value::Decimal(
                rust_decimal::Decimal::from_str(
                    &sqlite_text(cell).map_err(|_| mismatch("decimal text"))?,
                )
                .map_err(|_| mismatch("decimal"))?,
            )
        }
        FieldType::Timestamp => Value::Timestamp(
            sqlite_timestamp(&sqlite_text(cell).map_err(|_| mismatch("timestamp text"))?)
                .map_err(|_| mismatch("timestamp"))?,
        ),
        FieldType::Uuid => Value::Uuid(
            sqlite_text(cell)
                .map_err(|_| mismatch("uuid text"))?
                .parse()
                .map_err(|_| mismatch("uuid"))?,
        ),
        FieldType::Json => Value::Json(
            serde_json::from_str(&sqlite_text(cell).map_err(|_| mismatch("json text"))?)
                .map_err(|_| mismatch("json"))?,
        ),
    })
}
