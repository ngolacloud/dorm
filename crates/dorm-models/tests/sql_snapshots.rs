//! Snapshots of compiled SQL.
//!
//! These live in the models crate rather than in `dorm-core` because the
//! compiler needs a populated registry, and the registry is populated by linking
//! models. Nothing here opens a connection: the compiler is pure, which is what
//! makes a snapshot the right instrument.
//!
//! What a snapshot pins that an assertion would not: the **order of the
//! parameters**. A test asserting `text.contains("ILIKE")` passes just as
//! happily when `$1` and `$2` have been swapped, and swapped parameters are a
//! silent wrong-answer bug rather than a crash.

use std::fmt::Write as _;

use dorm_core::compiler::{self, Sql};
use dorm_core::prelude::*;
use dorm_core::queryset::QuerySet;
use dorm_models::*;

/// Render compiled SQL with its parameters and result columns, so all three are
/// under the snapshot.
fn render(sql: &Sql) -> String {
    let mut out = String::new();
    out.push_str(&sql.text);
    out.push('\n');
    if sql.params.is_empty() {
        out.push_str("\n-- no parameters\n");
    } else {
        out.push_str("\n-- parameters, in binding order:\n");
        for (i, p) in sql.params.iter().enumerate() {
            let _ = writeln!(out, "--   ${} = {:?}", i + 1, p);
        }
    }
    if !sql.columns.is_empty() {
        out.push_str("-- result columns:\n");
        for c in &sql.columns {
            let _ = writeln!(out, "--   {} : {:?}", c.name, c.ty);
        }
    }
    out
}

fn select<T: Model>(qs: QuerySet<T>) -> String {
    render(&qs.sql().expect("the plan compiles"))
}

// ---------------------------------------------------------------- basic shapes

#[test]
fn all_rows_names_every_column_and_honours_the_declared_ordering() {
    insta::assert_snapshot!(select(Tenant::objects().all()));
}

#[test]
fn filter_on_one_column() {
    insta::assert_snapshot!(select(Tenant::objects().filter(tenant::slug.eq("acme"))));
}

#[test]
fn repeated_filters_are_anded() {
    insta::assert_snapshot!(select(
        Tenant::objects()
            .filter(tenant::slug.eq("acme"))
            .filter(tenant::name.icontains("ac"))
    ));
}

#[test]
fn exclude_negates_the_whole_condition_not_each_leaf() {
    // Excluding `a AND b` must keep a row that fails only `b`.
    insta::assert_snapshot!(select(
        Tenant::objects().exclude(tenant::slug.eq("acme") & tenant::name.eq("ACME Lda"))
    ));
}

#[test]
fn filter_by_string_key_compiles_the_same_as_the_typed_form() {
    // The parity the REPL depends on: these two snapshots differ only in the
    // value, never in the shape.
    let typed = select(Tenant::objects().filter(tenant::name.icontains("ac")));
    let dynamic = select(Tenant::objects().filter_str("name__icontains", "ac"));
    assert_eq!(typed, dynamic, "the typed and string front ends diverged");
    insta::assert_snapshot!(dynamic);
}

// -------------------------------------------------------------------- lookups

#[test]
fn every_lookup_in_turn() {
    let mut out = String::new();
    let cases: Vec<(&str, Q)> = vec![
        ("exact", tenant::slug.eq("acme")),
        ("iexact", tenant::slug.iexact("ACME")),
        ("contains", tenant::name.contains("CM")),
        ("icontains", tenant::name.icontains("cm")),
        ("startswith", tenant::slug.startswith("ac")),
        ("endswith", tenant::slug.endswith("me")),
        ("iendswith", tenant::name.iendswith("lda")),
        ("regex", tenant::slug.regex("^ac")),
        ("iregex", tenant::name.iregex("lda$")),
        ("in", tenant::slug.is_in(["acme", "globex"])),
        ("gt", tenant::id.gt(10i64)),
        ("gte", tenant::id.gte(10i64)),
        ("lt", tenant::id.lt(10i64)),
        ("lte", tenant::id.lte(10i64)),
        ("range", tenant::id.range(1i64, 9i64)),
        ("isnull true", tenant::name.isnull(true)),
        ("isnull false", tenant::name.isnull(false)),
    ];
    for (name, q) in cases {
        let sql = Tenant::objects().filter(q).sql().expect("it compiles");
        let _ = writeln!(out, "### {name}\n{}", render(&sql));
    }
    insta::assert_snapshot!(out);
}

#[test]
fn endswith_concatenates_the_wildcard_in_sql_so_the_parameter_is_the_bare_suffix() {
    // The shape the request pins: `… LIKE '%' || $1`. The `%` is a literal in the
    // statement, not baked into the value, so the parameter is the escaped suffix
    // on its own — and `endswith("50%")` still matches a literal percent, not
    // every row.
    insta::assert_snapshot!(select(
        Tenant::objects().filter(tenant::slug.endswith("me"))
    ));
}

#[test]
fn a_regex_binds_its_pattern_raw_so_its_metacharacters_survive() {
    // Unlike a `LIKE` lookup, the regex operand is not escaped: `^ac` is a regex
    // anchor, and escaping it would defeat the lookup. `~` is case-sensitive.
    insta::assert_snapshot!(select(
        Tenant::objects().filter(tenant::slug.regex("^ac.*e$"))
    ));
}

#[test]
fn a_pattern_lookup_escapes_the_like_metacharacters_in_the_value() {
    // Searching for a literal percent sign must not match every row. The `%`
    // that does the wildcarding is added by the compiler; the one in the value
    // is escaped.
    insta::assert_snapshot!(select(
        Tenant::objects().filter(tenant::name.contains("50% _off"))
    ));
}

#[test]
fn comparing_against_none_becomes_is_null_not_equals_null() {
    // `= NULL` is never true, so `.eq(None)` has to compile to `IS NULL`.
    let none: Option<String> = None;
    insta::assert_snapshot!(select(Tenant::objects().filter(Q::new("name", none))));
}

// ----------------------------------------------------------- relation walking

#[test]
fn traversing_one_relation_joins_once() {
    insta::assert_snapshot!(select(
        Application::objects().filter(application::tenant.slug().eq("acme"))
    ));
}

#[test]
fn two_filters_through_the_same_relation_share_a_single_join() {
    insta::assert_snapshot!(select(
        Application::objects()
            .filter(application::tenant.slug().eq("acme"))
            .filter(application::tenant.name().icontains("lda"))
    ));
}

#[test]
fn traversing_two_relations_deep() {
    insta::assert_snapshot!(select(
        Deployment::objects().filter(deployment::app.tenant().slug().eq("acme"))
    ));
}

#[test]
fn the_acceptance_query_from_the_request() {
    // `Application.objects.filter(tenant__slug="acme", name__icontains="ap")`.
    insta::assert_snapshot!(select(
        Application::objects()
            .filter(application::tenant.slug().eq("acme"))
            .filter(application::name.icontains("ap"))
    ));
}

#[test]
fn a_foreign_key_compared_without_traversal_uses_its_own_column() {
    insta::assert_snapshot!(select(
        Deployment::objects().filter(deployment::app.eq(Value::I64(7)))
    ));
}

#[test]
fn filtering_across_a_reverse_relation_joins_the_child_by_its_related_name() {
    // `Tenant` has no `apps` column; `Application.tenant` declares
    // `related_name = "apps"`. `apps__name__icontains` must join `applications`
    // on `applications.tenant = tenants.id` and read `name` from there — the
    // reverse of `application::tenant.name()`.
    insta::assert_snapshot!(select(
        Tenant::objects().filter_str("apps__name__icontains", "api")
    ));
}

#[test]
fn a_reverse_relation_walks_two_hops_through_related_names() {
    // Tenant → (reverse `apps`) → Application → (reverse `deployments`) →
    // Deployment: two one-to-many joins reached entirely by `related_name`.
    insta::assert_snapshot!(select(
        Tenant::objects().filter_str("apps__deployments__status", "running")
    ));
}

#[test]
fn select_related_fetches_the_parent_in_the_same_query() {
    insta::assert_snapshot!(select(
        Application::objects()
            .select_related("tenant")
            .select_related("plan")
    ));
}

// ---------------------------------------------------------------- Q and F

#[test]
fn q_composed_with_and_or_and_not() {
    let q = (tenant::slug.eq("acme") | tenant::slug.eq("globex")) & !tenant::name.icontains("test");
    insta::assert_snapshot!(select(Tenant::objects().filter(q)));
}

#[test]
fn a_double_negation_does_not_reach_the_sql() {
    insta::assert_snapshot!(select(Tenant::objects().filter(!!tenant::slug.eq("acme"))));
}

#[test]
fn an_f_update_is_atomic_in_the_database() {
    // The acceptance case: `update(replicas = F("replicas") + 1)`.
    let qs = Deployment::objects().filter(deployment::app.eq(Value::I64(7)));
    let sql = compiler::compile_update(
        qs.plan(),
        &[deployment::replicas.set(F::col("replicas") + 1i64)],
    )
    .expect("it compiles");
    insta::assert_snapshot!(render(&sql));
}

#[test]
fn an_update_through_a_relation_becomes_a_subquery_on_the_primary_key() {
    // An UPDATE has no join, so a condition that traverses a relation has to
    // become `WHERE pk IN (SELECT …)`.
    let qs = Deployment::objects().filter(deployment::app.tenant().slug().eq("acme"));
    let sql = compiler::compile_update(qs.plan(), &[deployment::replicas.set(3i64)])
        .expect("it compiles");
    insta::assert_snapshot!(render(&sql));
}

#[test]
fn an_update_refreshes_auto_now_without_being_asked() {
    let qs = Deployment::objects().filter(deployment::id.eq(1i64));
    let sql = compiler::compile_update(qs.plan(), &[deployment::status.set("running")])
        .expect("it compiles");
    insta::assert_snapshot!(render(&sql));
}

#[test]
fn a_mixed_arithmetic_expression_keeps_its_grouping() {
    let qs = Deployment::objects().filter(deployment::id.eq(1i64));
    let sql = compiler::compile_update(
        qs.plan(),
        &[deployment::replicas.set((F::col("replicas") + 1i64) * 2i64)],
    )
    .expect("it compiles");
    insta::assert_snapshot!(render(&sql));
}

// ------------------------------------------------------------- aggregates

#[test]
fn all_five_aggregates_over_a_column() {
    let kinds = [
        AggKind::Count,
        AggKind::Sum,
        AggKind::Avg,
        AggKind::Min,
        AggKind::Max,
    ];
    let mut out = String::new();
    for kind in kinds {
        let qs = Deployment::objects().all();
        let sql =
            compiler::compile_aggregate(qs.plan(), &[Aggregate::new("value", kind, "replicas")])
                .expect("it compiles");
        let _ = writeln!(out, "### {:?}\n{}", kind, render(&sql));
    }
    insta::assert_snapshot!(out);
}

#[test]
fn counting_a_reverse_relation_joins_and_counts_distinct() {
    // `Count("apps")` on Tenant: `apps` is a related_name, so the rows being
    // counted live in another table. DISTINCT, because a second join would
    // otherwise multiply the count by its fan-out.
    let qs = Tenant::objects().annotate(Aggregate::new("n", AggKind::Count, "apps"));
    insta::assert_snapshot!(select(qs));
}

#[test]
fn the_acceptance_annotation_filters_the_aggregate_in_having() {
    // `Tenant.objects.annotate(n=Count("apps")).filter(n__gte=1).values(["slug","n"])`
    // — the aggregate does not exist when WHERE is evaluated, so the condition
    // has to land in HAVING.
    let qs = Tenant::objects()
        .annotate(Aggregate::new("n", AggKind::Count, "apps"))
        .filter(Q::new("n__gte", 1i64))
        .values(&["slug", "n"]);
    insta::assert_snapshot!(select(qs));
}

#[test]
fn a_condition_on_a_column_and_one_on_an_aggregate_are_separated() {
    let qs = Tenant::objects()
        .annotate(Aggregate::new("n", AggKind::Count, "apps"))
        .filter(tenant::slug.startswith("ac"))
        .filter(Q::new("n__gte", 2i64));
    insta::assert_snapshot!(select(qs));
}

#[test]
fn an_or_that_mixes_an_aggregate_with_a_column_is_refused() {
    // Splitting it across WHERE and HAVING would change its meaning, so it is
    // an error rather than a silently different query.
    let qs = Tenant::objects()
        .annotate(Aggregate::new("n", AggKind::Count, "apps"))
        .filter(tenant::slug.eq("acme") | Q::new("n__gte", 2i64));
    let err = qs.sql().expect_err("this must not compile");
    insta::assert_snapshot!(err.to_string());
}

// ------------------------------------------------------- shaping and terminals

#[test]
fn ordering_limit_offset_and_distinct() {
    insta::assert_snapshot!(select(
        Application::objects()
            .order_by_str(&["tenant__slug", "-created_at"])
            .distinct()
            .limit(10)
            .offset(20)
    ));
}

#[test]
fn ordering_by_an_annotation_uses_its_alias() {
    let qs = Tenant::objects()
        .annotate(Aggregate::new("n", AggKind::Count, "apps"))
        .order_by_str(&["-n"]);
    insta::assert_snapshot!(select(qs));
}

#[test]
fn count_is_a_plain_count_when_nothing_is_grouped() {
    let qs = Application::objects().filter(application::tenant.slug().eq("acme"));
    let sql = compiler::compile_count(qs.plan()).expect("it compiles");
    insta::assert_snapshot!(render(&sql));
}

#[test]
fn count_wraps_a_grouped_query_so_it_counts_the_groups() {
    let qs = Tenant::objects().annotate(Aggregate::new("n", AggKind::Count, "apps"));
    let sql = compiler::compile_count(qs.plan()).expect("it compiles");
    insta::assert_snapshot!(render(&sql));
}

#[test]
fn exists_asks_for_one_row_and_no_ordering() {
    let qs = Application::objects()
        .filter(application::name.eq("api"))
        .order_by_str(&["-created_at"]);
    let sql = compiler::compile_exists(qs.plan()).expect("it compiles");
    insta::assert_snapshot!(render(&sql));
}

#[test]
fn delete_goes_through_a_primary_key_subquery() {
    let qs = Application::objects().filter(application::tenant.slug().eq("acme"));
    let sql = compiler::compile_delete(qs.plan()).expect("it compiles");
    insta::assert_snapshot!(render(&sql));
}

#[test]
fn insert_returns_every_column_so_generated_ones_come_back() {
    let new = NewTenant {
        slug: "acme".into(),
        name: "ACME Lda".into(),
    };
    let sql =
        compiler::compile_insert(Tenant::meta(), &[new.column_values()]).expect("it compiles");
    insta::assert_snapshot!(render(&sql));
}

#[test]
fn a_bulk_insert_is_one_statement_with_one_tuple_per_row() {
    let rows: Vec<Vec<(String, Value)>> = ["a", "b", "c"]
        .iter()
        .map(|s| {
            NewTenant {
                slug: (*s).to_string(),
                name: format!("Tenant {s}"),
            }
            .column_values()
        })
        .collect();
    let sql = compiler::compile_insert(Tenant::meta(), &rows).expect("it compiles");
    insta::assert_snapshot!(render(&sql));
}

#[test]
fn values_projects_onto_named_columns_including_across_a_relation() {
    insta::assert_snapshot!(select(
        Application::objects().values(&["name", "tenant__slug"])
    ));
}

// ------------------------------------------------------------------- refusals

#[test]
fn an_unknown_field_is_refused_with_the_model_named() {
    let err = Tenant::objects()
        .filter_str("slog", "acme")
        .sql()
        .expect_err("there is no such field");
    insta::assert_snapshot!(err.to_string());
}

#[test]
fn a_path_through_a_non_relation_is_refused() {
    let err = Tenant::objects()
        .filter_str("slug__name", "acme")
        .sql()
        .expect_err("slug is text, not a relation");
    insta::assert_snapshot!(err.to_string());
}

#[test]
fn a_pattern_lookup_on_a_number_is_refused() {
    let err = Deployment::objects()
        .filter_str("replicas__icontains", "2")
        .sql()
        .expect_err("replicas is a bigint");
    insta::assert_snapshot!(err.to_string());
}

#[test]
fn an_empty_in_list_is_refused_rather_than_compiled_to_in_nothing() {
    let err = Tenant::objects()
        .filter(Q::many("slug__in", vec![]))
        .sql()
        .expect_err("an empty IN is a caller bug");
    insta::assert_snapshot!(err.to_string());
}

#[test]
fn an_unknown_lookup_is_refused() {
    let err = Tenant::objects()
        .filter_str("slug__matches", "^ac")
        .sql()
        .expect_err("`matches` is not among the lookups");
    insta::assert_snapshot!(err.to_string());
}
