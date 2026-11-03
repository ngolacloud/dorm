//! Schema-level validation — the buildless, database-less half of the ADR 0019
//! checks, plus a home for any future `dorm.schema` lint.
//!
//! This runs at `dorm generate`/`dorm validate` time over the parsed [`Schema`],
//! so a mistake in a declaration (a `@@rag` field that is not `String`, a typo in
//! a metadata column, a model with no key) is caught before any Rust is emitted
//! or any project is built. It is pure — `Schema` in, findings out — which is why
//! it lives in `dorm-schema` and owns its own [`Issue`]/[`Severity`] rather than
//! borrowing `dorm-core`'s (this crate deliberately has no dependency on the ORM
//! core).

use crate::{Field, Scalar, Schema};

/// How serious a [`validate()`] finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// The schema will not work as written — generation or a later query fails.
    Error,
    /// Legal, but likely a mistake worth naming.
    Warning,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

/// One thing [`validate()`] found wrong with the schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub severity: Severity,
    /// The model the issue is about, or empty for a schema-wide finding.
    pub model: String,
    pub message: String,
}

impl Issue {
    fn error(model: &str, message: impl Into<String>) -> Issue {
        Issue {
            severity: Severity::Error,
            model: model.to_string(),
            message: message.into(),
        }
    }
}

/// Check a parsed [`Schema`]. An empty result means it is internally consistent.
///
/// Today it checks the `@@rag(..)` declarations (ADR 0019); the shape is ready
/// for more lints. For each RAG model: every `fields` entry names a declared
/// `String` scalar (only text can be embedded), every `metadata` entry names a
/// declared column, the model has a primary key, `chunk_overlap` is below
/// `chunk_size`, and any `distance` is a known metric.
pub fn validate(schema: &Schema) -> Vec<Issue> {
    let mut issues = Vec::new();
    for m in &schema.models {
        let Some(rag) = &m.rag else { continue };

        if m.primary_key().is_none() {
            issues.push(Issue::error(
                &m.name,
                "has `@@rag` but no primary key (`@id`), so a chunk cannot be tied to its row",
            ));
        }

        if rag.fields.is_empty() {
            issues.push(Issue::error(
                &m.name,
                "`@@rag(fields: [])` is empty — RAG needs at least one text field to embed",
            ));
        }

        for field in &rag.fields {
            match scalar(m, field) {
                None => issues.push(Issue::error(
                    &m.name,
                    format!("`@@rag` field `{field}` is not a scalar field of the model"),
                )),
                Some(s) if s.ty != Scalar::String => issues.push(Issue::error(
                    &m.name,
                    format!(
                        "`@@rag` field `{field}` is `{:?}`, but only `String` fields can be embedded",
                        s.ty
                    ),
                )),
                Some(_) => {}
            }
        }

        for field in &rag.metadata {
            if !has_column(m, field) {
                issues.push(Issue::error(
                    &m.name,
                    format!("`@@rag` metadata field `{field}` is not a column of the model"),
                ));
            }
        }

        if let (Some(size), Some(overlap)) = (rag.chunk_size, rag.chunk_overlap) {
            if overlap >= size {
                issues.push(Issue::error(
                    &m.name,
                    format!(
                        "`@@rag` chunk_overlap ({overlap}) must be smaller than chunk_size ({size})"
                    ),
                ));
            }
        }

        if let Some(d) = &rag.distance {
            if !matches!(
                d.as_str(),
                "cosine" | "l2" | "euclidean" | "inner_product" | "ip" | "dot"
            ) {
                issues.push(Issue::error(
                    &m.name,
                    format!(
                        "`@@rag` distance `{d}` is unknown; use cosine, l2 (euclidean) or \
                         inner_product (ip, dot)"
                    ),
                ));
            }
        }
    }
    issues
}

/// The scalar field of `m` named `name`, if any.
fn scalar<'a>(m: &'a crate::Model, name: &str) -> Option<&'a crate::ScalarField> {
    m.fields.iter().find_map(|f| match f {
        Field::Scalar(s) if s.name == name => Some(s),
        _ => None,
    })
}

/// Whether `m` has a column named `name` — a scalar field or a to-one relation
/// (whose foreign key is a column); a to-many back-reference is not a column.
fn has_column(m: &crate::Model, name: &str) -> bool {
    m.fields.iter().any(|f| match f {
        Field::Scalar(s) => s.name == name,
        Field::ToOne(r) => r.name == name,
        Field::ToMany { .. } => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    const WITH_RAG: &str = r#"
        model Ticket {
          id     Int    @id @default(autoincrement())
          title  String @db.VarChar(200)
          body   String @db.Text
          status String @db.VarChar(32)
          @@rag(fields: [title, body], embedder: "default", metadata: [status])
        }
    "#;

    #[test]
    fn a_well_formed_rag_model_validates_clean() {
        let schema = parse(WITH_RAG).expect("parses");
        assert!(validate(&schema).is_empty(), "{:?}", validate(&schema));
    }

    #[test]
    fn a_non_string_rag_field_is_rejected() {
        let src = r#"
            model Ticket {
              id       Int    @id @default(autoincrement())
              title    String
              priority Int
              @@rag(fields: [title, priority])
            }
        "#;
        let issues = validate(&parse(src).expect("parses"));
        assert!(issues
            .iter()
            .any(|i| i.message.contains("only `String` fields can be embedded")));
    }

    #[test]
    fn a_missing_field_a_missing_metadata_column_and_no_key_are_all_named() {
        let src = r#"
            model Ticket {
              title String
              @@rag(fields: [titel], metadata: [staus])
            }
        "#;
        let issues = validate(&parse(src).expect("parses"));
        assert!(issues.iter().any(|i| i.message.contains("`titel`")));
        assert!(issues.iter().any(|i| i.message.contains("`staus`")));
        assert!(issues.iter().any(|i| i.message.contains("no primary key")));
    }

    #[test]
    fn an_unknown_distance_and_a_bad_overlap_are_rejected() {
        let src = r#"
            model Ticket {
              id    Int    @id @default(autoincrement())
              title String
              @@rag(fields: [title], distance: "cosone", chunk_size: 100, chunk_overlap: 200)
            }
        "#;
        let issues = validate(&parse(src).expect("parses"));
        assert!(issues
            .iter()
            .any(|i| i.message.contains("distance `cosone`")));
        assert!(issues
            .iter()
            .any(|i| i.message.contains("must be smaller than chunk_size")));
    }

    #[test]
    fn a_schema_without_rag_has_nothing_to_say() {
        let src = r#"
            model Tenant {
              id   Int    @id @default(autoincrement())
              name String
            }
        "#;
        assert!(validate(&parse(src).expect("parses")).is_empty());
    }
}
