//! Parity: the schema the ORM **generates** from model metadata equals the
//! known-good schema that was hand-written.
//!
//! The oracle is the exact text `schema_sql()` used to return before it started
//! delegating to `dorm-migrations`. Freezing it here (rather than deleting
//! it) is the point: the generator is proven against a human-reviewed schema, and
//! if a model changes, this oracle must change with it in the same commit — which
//! is the review gate. Comparison is whitespace-insensitive (`normalize`).
//!
//! These are all GENERIC models — the platform's own tables and their parity
//! against the `CpStore` schema live in `delonix-store-models`.

use dorm_core::model::Model;
use dorm_migrations::ddl;
use dorm_models::{Application, Deployment, Plan, Product, Tenant};

/// Collapse every run of whitespace to a single space and trim.
fn normalize(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The hand-written schema for the four example models — the oracle.
const ORACLE_EXAMPLE_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS tenants (
    id          BIGSERIAL PRIMARY KEY,
    slug        TEXT        NOT NULL UNIQUE,
    name        TEXT        NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS tenants_slug_idx ON tenants (slug);

CREATE TABLE IF NOT EXISTS plans (
    id            BIGSERIAL PRIMARY KEY,
    code          TEXT    NOT NULL UNIQUE,
    name          TEXT    NOT NULL,
    monthly_price NUMERIC NOT NULL
);

CREATE TABLE IF NOT EXISTS applications (
    id         BIGSERIAL PRIMARY KEY,
    tenant     BIGINT      NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    plan       BIGINT      NOT NULL REFERENCES plans (id)   ON DELETE RESTRICT,
    name       TEXT        NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS applications_name_idx ON applications (name);

CREATE TABLE IF NOT EXISTS deployments (
    id         BIGSERIAL PRIMARY KEY,
    app        BIGINT      NOT NULL REFERENCES applications (id) ON DELETE CASCADE,
    replicas   BIGINT      NOT NULL,
    status     TEXT        NOT NULL,
    config     JSONB       NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
";

#[test]
fn the_generated_schema_reproduces_the_hand_written_oracle() {
    let generated = ddl::schema_for(&[
        <Tenant as Model>::meta(),
        <Plan as Model>::meta(),
        <Application as Model>::meta(),
        <Deployment as Model>::meta(),
    ])
    .expect("the example models' schema generates");

    assert_eq!(
        normalize(&generated),
        normalize(ORACLE_EXAMPLE_SCHEMA),
        "the generated schema drifted from the hand-written oracle:\n\
         --- generated ---\n{generated}\n--- oracle ---\n{ORACLE_EXAMPLE_SCHEMA}"
    );
}

/// `schema_sql()` (which now delegates to the generator) agrees with the oracle.
#[test]
fn schema_sql_delegates_to_the_generator_and_still_matches_the_oracle() {
    assert_eq!(
        normalize(&dorm_models::schema_sql()),
        normalize(ORACLE_EXAMPLE_SCHEMA)
    );
}

/// A generic model proves the three features the platform's real tables need —
/// a `TEXT` primary key, a composite `UNIQUE`, and a table-level `CHECK` — stay
/// covered without any platform entity in the ORM.
#[test]
fn a_generic_text_pk_composite_unique_and_check_all_generate() {
    let generated = ddl::create_table(<Product as Model>::meta()).expect("products generate");
    assert_eq!(
        normalize(&generated),
        normalize(
            "CREATE TABLE IF NOT EXISTS products (
                sku      TEXT    PRIMARY KEY,
                category TEXT    NOT NULL,
                name     TEXT    NOT NULL,
                price    NUMERIC NOT NULL,
                UNIQUE (category, name),
                CHECK (price >= 0)
            )"
        ),
        "products parity broke:\n{generated}"
    );
}
