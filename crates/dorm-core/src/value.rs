//! The dynamic value, and the bridge from Rust types to SQL parameters.
//!
//! Every value the ORM binds to a statement passes through [`Value`]. That is
//! what makes one compiler serve two front ends: the typed API converts a
//! concrete Rust type into a `Value` on the way in, the REPL converts a Rhai
//! dynamic into the same `Value`, and from there the two are indistinguishable.
//!
//! Nothing here ever reaches SQL as text. `Value` implements `ToSql` and travels
//! as a bound parameter, which is the only reason it is safe for the REPL to
//! accept a value typed by a human.

use std::fmt;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use tokio_postgres::types::{to_sql_checked, IsNull, ToSql, Type};
use uuid::Uuid;

use crate::error::{Error, Result};

/// The column types the ORM knows how to store and read back.
///
/// This is deliberately a closed set. An open one (arbitrary `ToSql`) would make
/// the migration autodetector unable to name a column's type, and would let the
/// REPL ask for a column the compiler cannot describe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FieldType {
    I32,
    I64,
    Text,
    Bool,
    Decimal,
    Timestamp,
    Uuid,
    Json,
}

impl FieldType {
    /// The PostgreSQL type this field is created as.
    pub fn sql_type(self) -> &'static str {
        match self {
            FieldType::I32 => "INTEGER",
            FieldType::I64 => "BIGINT",
            FieldType::Text => "TEXT",
            FieldType::Bool => "BOOLEAN",
            // NUMERIC without precision, on purpose: money columns get their
            // precision from `max_length`-style attributes when they need it,
            // and an unqualified NUMERIC never silently rounds.
            FieldType::Decimal => "NUMERIC",
            FieldType::Timestamp => "TIMESTAMPTZ",
            FieldType::Uuid => "UUID",
            FieldType::Json => "JSONB",
        }
    }

    /// Whether a text-shaped lookup (`contains`, `startswith`, …) makes sense.
    /// Used by the dynamic path, where the lookup arrives as a string and there
    /// is no type checker to stop `replicas__icontains`.
    pub fn is_textual(self) -> bool {
        matches!(self, FieldType::Text)
    }

    /// A human name, for error messages.
    pub fn describe(self) -> &'static str {
        match self {
            FieldType::I32 => "integer",
            FieldType::I64 => "bigint",
            FieldType::Text => "text",
            FieldType::Bool => "boolean",
            FieldType::Decimal => "decimal",
            FieldType::Timestamp => "timestamp",
            FieldType::Uuid => "uuid",
            FieldType::Json => "json",
        }
    }
}

/// A single bound value.
///
/// There is no `Array` variant: the `in` lookup expands to `IN ($1, $2, $3)`
/// rather than `= ANY($1)`. Expanding costs one placeholder per element but
/// keeps every parameter a scalar, which means no array type has to be inferred
/// server-side for a list the REPL just built out of mixed Rhai values.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    I32(i32),
    I64(i64),
    Text(String),
    Decimal(Decimal),
    Timestamp(DateTime<Utc>),
    Uuid(Uuid),
    Json(serde_json::Value),
}

impl Value {
    /// The field type this value would satisfy, or `None` for `Null`, which
    /// satisfies any nullable field.
    pub fn field_type(&self) -> Option<FieldType> {
        Some(match self {
            Value::Null => return None,
            Value::Bool(_) => FieldType::Bool,
            Value::I32(_) => FieldType::I32,
            Value::I64(_) => FieldType::I64,
            Value::Text(_) => FieldType::Text,
            Value::Decimal(_) => FieldType::Decimal,
            Value::Timestamp(_) => FieldType::Timestamp,
            Value::Uuid(_) => FieldType::Uuid,
            Value::Json(_) => FieldType::Json,
        })
    }

    /// Whether this is SQL `NULL`.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Read this value back as text, for the `contains`/`startswith` family,
    /// which has to wrap it in `%`.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }

    /// Coerce towards a declared field type, so that a REPL literal `1` lands in
    /// an `i64` column and a `"..."` lands in a `Uuid` one.
    ///
    /// Only widening and parsing that cannot lose information is allowed: an
    /// `i64` that does not fit an `i32` is an error, not a truncation.
    pub fn coerce(self, want: FieldType, column: &str) -> Result<Value> {
        let mismatch = |want: &'static str| Error::UnexpectedColumnType {
            column: column.to_string(),
            want,
        };
        Ok(match (self, want) {
            (Value::Null, _) => Value::Null,
            (v, w) if v.field_type() == Some(w) => v,

            // Integers: widen freely, narrow only when it fits.
            (Value::I32(n), FieldType::I64) => Value::I64(i64::from(n)),
            (Value::I64(n), FieldType::I32) => {
                Value::I32(i32::try_from(n).map_err(|_| mismatch("integer"))?)
            }
            (Value::I32(n), FieldType::Decimal) => Value::Decimal(Decimal::from(n)),
            (Value::I64(n), FieldType::Decimal) => Value::Decimal(Decimal::from(n)),

            // Text into the types whose canonical form is text.
            (Value::Text(s), FieldType::Uuid) => {
                Value::Uuid(s.parse().map_err(|_| mismatch("uuid"))?)
            }
            (Value::Text(s), FieldType::Decimal) => {
                Value::Decimal(s.parse().map_err(|_| mismatch("decimal"))?)
            }
            (Value::Text(s), FieldType::Timestamp) => Value::Timestamp(
                DateTime::parse_from_rfc3339(&s)
                    .map_err(|_| mismatch("RFC 3339 timestamp"))?
                    .with_timezone(&Utc),
            ),
            (Value::Text(s), FieldType::Json) => {
                Value::Json(serde_json::from_str(&s).map_err(|_| mismatch("json"))?)
            }

            (_, w) => return Err(mismatch(w.describe())),
        })
    }
}

impl fmt::Display for Value {
    /// How a value is rendered in the REPL's table output. `Null` shows as a
    /// dash rather than an empty cell, because an empty cell and an empty string
    /// are different answers and the operator needs to tell them apart.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("-"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::I32(n) => write!(f, "{n}"),
            Value::I64(n) => write!(f, "{n}"),
            Value::Text(s) => f.write_str(s),
            Value::Decimal(d) => write!(f, "{d}"),
            Value::Timestamp(t) => write!(f, "{}", t.to_rfc3339()),
            Value::Uuid(u) => write!(f, "{u}"),
            Value::Json(j) => write!(f, "{j}"),
        }
    }
}

impl ToSql for Value {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut tokio_postgres::types::private::BytesMut,
    ) -> std::result::Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        match self {
            // No type is named: the server infers it from where the placeholder
            // sits. Every path that could produce an untyped NULL in a bare
            // comparison compiles to `IS NULL` instead, so this stays unambiguous.
            Value::Null => Ok(IsNull::Yes),
            Value::Bool(v) => v.to_sql(ty, out),
            Value::I32(v) => v.to_sql(ty, out),
            Value::I64(v) => v.to_sql(ty, out),
            Value::Text(v) => v.to_sql(ty, out),
            Value::Decimal(v) => v.to_sql(ty, out),
            Value::Timestamp(v) => v.to_sql(ty, out),
            Value::Uuid(v) => v.to_sql(ty, out),
            Value::Json(v) => v.to_sql(ty, out),
        }
    }

    fn accepts(_ty: &Type) -> bool {
        // The variant decides, not the declared column type: `accepts` runs
        // before the variant is known. A genuine mismatch still fails, inside
        // `to_sql` above, with the driver's own message.
        true
    }

    to_sql_checked!();
}

/// Anything the typed API can bind as a parameter.
///
/// Deliberately separate from [`ColumnType`]: `Value` itself can be bound but
/// has no single column type to declare, and folding the two together would
/// force a meaningless answer out of it.
pub trait ToValue {
    fn to_value(&self) -> Value;
}

/// A Rust type that names the column type it maps to.
///
/// This is what lets the derive macro fill in a field's `FieldType` from the
/// struct field's declared type, instead of making the author restate it in an
/// attribute where the two could drift apart.
pub trait ColumnType {
    fn field_type() -> FieldType;
}

/// Binding a `Value` is the identity. The dynamic front end has already done
/// the conversion, and this impl is what lets it share every typed entry point.
impl ToValue for Value {
    fn to_value(&self) -> Value {
        self.clone()
    }
}

macro_rules! to_value {
    ($t:ty, $variant:ident, $ft:ident, |$v:ident| $conv:expr) => {
        impl ToValue for $t {
            fn to_value(&self) -> Value {
                let $v = self;
                Value::$variant($conv)
            }
        }
        impl ColumnType for $t {
            fn field_type() -> FieldType {
                FieldType::$ft
            }
        }
    };
}

to_value!(bool, Bool, Bool, |v| *v);
to_value!(i32, I32, I32, |v| *v);
to_value!(i64, I64, I64, |v| *v);
to_value!(String, Text, Text, |v| v.clone());
to_value!(Decimal, Decimal, Decimal, |v| *v);
to_value!(DateTime<Utc>, Timestamp, Timestamp, |v| *v);
to_value!(Uuid, Uuid, Uuid, |v| *v);
to_value!(serde_json::Value, Json, Json, |v| v.clone());

impl ToValue for &str {
    fn to_value(&self) -> Value {
        Value::Text((*self).to_string())
    }
}

impl ColumnType for &str {
    fn field_type() -> FieldType {
        FieldType::Text
    }
}

/// `Option<T>` is how a nullable column is written in the typed API; `None`
/// becomes SQL `NULL`.
impl<T: ToValue> ToValue for Option<T> {
    fn to_value(&self) -> Value {
        match self {
            Some(v) => v.to_value(),
            None => Value::Null,
        }
    }
}

/// A nullable column's type is the inner one; the nullability is recorded
/// separately in [`crate::meta::FieldMeta`].
impl<T: ColumnType> ColumnType for Option<T> {
    fn field_type() -> FieldType {
        T::field_type()
    }
}

/// Read one column out of a row as a dynamic [`Value`].
///
/// The dynamic path needs this because it has no Rust type to ask `FromSql` for;
/// the field metadata is the only thing that says what the column holds.
pub fn read_column(
    row: &tokio_postgres::Row,
    idx: usize,
    ty: FieldType,
    column: &str,
) -> Result<Value> {
    let fail = |want: &'static str| Error::UnexpectedColumnType {
        column: column.to_string(),
        want,
    };
    // Every read goes through `Option<T>`: a column declared NOT NULL can still
    // arrive NULL from a LEFT JOIN or an aggregate over no rows, and panicking
    // on that would take the whole REPL down.
    Ok(match ty {
        FieldType::I32 => row
            .try_get::<_, Option<i32>>(idx)
            .map_err(|_| fail("integer"))?
            .map_or(Value::Null, Value::I32),
        FieldType::I64 => row
            .try_get::<_, Option<i64>>(idx)
            .map_err(|_| fail("bigint"))?
            .map_or(Value::Null, Value::I64),
        FieldType::Text => row
            .try_get::<_, Option<String>>(idx)
            .map_err(|_| fail("text"))?
            .map_or(Value::Null, Value::Text),
        FieldType::Bool => row
            .try_get::<_, Option<bool>>(idx)
            .map_err(|_| fail("boolean"))?
            .map_or(Value::Null, Value::Bool),
        FieldType::Decimal => row
            .try_get::<_, Option<Decimal>>(idx)
            .map_err(|_| fail("decimal"))?
            .map_or(Value::Null, Value::Decimal),
        FieldType::Timestamp => row
            .try_get::<_, Option<DateTime<Utc>>>(idx)
            .map_err(|_| fail("timestamp"))?
            .map_or(Value::Null, Value::Timestamp),
        FieldType::Uuid => row
            .try_get::<_, Option<Uuid>>(idx)
            .map_err(|_| fail("uuid"))?
            .map_or(Value::Null, Value::Uuid),
        FieldType::Json => row
            .try_get::<_, Option<serde_json::Value>>(idx)
            .map_err(|_| fail("json"))?
            .map_or(Value::Null, Value::Json),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widening_an_integer_is_allowed_and_narrowing_only_when_it_fits() {
        assert_eq!(
            Value::I32(7).coerce(FieldType::I64, "n").unwrap(),
            Value::I64(7)
        );
        assert_eq!(
            Value::I64(7).coerce(FieldType::I32, "n").unwrap(),
            Value::I32(7)
        );
        // This is the case that must not silently truncate.
        assert!(Value::I64(i64::from(i32::MAX) + 1)
            .coerce(FieldType::I32, "n")
            .is_err());
    }

    #[test]
    fn text_parses_into_the_types_whose_canonical_form_is_text() {
        let u = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";
        assert_eq!(
            Value::Text(u.into()).coerce(FieldType::Uuid, "id").unwrap(),
            Value::Uuid(u.parse().unwrap())
        );
        assert!(Value::Text("not-a-uuid".into())
            .coerce(FieldType::Uuid, "id")
            .is_err());
    }

    #[test]
    fn a_decimal_from_text_keeps_its_cents() {
        // The whole reason `Decimal` is in the closed set rather than `f64`.
        let v = Value::Text("19.99".into())
            .coerce(FieldType::Decimal, "price")
            .unwrap();
        assert_eq!(v.to_string(), "19.99");
    }

    #[test]
    fn null_satisfies_any_field_type_and_renders_as_a_dash() {
        assert_eq!(
            Value::Null.coerce(FieldType::Uuid, "id").unwrap(),
            Value::Null
        );
        assert_eq!(Value::Null.to_string(), "-");
        // An empty string is a different answer and must look different.
        assert_eq!(Value::Text(String::new()).to_string(), "");
    }

    #[test]
    fn option_none_binds_as_null_and_keeps_the_inner_field_type() {
        let none: Option<i64> = None;
        assert_eq!(none.to_value(), Value::Null);
        assert_eq!(<Option<i64> as ColumnType>::field_type(), FieldType::I64);
    }

    #[test]
    fn only_text_accepts_textual_lookups() {
        assert!(FieldType::Text.is_textual());
        assert!(!FieldType::I64.is_textual());
        assert!(!FieldType::Json.is_textual());
    }
}
