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
use tokio_postgres::types::{FromSql, Kind, Type};

/// Reads a native enum column as its text label (ADR 0046).
///
/// `String`'s own `FromSql` rejects a custom enum type — the mirror of the
/// `ToSql` side in [`crate::value`] — so this wrapper accepts `Kind::Enum` and
/// decodes the raw bytes, which for an enum are exactly the label's UTF-8. Shared
/// by the untyped [`crate::value::read_column`] and the typed `String` [`Decode`].
pub(crate) struct PgEnumLabel(pub String);

impl<'a> FromSql<'a> for PgEnumLabel {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> std::result::Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(PgEnumLabel(std::str::from_utf8(raw)?.to_owned()))
    }

    fn accepts(ty: &Type) -> bool {
        matches!(ty.kind(), Kind::Enum(_))
    }
}

/// One row of a result set, from whichever backend produced it.
pub enum Row {
    /// A PostgreSQL row, decoded through `tokio_postgres`' `FromSql`.
    Pg(tokio_postgres::Row),
    /// A SQLite row, its columns already read into owned values.
    #[cfg(feature = "sqlite")]
    Sqlite(SqliteRow),
    /// A MySQL row (ADR 0018). `mysql_async::Row` owns its values, like SQLite's,
    /// so it is held directly rather than materialised into a side type.
    #[cfg(feature = "mysql")]
    MySql(mysql_async::Row),
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
            #[cfg(feature = "mysql")]
            Row::MySql(row) => T::from_mysql(mysql_cell(row, idx)),
        }
    }
}

/// The MySQL cell at `idx`, or `NULL` if the row has no such column — a missing
/// cell is a `NULL`, never a panic, as on SQLite.
#[cfg(feature = "mysql")]
pub(crate) fn mysql_cell(row: &mysql_async::Row, idx: usize) -> &mysql_async::Value {
    row.as_ref(idx).unwrap_or(&mysql_async::Value::NULL)
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

    /// Decode from one MySQL cell (ADR 0018).
    #[cfg(feature = "mysql")]
    fn from_mysql(cell: &mysql_async::Value) -> Result<Self>;
}

/// Impl [`Decode`] for a scalar and its `Option`. `$from_sqlite`/`$from_mysql`
/// turn a non-NULL cell of each backend into the value; a NULL is an error for
/// the scalar and `None` for the `Option`, which is exactly PostgreSQL's
/// `try_get::<_, T>` vs `try_get::<_, Option<T>>` split.
macro_rules! impl_decode {
    ($t:ty, $from_sqlite:expr, $from_mysql:expr) => {
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
                    other => ($from_sqlite)(other),
                }
            }

            #[cfg(feature = "mysql")]
            fn from_mysql(cell: &mysql_async::Value) -> Result<Self> {
                match cell {
                    mysql_async::Value::NULL => Err(Error::Db(
                        concat!("unexpected NULL reading a non-null ", stringify!($t)).into(),
                    )),
                    other => ($from_mysql)(other),
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
                    other => ($from_sqlite)(other).map(Some),
                }
            }

            #[cfg(feature = "mysql")]
            fn from_mysql(cell: &mysql_async::Value) -> Result<Self> {
                match cell {
                    mysql_async::Value::NULL => Ok(None),
                    other => ($from_mysql)(other).map(Some),
                }
            }
        }
    };
}

// The closures are compiled only under their feature; with it off they are dead
// tokens inside the cfg'd `from_sqlite`/`from_mysql` body. Non-scalar types
// (Decimal, timestamp, uuid, json) travel as text on both SQLite and MySQL, so a
// value written by one reads back identically.
impl_decode!(i32, |c| sqlite_int(c).map(|n| n as i32), |c| mysql_int(c)
    .map(|n| n as i32));
impl_decode!(i64, sqlite_int, mysql_int);
impl_decode!(bool, |c| sqlite_int(c).map(|n| n != 0), |c| mysql_int(c)
    .map(|n| n != 0));
// `String` is hand-written rather than `impl_decode!`'d: a native enum column
// decodes to a `String`, but `String`'s own `FromSql` rejects the custom enum
// type, so `from_pg` falls back to reading the label bytes (ADR 0046). SQLite and
// MySQL store the enum as `TEXT`, so their paths are the plain text decode.
impl Decode for String {
    fn from_pg(row: &tokio_postgres::Row, idx: usize) -> Result<Self> {
        match row.try_get::<usize, String>(idx) {
            Ok(s) => Ok(s),
            // A `Kind::Enum` column: read its label directly.
            Err(_) => row
                .try_get::<usize, PgEnumLabel>(idx)
                .map(|l| l.0)
                .map_err(|e| Error::Db(e.to_string())),
        }
    }

    #[cfg(feature = "sqlite")]
    fn from_sqlite(cell: &rusqlite::types::Value) -> Result<Self> {
        match cell {
            rusqlite::types::Value::Null => Err(Error::Db(
                "unexpected NULL reading a non-null String".into(),
            )),
            other => sqlite_text(other),
        }
    }

    #[cfg(feature = "mysql")]
    fn from_mysql(cell: &mysql_async::Value) -> Result<Self> {
        match cell {
            mysql_async::Value::NULL => Err(Error::Db(
                "unexpected NULL reading a non-null String".into(),
            )),
            other => mysql_text(other),
        }
    }
}

impl Decode for Option<String> {
    fn from_pg(row: &tokio_postgres::Row, idx: usize) -> Result<Self> {
        match row.try_get::<usize, Option<String>>(idx) {
            Ok(s) => Ok(s),
            Err(_) => row
                .try_get::<usize, Option<PgEnumLabel>>(idx)
                .map(|o| o.map(|l| l.0))
                .map_err(|e| Error::Db(e.to_string())),
        }
    }

    #[cfg(feature = "sqlite")]
    fn from_sqlite(cell: &rusqlite::types::Value) -> Result<Self> {
        match cell {
            rusqlite::types::Value::Null => Ok(None),
            other => sqlite_text(other).map(Some),
        }
    }

    #[cfg(feature = "mysql")]
    fn from_mysql(cell: &mysql_async::Value) -> Result<Self> {
        match cell {
            mysql_async::Value::NULL => Ok(None),
            other => mysql_text(other).map(Some),
        }
    }
}
impl_decode!(
    rust_decimal::Decimal,
    |c| {
        use std::str::FromStr as _;
        rust_decimal::Decimal::from_str(&sqlite_text(c)?)
            .map_err(|_| Error::Db("not a decimal".into()))
    },
    |c| {
        use std::str::FromStr as _;
        rust_decimal::Decimal::from_str(&mysql_text(c)?)
            .map_err(|_| Error::Db("not a decimal".into()))
    }
);
impl_decode!(
    chrono::DateTime<chrono::Utc>,
    |c| sqlite_timestamp(&sqlite_text(c)?),
    mysql_timestamp
);
impl_decode!(
    uuid::Uuid,
    |c| sqlite_text(c)?
        .parse()
        .map_err(|_| Error::Db("not a uuid".into())),
    |c| mysql_text(c)?
        .parse()
        .map_err(|_| Error::Db("not a uuid".into()))
);
impl_decode!(
    serde_json::Value,
    |c| serde_json::from_str(&sqlite_text(c)?).map_err(|_| Error::Db("not json".into())),
    |c| serde_json::from_str(&mysql_text(c)?).map_err(|_| Error::Db("not json".into()))
);
impl_decode!(f64, sqlite_real, mysql_double);
impl_decode!(Vec<u8>, sqlite_blob, mysql_blob);
// `INET` → `std::net::IpAddr`, native on PostgreSQL (ADR 0040). SQLite and MySQL
// have no INET type, so a typed read there is refused rather than silently
// reinterpreting the stored text — the same explicit refusal `cell_to_value` makes.
impl_decode!(
    Vec<String>,
    |_c| Err(Error::Db(
        "text[] is a PostgreSQL array; SQLite does not support it".into()
    )),
    |_c| Err(Error::Db(
        "text[] is a PostgreSQL array; MySQL does not support it".into()
    ))
);
impl_decode!(
    Vec<i64>,
    |_c| Err(Error::Db(
        "bigint[] is a PostgreSQL array; SQLite does not support it".into()
    )),
    |_c| Err(Error::Db(
        "bigint[] is a PostgreSQL array; MySQL does not support it".into()
    ))
);
impl_decode!(
    std::net::IpAddr,
    |_c| Err(Error::Db(
        "inet is a PostgreSQL-only type; SQLite does not support it".into()
    )),
    |_c| Err(Error::Db(
        "inet is a PostgreSQL-only type; MySQL does not support it".into()
    ))
);

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

/// A SQLite real cell, or a type error. An integer is accepted and widened — a
/// whole number stored in a `REAL` column comes back as `Integer` from SQLite.
#[cfg(feature = "sqlite")]
fn sqlite_real(cell: &rusqlite::types::Value) -> Result<f64> {
    match cell {
        rusqlite::types::Value::Real(x) => Ok(*x),
        rusqlite::types::Value::Integer(n) => Ok(*n as f64),
        _ => Err(Error::Db("expected a real".into())),
    }
}

/// A SQLite blob cell, or a type error.
#[cfg(feature = "sqlite")]
fn sqlite_blob(cell: &rusqlite::types::Value) -> Result<Vec<u8>> {
    match cell {
        rusqlite::types::Value::Blob(b) => Ok(b.clone()),
        _ => Err(Error::Db("expected a blob".into())),
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
#[cfg(any(feature = "sqlite", feature = "mysql"))]
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
        Value::F64(x) => S::Real(*x),
        Value::Text(s) => S::Text(s.clone()),
        Value::Decimal(d) => S::Text(d.to_string()),
        Value::Timestamp(t) => S::Text(t.to_rfc3339()),
        Value::Uuid(u) => S::Text(u.to_string()),
        Value::Json(j) => S::Text(j.to_string()),
        Value::Bytes(b) => S::Blob(b.clone()),
        // SQLite has no INET; stored as text if it ever reaches here (the DDL
        // refuses an inet column on SQLite — ADR 0040).
        Value::Inet(ip) => S::Text(ip.to_string()),
        Value::TextArray(v) => S::Text(format!("{{{}}}", v.join(","))),
        Value::IntArray(v) => S::Text(format!(
            "{{{}}}",
            v.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        )),
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
        FieldType::F64 => Value::F64(sqlite_real(cell).map_err(|_| mismatch("float"))?),
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
        FieldType::Bytes => Value::Bytes(sqlite_blob(cell).map_err(|_| mismatch("bytes"))?),
        FieldType::Inet => return Err(mismatch("inet, which SQLite does not support")),
        FieldType::TextArray | FieldType::IntArray => {
            return Err(mismatch("arrays, which SQLite does not support"))
        }
        // An enum is a plain TEXT column here (ADR 0046); decode the label.
        FieldType::Enum(_) => Value::Text(sqlite_text(cell).map_err(|_| mismatch("enum text"))?),
    })
}

// ----------------------------------------------------------------- MySQL (ADR 0018)
//
// The non-scalar types travel as text/native values, mirroring SQLite: Decimal,
// Uuid and Json as UTF-8 bytes; a Timestamp as MySQL's native `Date` value (an
// RFC-3339 string would be rejected by a `DATETIME` column). The session is fixed
// to UTC (`time_zone='+00:00'`), so a `DATETIME` round-trips a `DateTime<Utc>`.

/// A MySQL integer cell, or a type error. `TINYINT(1)` (a boolean) also arrives here.
#[cfg(feature = "mysql")]
fn mysql_int(cell: &mysql_async::Value) -> Result<i64> {
    match cell {
        mysql_async::Value::Int(n) => Ok(*n),
        mysql_async::Value::UInt(n) => Ok(*n as i64),
        _ => Err(Error::Db("expected an integer".into())),
    }
}

/// A MySQL floating cell, or a type error. An integer widens.
#[cfg(feature = "mysql")]
fn mysql_double(cell: &mysql_async::Value) -> Result<f64> {
    match cell {
        mysql_async::Value::Double(x) => Ok(*x),
        mysql_async::Value::Float(x) => Ok(f64::from(*x)),
        mysql_async::Value::Int(n) => Ok(*n as f64),
        mysql_async::Value::UInt(n) => Ok(*n as f64),
        _ => Err(Error::Db("expected a real".into())),
    }
}

/// A MySQL text cell (returned as bytes), decoded as UTF-8.
#[cfg(feature = "mysql")]
fn mysql_text(cell: &mysql_async::Value) -> Result<String> {
    match cell {
        mysql_async::Value::Bytes(b) => {
            String::from_utf8(b.clone()).map_err(|_| Error::Db("not valid UTF-8".into()))
        }
        _ => Err(Error::Db("expected text".into())),
    }
}

/// A MySQL blob cell.
#[cfg(feature = "mysql")]
fn mysql_blob(cell: &mysql_async::Value) -> Result<Vec<u8>> {
    match cell {
        mysql_async::Value::Bytes(b) => Ok(b.clone()),
        _ => Err(Error::Db("expected a blob".into())),
    }
}

/// A MySQL `DATETIME` cell as a UTC timestamp — the native `Date` value, or a
/// text form (if the server sent it as a string), both read as UTC.
#[cfg(feature = "mysql")]
fn mysql_timestamp(cell: &mysql_async::Value) -> Result<chrono::DateTime<chrono::Utc>> {
    use chrono::{NaiveDate, TimeZone, Utc};
    match cell {
        mysql_async::Value::Date(y, mo, d, h, mi, s, micro) => {
            let naive = NaiveDate::from_ymd_opt(i32::from(*y), u32::from(*mo), u32::from(*d))
                .and_then(|date| {
                    date.and_hms_micro_opt(u32::from(*h), u32::from(*mi), u32::from(*s), *micro)
                })
                .ok_or_else(|| Error::Db("invalid datetime".into()))?;
            Ok(Utc.from_utc_datetime(&naive))
        }
        mysql_async::Value::Bytes(_) => sqlite_timestamp(&mysql_text(cell)?),
        _ => Err(Error::Db("expected a datetime".into())),
    }
}

/// Bind one ORM [`Value`](crate::value::Value) as a MySQL parameter (ADR 0018).
/// The counterpart of the `mysql_*` decoders, so a value round-trips the same way.
#[cfg(feature = "mysql")]
pub(crate) fn value_to_mysql(v: &crate::value::Value) -> mysql_async::Value {
    use crate::value::Value;
    use chrono::{Datelike, Timelike};
    use mysql_async::Value as M;
    match v {
        Value::Null => M::NULL,
        Value::Bool(b) => M::Int(i64::from(*b)),
        Value::I32(n) => M::Int(i64::from(*n)),
        Value::I64(n) => M::Int(*n),
        Value::F64(x) => M::Double(*x),
        Value::Text(s) => M::Bytes(s.clone().into_bytes()),
        Value::Decimal(d) => M::Bytes(d.to_string().into_bytes()),
        Value::Timestamp(t) => {
            let t = t.naive_utc();
            M::Date(
                t.year() as u16,
                t.month() as u8,
                t.day() as u8,
                t.hour() as u8,
                t.minute() as u8,
                t.second() as u8,
                t.nanosecond() / 1_000,
            )
        }
        Value::Uuid(u) => M::Bytes(u.to_string().into_bytes()),
        Value::Json(j) => M::Bytes(j.to_string().into_bytes()),
        Value::Bytes(b) => M::Bytes(b.clone()),
        Value::Inet(ip) => M::Bytes(ip.to_string().into_bytes()),
        Value::TextArray(v) => M::Bytes(format!("{{{}}}", v.join(",")).into_bytes()),
        Value::IntArray(v) => M::Bytes(
            format!(
                "{{{}}}",
                v.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            )
            .into_bytes(),
        ),
    }
}

/// Decode one MySQL cell into the [`Value`](crate::value::Value) its declared
/// [`FieldType`](crate::value::FieldType) expects — the dynamic (model-erased)
/// path [`crate::value::read_column`] takes for a MySQL row.
#[cfg(feature = "mysql")]
pub(crate) fn mysql_cell_to_value(
    cell: &mysql_async::Value,
    ty: crate::value::FieldType,
    column: &str,
) -> Result<crate::value::Value> {
    use crate::value::{FieldType, Value};
    if matches!(cell, mysql_async::Value::NULL) {
        return Ok(Value::Null);
    }
    let mismatch = |want: &'static str| Error::UnexpectedColumnType {
        column: column.to_string(),
        want,
    };
    Ok(match ty {
        FieldType::I32 => Value::I32(mysql_int(cell).map_err(|_| mismatch("integer"))? as i32),
        FieldType::I64 => Value::I64(mysql_int(cell).map_err(|_| mismatch("bigint"))?),
        FieldType::F64 => Value::F64(mysql_double(cell).map_err(|_| mismatch("float"))?),
        FieldType::Bool => Value::Bool(mysql_int(cell).map_err(|_| mismatch("boolean"))? != 0),
        FieldType::Text => Value::Text(mysql_text(cell).map_err(|_| mismatch("text"))?),
        FieldType::Decimal => {
            use std::str::FromStr as _;
            Value::Decimal(
                rust_decimal::Decimal::from_str(
                    &mysql_text(cell).map_err(|_| mismatch("decimal text"))?,
                )
                .map_err(|_| mismatch("decimal"))?,
            )
        }
        FieldType::Timestamp => {
            Value::Timestamp(mysql_timestamp(cell).map_err(|_| mismatch("timestamp"))?)
        }
        FieldType::Uuid => Value::Uuid(
            mysql_text(cell)
                .map_err(|_| mismatch("uuid text"))?
                .parse()
                .map_err(|_| mismatch("uuid"))?,
        ),
        FieldType::Json => Value::Json(
            serde_json::from_str(&mysql_text(cell).map_err(|_| mismatch("json text"))?)
                .map_err(|_| mismatch("json"))?,
        ),
        FieldType::Bytes => Value::Bytes(mysql_blob(cell).map_err(|_| mismatch("bytes"))?),
        FieldType::Inet => return Err(mismatch("inet, which MySQL does not support")),
        FieldType::TextArray | FieldType::IntArray => {
            return Err(mismatch("arrays, which MySQL does not support"))
        }
        FieldType::Enum(_) => Value::Text(mysql_text(cell).map_err(|_| mismatch("enum text"))?),
    })
}
