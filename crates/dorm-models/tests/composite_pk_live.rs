//! Composite primary key against a real PostgreSQL (ADR 0042 + 0045 / L5-composite-PK).
//!
//! ```text
//! DELONIX_ORM_TEST_DSN='host=127.0.0.1 port=5432 user=orm dbname=ormtest' \
//!   cargo test -p dorm-models --test composite_pk_live -- --ignored --nocapture
//! ```

use dorm_core::error::Error;
use dorm_core::model::Model;
use dorm_derive::Model;
use tokio::runtime::Runtime;

/// A natural-key row: the key is `(region, code)`, not a surrogate id.
#[derive(Model, Debug, Clone)]
#[model(table = "l5_region_codes", ordering = "region")]
struct RegionCode {
    #[field(primary_key, max_length = 20)]
    region: String,
    #[field(primary_key, max_length = 20)]
    code: String,
    label: String,
}

fn dsn() -> Option<String> {
    std::env::var("DELONIX_ORM_TEST_DSN")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

fn runtime() -> &'static Runtime {
    static RT: std::sync::OnceLock<Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("a tokio runtime")
    })
}

/// Pure shape check: the DDL is a table-level `PRIMARY KEY (region, code)`, not an
/// inline per-column key. Runs without a database.
#[test]
fn the_ddl_emits_a_table_level_composite_primary_key() {
    let ddl = dorm_migrations::ddl::create_table(RegionCode::meta()).expect("ddl");
    assert!(
        ddl.contains("PRIMARY KEY (region, code)"),
        "expected a composite table-level key: {ddl}"
    );
    assert!(
        !ddl.contains("region TEXT PRIMARY KEY") && !ddl.contains("code TEXT PRIMARY KEY"),
        "the key columns must be plain, not inline PRIMARY KEY: {ddl}"
    );
    assert!(RegionCode::meta().is_composite_pk());
    assert_eq!(RegionCode::meta().primary_keys().count(), 2);
}

#[test]
#[ignore = "needs a Postgres (DELONIX_ORM_TEST_DSN)"]
fn a_composite_key_table_creates_inserts_queries_and_enforces_uniqueness() {
    let Some(dsn) = dsn() else {
        eprintln!("skipped: DELONIX_ORM_TEST_DSN is not set");
        return;
    };
    std::env::set_var(dorm_core::db::DSN_ENV, &dsn);

    runtime().block_on(async {
        dorm_core::db::execute("DROP TABLE IF EXISTS l5_region_codes", &[])
            .await
            .expect("clean");
        // Build the table from the generated DDL — proving it is valid SQL.
        let ddl = dorm_migrations::ddl::create_table(RegionCode::meta()).expect("ddl");
        dorm_core::db::execute(&ddl, &[])
            .await
            .expect("create table");

        // Two rows share a region, one shares a code — only the full (region, code)
        // is unique.
        for (region, code, label) in [
            ("eu", "lis", "Lisbon"),
            ("eu", "mad", "Madrid"),
            ("af", "lis", "Lisbon-AF"),
        ] {
            RegionCode::objects()
                .create(NewRegionCode {
                    region: region.into(),
                    code: code.into(),
                    label: label.into(),
                })
                .await
                .expect("insert");
        }

        // A query on the full composite key returns exactly one row.
        let one = RegionCode::objects()
            .filter(
                region_code::region
                    .eq("eu")
                    .and(region_code::code.eq("mad")),
            )
            .get()
            .await
            .expect("composite-key lookup");
        assert_eq!(one.label, "Madrid");

        // Half the key is ambiguous: region "eu" alone matches two.
        let eu = RegionCode::objects()
            .filter(region_code::region.eq("eu"))
            .count()
            .await
            .expect("by region");
        assert_eq!(eu, 2, "the region is not unique on its own");

        // The composite key is enforced: re-inserting (eu, lis) violates the PK.
        let dup = RegionCode::objects()
            .create(NewRegionCode {
                region: "eu".into(),
                code: "lis".into(),
                label: "dup".into(),
            })
            .await;
        assert!(
            matches!(dup, Err(Error::Db(ref m)) if m.contains("23505")),
            "a duplicate composite key is a unique violation, got {dup:?}"
        );

        // Instance save now works on a composite key (ADR 0045): the UPDATE is
        // located by the whole (region, code) key, not by a single value.
        let mut row = one;
        row.label = "changed".into();
        row.save().await.expect("instance save on a composite key");

        // Re-read it to prove the change hit the row the key names, and only it.
        let reread = RegionCode::objects()
            .filter(
                region_code::region
                    .eq("eu")
                    .and(region_code::code.eq("mad")),
            )
            .get()
            .await
            .expect("re-read after save");
        assert_eq!(reread.label, "changed", "the composite-key save persisted");

        // Instance delete also works (ADR 0045): it removes exactly the keyed row.
        let removed = reread
            .delete()
            .await
            .expect("instance delete on a composite key");
        assert_eq!(removed, 1, "exactly the (eu, mad) row is deleted");

        // It is gone: the full-key lookup now finds nothing, and the table holds
        // the two rows that were left untouched.
        let gone = RegionCode::objects()
            .filter(
                region_code::region
                    .eq("eu")
                    .and(region_code::code.eq("mad")),
            )
            .count()
            .await
            .expect("count after delete");
        assert_eq!(gone, 0, "the deleted composite-key row is gone");
        let remaining = RegionCode::objects()
            .all()
            .count()
            .await
            .expect("total count");
        assert_eq!(remaining, 2, "only the keyed row was removed");
    });
}
