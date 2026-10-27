//! `Q`, `F`, assignments, ordering and aggregates.
//!
//! One representation decision runs through this file and pays for itself in the
//! compiler: the typed API does **not** carry its lookup as an enum. When you
//! write `tenant::name.icontains("ac")` the field handle produces the path
//! `"name__icontains"` — the same string the REPL would have sent. Type safety
//! still happens at the call site, because `icontains` is only implemented for
//! text fields, but downstream there is a single code path and the snapshot
//! tests cover both front ends at once.

use std::ops::{Add, BitAnd, BitOr, Div, Mul, Not, Sub};

use crate::value::{ToValue, Value};

/// One resolved-at-compile-time condition: a lookup path and its values.
#[derive(Debug, Clone, PartialEq)]
pub struct Predicate {
    /// A full filter key, lookup suffix included: `name`, `name__icontains`,
    /// `tenant__slug`, `app__tenant__slug__startswith`.
    pub path: String,
    pub values: Vec<Value>,
}

/// A tree of conditions.
///
/// `Empty` is the identity, which is what makes `filter()` with no arguments and
/// an `and` fold over an empty list both behave.
#[derive(Debug, Clone, PartialEq)]
pub enum Q {
    Empty,
    Pred(Predicate),
    And(Box<Q>, Box<Q>),
    Or(Box<Q>, Box<Q>),
    Not(Box<Q>),
}

impl Q {
    /// Build a condition from a path and one value — the dynamic front end's
    /// entry point, and what every typed lookup method funnels into.
    pub fn new(path: impl Into<String>, value: impl ToValue) -> Q {
        Q::Pred(Predicate {
            path: path.into(),
            values: vec![value.to_value()],
        })
    }

    /// Build a condition whose lookup binds several values (`in`, `range`).
    pub fn many(path: impl Into<String>, values: Vec<Value>) -> Q {
        Q::Pred(Predicate {
            path: path.into(),
            values,
        })
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, Q::Empty)
    }

    /// `and`, skipping the identity so the compiled SQL has no `TRUE AND`.
    pub fn and(self, other: Q) -> Q {
        match (self, other) {
            (Q::Empty, q) | (q, Q::Empty) => q,
            (a, b) => Q::And(Box::new(a), Box::new(b)),
        }
    }

    /// `or`. Note it does *not* absorb `Empty`: `q | Q::Empty` is `q`, because an
    /// empty condition matches everything and `x OR everything` would be a
    /// surprising way to spell "no filter".
    pub fn or(self, other: Q) -> Q {
        match (self, other) {
            (Q::Empty, q) | (q, Q::Empty) => q,
            (a, b) => Q::Or(Box::new(a), Box::new(b)),
        }
    }

    /// Collect every path this tree mentions, so the compiler can resolve the
    /// joins before it writes a clause.
    pub fn paths(&self, out: &mut Vec<String>) {
        match self {
            Q::Empty => {}
            Q::Pred(p) => out.push(p.path.clone()),
            Q::And(a, b) | Q::Or(a, b) => {
                a.paths(out);
                b.paths(out);
            }
            Q::Not(q) => q.paths(out),
        }
    }
}

impl BitAnd for Q {
    type Output = Q;
    fn bitand(self, rhs: Q) -> Q {
        self.and(rhs)
    }
}

impl BitOr for Q {
    type Output = Q;
    fn bitor(self, rhs: Q) -> Q {
        self.or(rhs)
    }
}

impl Not for Q {
    type Output = Q;
    fn not(self) -> Q {
        match self {
            // `!!q` is `q`, which keeps a double negation out of the SQL.
            Q::Not(inner) => *inner,
            Q::Empty => Q::Empty,
            q => Q::Not(Box::new(q)),
        }
    }
}

/// An expression evaluated by the database, not by us.
///
/// This is what makes `replicas = replicas + 1` atomic: the increment happens
/// inside the `UPDATE`, so two concurrent callers both take effect. Reading the
/// row, adding one in Rust and writing it back loses one of the two.
#[derive(Debug, Clone, PartialEq)]
pub enum F {
    /// A column of the model being updated, by field name.
    Col(String),
    Lit(Value),
    Add(Box<F>, Box<F>),
    Sub(Box<F>, Box<F>),
    Mul(Box<F>, Box<F>),
    Div(Box<F>, Box<F>),
}

impl F {
    /// Reference a column — `F::col("replicas")`.
    pub fn col(name: impl Into<String>) -> F {
        F::Col(name.into())
    }

    /// A constant inside an expression.
    pub fn lit(v: impl ToValue) -> F {
        F::Lit(v.to_value())
    }

    /// Every column this expression reads, for validation against metadata.
    pub fn columns(&self, out: &mut Vec<String>) {
        match self {
            F::Col(c) => out.push(c.clone()),
            F::Lit(_) => {}
            F::Add(a, b) | F::Sub(a, b) | F::Mul(a, b) | F::Div(a, b) => {
                a.columns(out);
                b.columns(out);
            }
        }
    }
}

// The arithmetic operators are written out per right-hand type rather than
// generically over `ToValue`. A blanket `impl<T: ToValue> Add<T> for F` cannot
// coexist with `impl Add<F> for F` under coherence, and four short impls read
// better than the trait gymnastics that would avoid them.
macro_rules! f_ops {
    ($($rhs:ty),*) => {
        $(
            impl Add<$rhs> for F {
                type Output = F;
                fn add(self, rhs: $rhs) -> F { F::Add(Box::new(self), Box::new(F::lit(rhs))) }
            }
            impl Sub<$rhs> for F {
                type Output = F;
                fn sub(self, rhs: $rhs) -> F { F::Sub(Box::new(self), Box::new(F::lit(rhs))) }
            }
            impl Mul<$rhs> for F {
                type Output = F;
                fn mul(self, rhs: $rhs) -> F { F::Mul(Box::new(self), Box::new(F::lit(rhs))) }
            }
            impl Div<$rhs> for F {
                type Output = F;
                fn div(self, rhs: $rhs) -> F { F::Div(Box::new(self), Box::new(F::lit(rhs))) }
            }
        )*
    };
}

f_ops!(i32, i64, rust_decimal::Decimal);

impl Add for F {
    type Output = F;
    fn add(self, rhs: F) -> F {
        F::Add(Box::new(self), Box::new(rhs))
    }
}
impl Sub for F {
    type Output = F;
    fn sub(self, rhs: F) -> F {
        F::Sub(Box::new(self), Box::new(rhs))
    }
}
impl Mul for F {
    type Output = F;
    fn mul(self, rhs: F) -> F {
        F::Mul(Box::new(self), Box::new(rhs))
    }
}
impl Div for F {
    type Output = F;
    fn div(self, rhs: F) -> F {
        F::Div(Box::new(self), Box::new(rhs))
    }
}

/// One `SET column = expression` in an `UPDATE`.
#[derive(Debug, Clone, PartialEq)]
pub struct Assignment {
    /// A field name of the model being updated. Never a path: an `UPDATE` cannot
    /// write through a join.
    pub field: String,
    pub expr: F,
}

impl Assignment {
    pub fn new(field: impl Into<String>, expr: F) -> Assignment {
        Assignment {
            field: field.into(),
            expr,
        }
    }
}

/// One `ORDER BY` term.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderBy {
    pub path: String,
    pub descending: bool,
}

impl OrderBy {
    /// Parse the Django spelling, where a leading `-` means descending.
    pub fn parse(s: &str) -> OrderBy {
        match s.strip_prefix('-') {
            Some(rest) => OrderBy {
                path: rest.to_string(),
                descending: true,
            },
            None => OrderBy {
                path: s.to_string(),
                descending: false,
            },
        }
    }
}

/// The aggregate functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggKind {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl AggKind {
    pub fn sql(self) -> &'static str {
        match self {
            AggKind::Count => "COUNT",
            AggKind::Sum => "SUM",
            AggKind::Avg => "AVG",
            AggKind::Min => "MIN",
            AggKind::Max => "MAX",
        }
    }

    pub fn parse(s: &str) -> Option<AggKind> {
        Some(match s {
            "Count" | "count" => AggKind::Count,
            "Sum" | "sum" => AggKind::Sum,
            "Avg" | "avg" => AggKind::Avg,
            "Min" | "min" => AggKind::Min,
            "Max" | "max" => AggKind::Max,
            _ => return None,
        })
    }

    /// The field type the result comes back as.
    ///
    /// `COUNT` is always `BIGINT`; `AVG` over any numeric is `NUMERIC`; the rest
    /// keep the column's own type, which the compiler fills in.
    pub fn result_type(self) -> Option<crate::value::FieldType> {
        match self {
            AggKind::Count => Some(crate::value::FieldType::I64),
            AggKind::Avg => Some(crate::value::FieldType::Decimal),
            _ => None,
        }
    }
}

/// One aggregate, with the name its result is returned under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Aggregate {
    pub alias: String,
    pub kind: AggKind,
    /// A field path, or a `related_name` when counting the far side of a
    /// relation — which is what `Count("apps")` on `Tenant` means.
    pub path: String,
}

impl Aggregate {
    pub fn new(alias: impl Into<String>, kind: AggKind, path: impl Into<String>) -> Aggregate {
        Aggregate {
            alias: alias.into(),
            kind,
            path: path.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn and_absorbs_the_identity_so_no_true_and_reaches_the_sql() {
        let q = Q::new("slug", "acme");
        assert_eq!(Q::Empty.and(q.clone()), q);
        assert_eq!(q.clone().and(Q::Empty), q);
    }

    #[test]
    fn the_operators_build_the_same_tree_as_the_methods() {
        let a = Q::new("slug", "acme");
        let b = Q::new("name", "ACME");
        assert_eq!(a.clone() & b.clone(), a.clone().and(b.clone()));
        assert_eq!(a.clone() | b.clone(), a.or(b));
    }

    #[test]
    fn a_double_negation_cancels_rather_than_nesting() {
        let q = Q::new("slug", "acme");
        assert_eq!(!!q.clone(), q);
        assert!(matches!(!Q::new("slug", "acme"), Q::Not(_)));
        // Negating "no condition" is still "no condition", not NOT TRUE.
        assert_eq!(!Q::Empty, Q::Empty);
    }

    #[test]
    fn every_path_in_a_tree_is_collected_for_join_resolution() {
        let q = (Q::new("tenant__slug", "acme") | Q::new("name__icontains", "ap"))
            & !Q::new("replicas__gte", 2i64);
        let mut paths = Vec::new();
        q.paths(&mut paths);
        paths.sort();
        assert_eq!(paths, ["name__icontains", "replicas__gte", "tenant__slug"]);
    }

    #[test]
    fn f_arithmetic_nests_left_to_right_with_a_literal_on_the_right() {
        let e = F::col("replicas") + 1i64;
        assert_eq!(
            e,
            F::Add(
                Box::new(F::Col("replicas".into())),
                Box::new(F::Lit(Value::I64(1)))
            )
        );
    }

    #[test]
    fn f_reports_the_columns_it_reads() {
        let e = (F::col("replicas") + 1i64) * F::col("factor");
        let mut cols = Vec::new();
        e.columns(&mut cols);
        cols.sort();
        assert_eq!(cols, ["factor", "replicas"]);
    }

    #[test]
    fn ordering_reads_the_leading_minus_as_descending() {
        assert_eq!(
            OrderBy::parse("-created_at"),
            OrderBy {
                path: "created_at".into(),
                descending: true
            }
        );
        assert!(!OrderBy::parse("slug").descending);
    }

    #[test]
    fn aggregate_names_are_accepted_in_both_the_repl_and_rust_spellings() {
        assert_eq!(AggKind::parse("Count"), Some(AggKind::Count));
        assert_eq!(AggKind::parse("count"), Some(AggKind::Count));
        assert_eq!(AggKind::parse("Median"), None);
    }

    #[test]
    fn count_always_comes_back_as_a_bigint_and_min_keeps_the_column_type() {
        assert_eq!(
            AggKind::Count.result_type(),
            Some(crate::value::FieldType::I64)
        );
        assert_eq!(AggKind::Min.result_type(), None);
    }
}
