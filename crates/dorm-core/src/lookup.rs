//! Lookups — the `__icontains` half of a filter path.
//!
//! The typed API never needs to parse one of these: `field.icontains(x)` names
//! the variant directly. The REPL always does, because its filter keys arrive as
//! strings. Both end up in the same [`Lookup`], so the compiler below has one
//! code path and the snapshot tests cover the typed and dynamic front ends at
//! once.

use crate::error::{Error, Result};
use crate::value::{FieldType, Value};

/// How a field is compared against the bound value(s).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup {
    Exact,
    IExact,
    Contains,
    IContains,
    In,
    Gt,
    Gte,
    Lt,
    Lte,
    StartsWith,
    IsNull,
    Range,
}

/// How many values a lookup binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arity {
    One,
    /// `in` — at least one. An empty `in` is rejected rather than compiled to
    /// `IN ()`, which is a syntax error in PostgreSQL, or silently to `false`,
    /// which hides a caller bug.
    AtLeastOne,
    /// `range` — a lower and an upper bound.
    Two,
}

impl Lookup {
    /// Parse the suffix of a filter key. Returns `None` for anything that is not
    /// a lookup, which is how the path resolver tells `tenant__slug` (a field)
    /// from `slug__icontains` (a lookup).
    pub fn parse(s: &str) -> Option<Lookup> {
        Some(match s {
            "exact" => Lookup::Exact,
            "iexact" => Lookup::IExact,
            "contains" => Lookup::Contains,
            "icontains" => Lookup::IContains,
            "in" => Lookup::In,
            "gt" => Lookup::Gt,
            "gte" => Lookup::Gte,
            "lt" => Lookup::Lt,
            "lte" => Lookup::Lte,
            "startswith" => Lookup::StartsWith,
            "isnull" => Lookup::IsNull,
            "range" => Lookup::Range,
            _ => return None,
        })
    }

    /// The spelling used in filter keys and in error messages.
    pub fn name(self) -> &'static str {
        match self {
            Lookup::Exact => "exact",
            Lookup::IExact => "iexact",
            Lookup::Contains => "contains",
            Lookup::IContains => "icontains",
            Lookup::In => "in",
            Lookup::Gt => "gt",
            Lookup::Gte => "gte",
            Lookup::Lt => "lt",
            Lookup::Lte => "lte",
            Lookup::StartsWith => "startswith",
            Lookup::IsNull => "isnull",
            Lookup::Range => "range",
        }
    }

    pub fn arity(self) -> Arity {
        match self {
            Lookup::In => Arity::AtLeastOne,
            Lookup::Range => Arity::Two,
            _ => Arity::One,
        }
    }

    /// Whether the comparison is a text pattern match, and therefore only
    /// meaningful on a text column.
    pub fn is_textual(self) -> bool {
        matches!(
            self,
            Lookup::Contains | Lookup::IContains | Lookup::StartsWith | Lookup::IExact
        )
    }

    /// Check a lookup against the field it is applied to and the values given.
    ///
    /// This is the gate that makes the dynamic front end as safe as the typed
    /// one. `replicas__icontains` has no compile error to catch it, so it is
    /// caught here, before any SQL exists.
    pub fn validate(self, ty: FieldType, path: &str, values: &[Value]) -> Result<()> {
        let got = values.len();
        let bad = |want: &'static str| {
            Err(Error::LookupArity {
                lookup: self.name(),
                path: path.to_string(),
                want,
                got,
            })
        };
        match self.arity() {
            Arity::One if got != 1 => return bad("exactly one"),
            Arity::AtLeastOne if got == 0 => return bad("at least one"),
            Arity::Two if got != 2 => return bad("exactly two"),
            _ => {}
        }

        if self.is_textual() && !ty.is_textual() {
            return Err(Error::UnexpectedColumnType {
                column: path.to_string(),
                want: "text column, for a pattern lookup",
            });
        }

        if self == Lookup::IsNull && !matches!(values[0], Value::Bool(_)) {
            return Err(Error::UnexpectedColumnType {
                column: path.to_string(),
                want: "boolean, for isnull",
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lookup_suffix_is_told_apart_from_a_field_name() {
        assert_eq!(Lookup::parse("icontains"), Some(Lookup::IContains));
        // `slug` is a field, not a lookup — this `None` is what lets
        // `tenant__slug` resolve as a relation traversal.
        assert_eq!(Lookup::parse("slug"), None);
    }

    #[test]
    fn every_lookup_name_round_trips() {
        for l in [
            Lookup::Exact,
            Lookup::IExact,
            Lookup::Contains,
            Lookup::IContains,
            Lookup::In,
            Lookup::Gt,
            Lookup::Gte,
            Lookup::Lt,
            Lookup::Lte,
            Lookup::StartsWith,
            Lookup::IsNull,
            Lookup::Range,
        ] {
            assert_eq!(Lookup::parse(l.name()), Some(l), "{}", l.name());
        }
    }

    #[test]
    fn range_wants_two_values_and_in_wants_at_least_one() {
        let one = [Value::I64(1)];
        let two = [Value::I64(1), Value::I64(2)];
        assert!(Lookup::Range.validate(FieldType::I64, "n", &two).is_ok());
        assert!(Lookup::Range.validate(FieldType::I64, "n", &one).is_err());
        assert!(Lookup::In.validate(FieldType::I64, "n", &one).is_ok());
        // An empty `in` is an error, not `IN ()` and not a silent `false`.
        assert!(Lookup::In.validate(FieldType::I64, "n", &[]).is_err());
    }

    #[test]
    fn a_pattern_lookup_on_a_number_is_refused_before_any_sql_exists() {
        // The dynamic front end has no type checker; this is its stand-in.
        let v = [Value::Text("ac".into())];
        assert!(Lookup::IContains
            .validate(FieldType::Text, "name", &v)
            .is_ok());
        assert!(Lookup::IContains
            .validate(FieldType::I64, "replicas", &v)
            .is_err());
    }

    #[test]
    fn isnull_insists_on_a_boolean() {
        assert!(Lookup::IsNull
            .validate(FieldType::Text, "plan", &[Value::Bool(true)])
            .is_ok());
        assert!(Lookup::IsNull
            .validate(FieldType::Text, "plan", &[Value::Text("yes".into())])
            .is_err());
    }
}
