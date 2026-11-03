//! Native PostgreSQL `ENUM` types against a real database (ADR 0046).
//!
//! ```text
//! DELONIX_ORM_TEST_DSN='host=/var/run/postgresql port=5432 user=walter dbname=ormtest' \
//!   cargo test -p dorm-models --test enum_types_live -- --ignored --nocapture
//! ```
//!
//! An enum field is a `String`-valued column backed by a native `CREATE TYPE … AS
//! ENUM (…)`. The type name rides on `FieldType::Enum`; the variants live in the
//! `EnumMeta` inventory slice, so `Value` gains no variant and the database
//! enforces the label set.

use dorm_core::error::Error;
use dorm_core::model::Model;
use dorm_core::value::FieldType;
use dorm_derive::Model;
use tokio::runtime::Runtime;

/// An order whose `status` is one of three labels — enforced by the database, not
/// just the application.
#[derive(Model, Debug, Clone)]
#[model(table = "l5_orders", ordering = "id")]
struct Order {
    #[field(primary_key)]
    id: i64,
    #[field(max_length = 60)]
    reference: String,
    #[field(enum_type = "order_status", enum_variants = "pending, paid, shipped")]
    status: String,
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

/// Pure shape, no database: the field is `FieldType::Enum`, its variants resolve
/// through the slice, and the DDL creates the type and uses it as the column type.
#[test]
fn the_enum_type_resolves_and_the_ddl_creates_and_uses_it() {
    let status = Order::meta().field("status").expect("status field");
    assert_eq!(status.ty, FieldType::Enum("order_status"));
    assert_eq!(status.ty.sql_type(), "order_status");

    assert_eq!(
        dorm_core::meta::enum_variants("order_status"),
        Some(&["pending", "paid", "shipped"][..])
    );

    let types = dorm_migrations::ddl::create_enum_types(
        &[Order::meta()],
        dorm_migrations::ddl::Backend::Postgres,
    );
    assert_eq!(types.len(), 1, "one CREATE TYPE: {types:?}");
    assert!(
        types[0].contains("CREATE TYPE order_status AS ENUM ('pending', 'paid', 'shipped')"),
        "{types:?}"
    );

    let ddl = dorm_migrations::ddl::create_table(Order::meta()).expect("ddl");
    assert!(
        ddl.contains("status order_status"),
        "the column uses the enum type: {ddl}"
    );
}

#[test]
#[ignore = "needs a Postgres (DELONIX_ORM_TEST_DSN)"]
fn the_database_enforces_the_enum_labels() {
    let Some(dsn) = dsn() else {
        eprintln!("skipped: DELONIX_ORM_TEST_DSN is not set");
        return;
    };
    std::env::set_var(dorm_core::db::DSN_ENV, &dsn);

    runtime().block_on(async {
        dorm_core::db::execute("DROP TABLE IF EXISTS l5_orders", &[])
            .await
            .expect("clean table");
        dorm_core::db::execute("DROP TYPE IF EXISTS order_status", &[])
            .await
            .expect("clean type");

        // `sync` creates the enum type (before the table) and the table.
        dorm_migrations::sync(&[Order::meta()])
            .await
            .expect("sync creates the type and the table");

        // A valid label inserts and reads back as its string.
        Order::objects()
            .create(NewOrder {
                reference: "ord-1".into(),
                status: "paid".into(),
            })
            .await
            .expect("a valid label inserts");
        let got = Order::objects()
            .filter(order::status.eq("paid"))
            .get()
            .await
            .expect("lookup by enum value");
        assert_eq!(got.status, "paid");
        assert_eq!(got.reference, "ord-1");

        // An invalid label is refused by the database, not silently stored.
        let bad = Order::objects()
            .create(NewOrder {
                reference: "ord-2".into(),
                status: "bogus".into(),
            })
            .await;
        assert!(
            matches!(bad, Err(Error::Db(ref m)) if m.contains("invalid input value")
                && m.contains("order_status")),
            "an out-of-set label is a database error naming the enum, got {bad:?}"
        );

        // The second sync is a no-op — the idempotent CREATE TYPE guard does not
        // fail on the existing type.
        let again = dorm_migrations::sync(&[Order::meta()])
            .await
            .expect("second sync is idempotent");
        assert!(
            again.is_empty(),
            "nothing to change on a second sync, got {again:?}"
        );

        eprintln!("ok: the native enum type enforces its labels and sync is idempotent");
    });
}
