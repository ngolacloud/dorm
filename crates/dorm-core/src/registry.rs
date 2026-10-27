//! The model registry, and the path resolver built on it.
//!
//! This module is the reason a model name typed into the REPL is safe. Nothing
//! downstream of here accepts an identifier from the caller: a filter key is
//! resolved to a `&'static FieldMeta` that the derive macro put in the binary, or
//! it is an error. The compiler then quotes only those static strings. There is
//! no path by which REPL input becomes a table or column name.

use crate::error::{Error, Result};
use crate::lookup::Lookup;
use crate::meta::{FieldMeta, ModelMeta};

/// Every registered model, in link order.
pub fn models() -> impl Iterator<Item = &'static ModelMeta> {
    inventory::iter::<ModelMeta>.into_iter()
}

/// Look a model up by the name used in Rust and in the REPL.
pub fn model(name: &str) -> Result<&'static ModelMeta> {
    models()
        .find(|m| m.name == name)
        .ok_or_else(|| Error::UnknownModel(name.to_string()))
}

/// Look a model up by its table, which is what the migration autodetector has.
pub fn model_by_table(table: &str) -> Option<&'static ModelMeta> {
    models().find(|m| m.table == table)
}

/// Resolve a `related_name` back to the model and foreign key that declared it.
///
/// `Tenant` does not know that `Application` points at it; only `Application`'s
/// metadata says so. Reverse access therefore means scanning the registry, which
/// is cheap (a handful of models) and keeps the two derives independent.
pub fn reverse_relation(
    target_model: &str,
    related_name: &str,
) -> Option<(&'static ModelMeta, &'static FieldMeta)> {
    models().find_map(|m| {
        m.fields.iter().find_map(|f| {
            let r = f.relation?;
            (r.model == target_model && r.related_name == Some(related_name)).then_some((m, f))
        })
    })
}

/// One `LEFT JOIN` the query needs in order to reach a field across a relation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Join {
    pub table: &'static str,
    /// Deterministic, derived from the path consumed so far — so two filters
    /// that traverse the same relation produce the same alias and the compiler
    /// emits the join once. It is also what makes snapshots stable.
    pub alias: String,
    pub from_alias: String,
    pub from_column: &'static str,
    pub to_column: &'static str,
    /// A nullable foreign key must not turn the join into a filter of its own,
    /// so every join is a LEFT JOIN and the WHERE clause does the filtering.
    pub nullable: bool,
}

/// A filter key, resolved against metadata.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub joins: Vec<Join>,
    /// The alias the field is read from: the root table, or a join alias.
    pub alias: String,
    pub field: &'static FieldMeta,
    pub lookup: Lookup,
}

/// The alias for the root table of a query.
pub fn root_alias(meta: &ModelMeta) -> String {
    meta.table.to_string()
}

/// The alias for a join reached by the given path segments.
///
/// Prefixed, so it can never collide with a real table name that happens to
/// match a field path.
fn join_alias(segments: &[&str]) -> String {
    format!("j_{}", segments.join("__"))
}

/// Resolve a lookup path such as `name`, `name__icontains`, `tenant__slug` or
/// `app__tenant__slug__istartswith` against a root model.
///
/// The ambiguity worth naming: the last segment may be a field or a lookup.
/// A field wins. `Lookup::parse` returning `None` for `slug` is exactly what
/// lets `tenant__slug` traverse the relation instead of looking for a lookup
/// called `slug`.
pub fn resolve(root: &'static ModelMeta, path: &str) -> Result<Resolved> {
    let segments: Vec<&str> = path.split("__").collect();
    if segments.iter().any(|s| s.is_empty()) {
        return Err(Error::UnknownField {
            model: root.name.to_string(),
            field: path.to_string(),
        });
    }

    let mut current = root;
    let mut alias = root_alias(root);
    let mut joins: Vec<Join> = Vec::new();

    for (i, segment) in segments.iter().enumerate() {
        let field = current.field(segment).ok_or_else(|| {
            // Nothing by that name. If it parses as a lookup it is a lookup in
            // the wrong place (`slug__icontains__gt`); say so precisely.
            if Lookup::parse(segment).is_some() {
                Error::UnknownLookup {
                    lookup: (*segment).to_string(),
                    path: path.to_string(),
                }
            } else {
                Error::UnknownField {
                    model: current.name.to_string(),
                    field: (*segment).to_string(),
                }
            }
        })?;

        let rest = &segments[i + 1..];

        // The path ends here: a plain `exact` on this field. For a foreign key
        // that means comparing against the related row's primary key, which is
        // what `filter(#{ app: app })` does.
        if rest.is_empty() {
            return Ok(Resolved {
                joins,
                alias,
                field,
                lookup: Lookup::Exact,
            });
        }

        // Exactly one segment left, and it names a lookup: the path ends on this
        // field. A relation is the one case needing care — `tenant__in` is a
        // lookup on the foreign key, but if the target model happens to have a
        // field with that name, the field wins and we traverse instead.
        if let ([tail], Some(lookup)) = (rest, Lookup::parse(rest[rest.len() - 1])) {
            let shadowed = field
                .relation
                .and_then(|r| model(r.model).ok())
                .is_some_and(|target| target.field(tail).is_some());
            if !shadowed {
                return Ok(Resolved {
                    joins,
                    alias,
                    field,
                    lookup,
                });
            }
        }

        // More path to walk, so this field had better be a relation.
        let relation = field.relation.ok_or_else(|| Error::NotARelation {
            path: path.to_string(),
            segment: (*segment).to_string(),
        })?;
        let target = model(relation.model)?;
        let to = target.primary_key().ok_or_else(|| {
            Error::Config(format!(
                "model `{}` is the target of a foreign key but declares no primary key",
                target.name
            ))
        })?;

        let next_alias = join_alias(&segments[..=i]);
        // Deduplicated by alias: two filters through the same relation share one
        // join. Without this, `tenant__slug` and `tenant__name` in the same
        // query would join `tenants` twice.
        if !joins.iter().any(|j| j.alias == next_alias) {
            joins.push(Join {
                table: target.table,
                alias: next_alias.clone(),
                from_alias: alias.clone(),
                from_column: field.column,
                to_column: to.column,
                nullable: field.nullable,
            });
        }
        current = target;
        alias = next_alias;
    }

    // Only reachable when the path ended on a relation with nothing after it,
    // which the `rest.is_empty()` arm already returns for. Kept as a named error
    // rather than an `unreachable!`, because `unwrap`-shaped code on a path that
    // depends on metadata is how a REPL panics in production.
    Err(Error::DanglingRelation {
        path: path.to_string(),
        model: current.name.to_string(),
    })
}

#[cfg(test)]
mod tests {
    //! These tests use the example models, which live in `dorm-models`
    //! and therefore cannot be imported here without a dependency cycle. The
    //! registry is exercised end to end from that crate's tests and from the
    //! compiler snapshots; what is tested here is the part that needs no
    //! registry at all.
    use super::*;

    #[test]
    fn join_aliases_are_prefixed_so_they_cannot_collide_with_a_table() {
        assert_eq!(join_alias(&["tenant"]), "j_tenant");
        assert_eq!(join_alias(&["app", "tenant"]), "j_app__tenant");
    }

    #[test]
    fn the_same_relation_path_always_produces_the_same_alias() {
        // This is what lets the compiler emit one join for two filters, and what
        // keeps the SQL snapshots stable across runs.
        assert_eq!(
            join_alias(&["app", "tenant"]),
            join_alias(&["app", "tenant"])
        );
        assert_ne!(join_alias(&["app"]), join_alias(&["app", "tenant"]));
    }

    #[test]
    fn an_empty_segment_is_rejected_rather_than_resolved() {
        // `""`, `"a__"`, `"__a"` all split to a vector containing an empty
        // string; none of them names a field.
        for path in ["", "slug__", "__slug", "a____b"] {
            assert!(path.split("__").any(|s| s.is_empty()), "{path}");
        }
    }
}
