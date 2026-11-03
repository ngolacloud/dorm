//! Converting between the ORM's dynamic [`Value`] and JSON, for tool arguments
//! and results. No identifier or value ever becomes SQL here — these feed the
//! dynamic query path, which binds every value as a parameter.

use dorm_core::value::Value;
use serde_json::Value as Json;

/// A JSON argument → a bound [`Value`]. An integer JSON number becomes `I64`, a
/// fractional one `F64`; the rest map by shape. The ORM coerces towards the
/// column's declared type when it compiles, so a string `"5"` against an integer
/// column still works.
pub fn json_to_value(j: &Json) -> Value {
    match j {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::I64(i)
            } else {
                Value::F64(n.as_f64().unwrap_or(0.0))
            }
        }
        Json::String(s) => Value::Text(s.clone()),
        // An array or object filter value is passed as JSON (a `jsonb` column), or
        // stringified for anything else — the compiler validates the column.
        other => Value::Json(other.clone()),
    }
}

/// A [`Value`] from a result row → JSON, for a tool's response.
pub fn value_to_json(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::I32(n) => Json::from(*n),
        Value::I64(n) => Json::from(*n),
        Value::F64(x) => Json::from(*x),
        Value::Text(s) => Json::String(s.clone()),
        Value::Decimal(d) => Json::String(d.to_string()),
        Value::Timestamp(t) => Json::String(t.to_rfc3339()),
        Value::Uuid(u) => Json::String(u.to_string()),
        Value::Json(j) => j.clone(),
        // Bytes as `\x`-hex, the same unambiguous rendering the REPL uses.
        Value::Bytes(b) => {
            let mut s = String::from("\\x");
            for byte in b {
                s.push_str(&format!("{byte:02x}"));
            }
            Json::String(s)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_split_into_integer_and_float() {
        assert_eq!(json_to_value(&serde_json::json!(5)), Value::I64(5));
        assert_eq!(json_to_value(&serde_json::json!(1.5)), Value::F64(1.5));
        assert_eq!(
            json_to_value(&serde_json::json!("open")),
            Value::Text("open".into())
        );
    }

    #[test]
    fn values_round_trip_to_json_shapes() {
        assert_eq!(value_to_json(&Value::I64(7)), serde_json::json!(7));
        assert_eq!(value_to_json(&Value::Null), Json::Null);
        assert_eq!(
            value_to_json(&Value::Text("x".into())),
            serde_json::json!("x")
        );
        assert_eq!(
            value_to_json(&Value::Bytes(vec![0x0f, 0xff])),
            serde_json::json!("\\x0fff")
        );
    }
}
