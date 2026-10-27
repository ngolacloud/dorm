//! A standalone blog, using ONLY the ORM crates — no NgolaCloud, no platform.
//!
//! This is the independence proof of ADR 0073: an external Rust developer who
//! has never heard of NgolaCloud defines models, sees the migration SQL to
//! review, and inspects a typed query's SQL — none of which needs a database.
//! Applying the migration and running the query live is shown in the ORM's
//! integration tests (gated on `DELONIX_ORM_DSN`).

// The model fields are read by the derive-generated metadata, not directly here.
#![allow(dead_code)]

use chrono::{DateTime, Utc};
use dorm_core::model::Model;
use dorm_core::prelude::*;
use dorm_derive::Model;
use dorm_migrations as mig;

/// A blog author.
#[derive(Model, Debug, Clone)]
#[model(table = "authors", ordering = "name")]
struct Author {
    #[field(primary_key)]
    id: i64,
    #[field(unique, max_length = 120)]
    name: String,
}

/// A post, owned by an author. Shows a foreign key, a composite unique, a check,
/// and an `auto_now_add` timestamp.
#[derive(Model, Debug, Clone)]
#[model(
    table = "posts",
    unique_together = "author, slug",
    check = "length(title) > 0"
)]
struct Post {
    #[field(primary_key)]
    id: i64,
    #[field(foreign_key = "Author", on_delete = "cascade", related_name = "posts")]
    author: ForeignKey<Author>,
    #[field(max_length = 80)]
    slug: String,
    #[field(max_length = 200, index)]
    title: String,
    #[field(auto_now_add)]
    published_at: DateTime<Utc>,
}

// `std::result::Result` spelled out: the ORM prelude brings its own one-argument
// `Result<T>` into scope, and `Box<dyn Error>` lets `?` box both the ORM's and
// the migrator's error types.
fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let models = [<Author as Model>::meta(), <Post as Model>::meta()];

    println!("== 1. Generated schema — review this before applying ==\n");
    println!("{}", mig::ddl::schema_for(&models)?);

    let migration = mig::initial(&models)?;
    println!(
        "== 2. Initial migration: version {}, {} statement(s) ==\n",
        migration.version,
        migration.statements.len()
    );

    // A typed query. The column names come from the model; nothing is a string
    // the caller invented, and the SQL is inspectable without touching a database.
    let query = Post::objects()
        .filter(post::title.icontains("rust"))
        .order_by([post::published_at.desc()])
        .limit(10);
    let sql = query.sql()?;
    println!("== 3. Typed query — SQL inspected (no database needed) ==\n");
    println!("{}\n  ({} parameter(s))", sql.text, sql.params.len());

    Ok(())
}
