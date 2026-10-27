//! Typed field handles, and the foreign key wrapper.
//!
//! A handle is what the derive macro puts in the per-model module, so that
//! `tenant::slug` is a value you can call lookups on. Two properties are worth
//! naming:
//!
//! * The path is a [`Cow`], which is what lets `tenant::slug` be a `const` while
//!   `application::tenant.slug()` still builds `"tenant__slug"` at runtime. A
//!   `&'static str` cannot be concatenated; an owned `String` cannot be `const`.
//! * The text lookups are implemented only for `Field<String>`. That is the type
//!   safety the typed API buys over the REPL: `deployment::replicas.icontains(..)`
//!   does not compile, where the REPL has to catch the same mistake at runtime.

use std::borrow::Cow;
use std::marker::PhantomData;

use crate::expr::{Assignment, OrderBy, F, Q};
use crate::value::{ToValue, Value};

/// A handle on one field of one model, carrying the Rust type of its column.
#[derive(Debug, Clone)]
pub struct Field<V> {
    path: Cow<'static, str>,
    marker: PhantomData<fn() -> V>,
}

impl<V> Field<V> {
    /// Build a handle from a literal path. `const`, so the generated per-model
    /// modules can expose their fields as constants.
    pub const fn borrowed(path: &'static str) -> Field<V> {
        Field {
            path: Cow::Borrowed(path),
            marker: PhantomData,
        }
    }

    /// Build a handle whose path was assembled at runtime, which is what a
    /// relation accessor does.
    pub fn owned(path: String) -> Field<V> {
        Field {
            path: Cow::Owned(path),
            marker: PhantomData,
        }
    }

    /// The lookup path, lookup suffix excluded.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// `path__lookup`, the single spelling both front ends produce.
    fn keyed(&self, lookup: &str) -> String {
        format!("{}__{}", self.path, lookup)
    }

    /// Ascending `ORDER BY` on this field.
    pub fn asc(&self) -> OrderBy {
        OrderBy {
            path: self.path.to_string(),
            descending: false,
        }
    }

    /// Descending `ORDER BY` on this field.
    pub fn desc(&self) -> OrderBy {
        OrderBy {
            path: self.path.to_string(),
            descending: true,
        }
    }

    /// `IS NULL` / `IS NOT NULL`.
    pub fn isnull(&self, yes: bool) -> Q {
        Q::new(self.keyed("isnull"), yes)
    }
}

impl<V: ToValue> Field<V> {
    /// `=`. Takes anything that converts into the column's type, so `.eq("acme")`
    /// works on a `Field<String>` without an explicit `to_string()`.
    pub fn eq<A: Into<V>>(&self, v: A) -> Q {
        Q::new(self.path.to_string(), v.into())
    }

    /// `>`.
    pub fn gt<A: Into<V>>(&self, v: A) -> Q {
        Q::new(self.keyed("gt"), v.into())
    }

    /// `>=`.
    pub fn gte<A: Into<V>>(&self, v: A) -> Q {
        Q::new(self.keyed("gte"), v.into())
    }

    /// `<`.
    pub fn lt<A: Into<V>>(&self, v: A) -> Q {
        Q::new(self.keyed("lt"), v.into())
    }

    /// `<=`.
    pub fn lte<A: Into<V>>(&self, v: A) -> Q {
        Q::new(self.keyed("lte"), v.into())
    }

    /// `IN (…)`. An empty list is refused by the compiler rather than turned
    /// into `IN ()`; see `Lookup::validate`.
    pub fn is_in<A: Into<V>, I: IntoIterator<Item = A>>(&self, vs: I) -> Q {
        Q::many(
            self.keyed("in"),
            vs.into_iter().map(|v| v.into().to_value()).collect(),
        )
    }

    /// `BETWEEN lo AND hi`, inclusive at both ends, as Django's `range` is.
    pub fn range<A: Into<V>, B: Into<V>>(&self, lo: A, hi: B) -> Q {
        Q::many(
            self.keyed("range"),
            vec![lo.into().to_value(), hi.into().to_value()],
        )
    }

    /// `SET` this column, to a value or to an expression the database evaluates.
    pub fn set<E: Into<F>>(&self, e: E) -> Assignment {
        Assignment::new(self.path.to_string(), e.into())
    }
}

/// Text-only lookups. The bound is what stops `replicas.icontains(..)` from
/// compiling.
impl Field<String> {
    /// Case-insensitive equality.
    pub fn iexact(&self, v: impl Into<String>) -> Q {
        Q::new(self.keyed("iexact"), v.into())
    }

    /// `LIKE '%v%'`.
    pub fn contains(&self, v: impl Into<String>) -> Q {
        Q::new(self.keyed("contains"), v.into())
    }

    /// `ILIKE '%v%'`.
    pub fn icontains(&self, v: impl Into<String>) -> Q {
        Q::new(self.keyed("icontains"), v.into())
    }

    /// `LIKE 'v%'`.
    pub fn startswith(&self, v: impl Into<String>) -> Q {
        Q::new(self.keyed("startswith"), v.into())
    }
}

// A bare value used where an expression is wanted, so `.set(3)` reads as well as
// `.set(F::col("replicas") + 1)`.
macro_rules! into_f {
    ($($t:ty),*) => {
        $(
            impl From<$t> for F {
                fn from(v: $t) -> F { F::lit(v) }
            }
        )*
    };
}
into_f!(i32, i64, bool, String, rust_decimal::Decimal, uuid::Uuid);

impl From<&str> for F {
    fn from(v: &str) -> F {
        F::lit(v)
    }
}

impl From<Value> for F {
    fn from(v: Value) -> F {
        F::Lit(v)
    }
}

/// A foreign key, as stored on a model instance.
///
/// It holds the referenced primary key, and — after `select_related` — the row
/// itself. Keeping both in one type is what lets `app.tenant` be useful whether
/// or not the caller asked for the join, without a second nullable field on
/// every model.
#[derive(Debug, Clone, PartialEq)]
pub struct ForeignKey<T> {
    id: Value,
    loaded: Option<Box<T>>,
}

impl<T> ForeignKey<T> {
    /// Reference a row by its primary key, without loading it.
    pub fn from_id(id: Value) -> ForeignKey<T> {
        ForeignKey { id, loaded: None }
    }

    /// The referenced primary key.
    pub fn id(&self) -> &Value {
        &self.id
    }

    /// The referenced row, if a `select_related` brought it along.
    pub fn get(&self) -> Option<&T> {
        self.loaded.as_deref()
    }

    /// Attach a loaded row. Used by `select_related`'s row decoder.
    pub fn attach(&mut self, row: T) {
        self.loaded = Some(Box::new(row));
    }

    /// Whether the referenced row is already in memory.
    pub fn is_loaded(&self) -> bool {
        self.loaded.is_some()
    }
}

impl<T> ToValue for ForeignKey<T> {
    fn to_value(&self) -> Value {
        self.id.clone()
    }
}

/// A foreign key's own column type is the *target's* primary key type, which
/// only the registry knows. `BIGINT` is the default because a surrogate key is
/// the common case; a model whose key is a `TEXT` slug states the type in the
/// field attribute, and the derive macro writes that into the metadata instead.
impl<T> crate::value::ColumnType for ForeignKey<T> {
    fn field_type() -> crate::value::FieldType {
        crate::value::FieldType::I64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::Predicate;

    const SLUG: Field<String> = Field::borrowed("slug");
    const REPLICAS: Field<i64> = Field::borrowed("replicas");

    fn predicate(q: Q) -> Predicate {
        match q {
            Q::Pred(p) => p,
            other => panic!("expected a predicate, got {other:?}"),
        }
    }

    #[test]
    fn eq_produces_a_bare_path_with_no_lookup_suffix() {
        let p = predicate(SLUG.eq("acme"));
        assert_eq!(p.path, "slug");
        assert_eq!(p.values, vec![Value::Text("acme".into())]);
    }

    #[test]
    fn a_lookup_is_carried_as_the_path_suffix_the_repl_would_have_sent() {
        // This is the representation decision this crate rests on: the typed and
        // the dynamic front end converge here, not in the compiler.
        assert_eq!(predicate(SLUG.icontains("ac")).path, "slug__icontains");
        assert_eq!(predicate(REPLICAS.gte(2i64)).path, "replicas__gte");
        assert_eq!(predicate(SLUG.isnull(true)).path, "slug__isnull");
    }

    #[test]
    fn range_and_in_carry_all_their_values_in_order() {
        let p = predicate(REPLICAS.range(1i64, 5i64));
        assert_eq!(p.values, vec![Value::I64(1), Value::I64(5)]);

        let p = predicate(REPLICAS.is_in([1i64, 2, 3]));
        assert_eq!(p.path, "replicas__in");
        assert_eq!(p.values.len(), 3);
    }

    #[test]
    fn a_runtime_path_behaves_exactly_like_a_const_one() {
        // What a relation accessor produces.
        let f: Field<String> = Field::owned("tenant__slug".to_string());
        assert_eq!(predicate(f.eq("acme")).path, "tenant__slug");
        assert_eq!(predicate(f.icontains("ac")).path, "tenant__slug__icontains");
    }

    #[test]
    fn set_takes_a_plain_value_or_an_expression() {
        assert_eq!(REPLICAS.set(3i64).expr, F::Lit(Value::I64(3)));
        let a = REPLICAS.set(F::col("replicas") + 1i64);
        assert_eq!(a.field, "replicas");
        assert!(matches!(a.expr, F::Add(_, _)));
    }

    #[test]
    fn ordering_helpers_drop_any_lookup_and_keep_the_field() {
        assert_eq!(SLUG.asc().path, "slug");
        assert!(SLUG.desc().descending);
    }

    #[test]
    fn a_foreign_key_binds_as_its_referenced_primary_key() {
        let fk: ForeignKey<()> = ForeignKey::from_id(Value::I64(7));
        assert_eq!(fk.to_value(), Value::I64(7));
        assert!(!fk.is_loaded());
        assert!(fk.get().is_none());
    }

    #[test]
    fn attaching_a_row_makes_the_foreign_key_readable_without_a_second_query() {
        let mut fk: ForeignKey<String> = ForeignKey::from_id(Value::I64(7));
        fk.attach("the tenant".to_string());
        assert!(fk.is_loaded());
        assert_eq!(fk.get().map(String::as_str), Some("the tenant"));
        // Attaching does not change what it binds as.
        assert_eq!(fk.to_value(), Value::I64(7));
    }
}
