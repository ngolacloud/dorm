//! JSON Schema derived from `FieldMeta` (ADR 0020), so a tool's `inputSchema`
//! and a model's description come from the same metadata the compiler reads —
//! never hand-written per model. This is what lets the server pass the MCP
//! Inspector's schema checks (acceptance #7).

use dorm_core::meta::{FieldMeta, ModelMeta};
use dorm_core::value::FieldType;
use serde_json::{json, Value};

/// The JSON Schema `type` (and `format`) for a field type.
fn json_type(ty: FieldType) -> Value {
    match ty {
        FieldType::I32 | FieldType::I64 => json!({ "type": "integer" }),
        FieldType::F64 => json!({ "type": "number" }),
        FieldType::Bool => json!({ "type": "boolean" }),
        FieldType::Decimal => json!({ "type": "string", "format": "decimal" }),
        FieldType::Timestamp => json!({ "type": "string", "format": "date-time" }),
        FieldType::Uuid => json!({ "type": "string", "format": "uuid" }),
        FieldType::Json => json!({ "type": "object" }),
        FieldType::Bytes => json!({ "type": "string", "contentEncoding": "base16" }),
        FieldType::Text => json!({ "type": "string" }),
        FieldType::Inet => json!({ "type": "string", "format": "ipv4" }),
        FieldType::TextArray => json!({ "type": "array", "items": { "type": "string" } }),
        FieldType::IntArray => json!({ "type": "array", "items": { "type": "integer" } }),
    }
}

/// One field as a JSON Schema property: its type, nullability and `maxLength`.
pub fn field_property(f: &FieldMeta) -> Value {
    let mut prop = json_type(f.ty);
    if let Some(obj) = prop.as_object_mut() {
        if f.nullable {
            // Allow null alongside the base type.
            let base = obj.remove("type").unwrap_or(json!("string"));
            obj.insert("type".to_string(), json!([base, "null"]));
        }
        if let Some(max) = f.max_length {
            if matches!(f.ty, FieldType::Text) {
                obj.insert("maxLength".to_string(), json!(max));
            }
        }
        let mut desc = format!("column `{}` ({})", f.column, f.ty.describe());
        if f.primary_key {
            desc.push_str("; primary key");
        }
        if f.unique {
            desc.push_str("; unique");
        }
        if let Some(rel) = f.relation {
            desc.push_str(&format!("; foreign key → {}", rel.model));
        }
        obj.insert("description".to_string(), json!(desc));
    }
    prop
}

/// A model's fields as a JSON Schema `object` (properties keyed by field name).
pub fn model_properties(meta: &ModelMeta) -> Value {
    let mut props = serde_json::Map::new();
    for f in meta.fields {
        props.insert(f.name.to_string(), field_property(f));
    }
    json!({ "type": "object", "properties": props })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &'static str, ty: FieldType, nullable: bool) -> FieldMeta {
        FieldMeta {
            name,
            column: name,
            ty,
            nullable,
            primary_key: false,
            unique: false,
            index: false,
            max_length: None,
            default: None,
            auto_now_add: false,
            auto_now: false,
            relation: None,
        }
    }

    #[test]
    fn a_text_field_with_max_length_maps_to_string_with_maxlength() {
        let f = FieldMeta {
            max_length: Some(200),
            ..field("title", FieldType::Text, false)
        };
        let p = field_property(&f);
        assert_eq!(p["type"], json!("string"));
        assert_eq!(p["maxLength"], json!(200));
    }

    #[test]
    fn a_nullable_integer_allows_null() {
        let p = field_property(&field("n", FieldType::I64, true));
        assert_eq!(p["type"], json!(["integer", "null"]));
    }

    #[test]
    fn a_timestamp_carries_the_date_time_format() {
        let p = field_property(&field("at", FieldType::Timestamp, false));
        assert_eq!(p["format"], json!("date-time"));
    }
}
