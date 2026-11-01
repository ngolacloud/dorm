//! MySQL driver, proven live (ADR 0018 Inc. 1): raw CRUD round-trips every
//! `Value` type through a real MySQL 8, and the positional-parameter rewrite
//! (`?n` → bare `?`, reordered) binds correctly. The typed `create` path (which
//! needs `last_insert_id` in place of `RETURNING`) is the next increment; this
//! proves the `mysql_async` driver plugs into the `Driver` seam.
//!
//! ```console
//! DORM_COMPAT_MYSQL_DSN='mysql://root:dorm@127.0.0.1:3306/dorm' \
//!   cargo test -p dorm-core --features mysql --test mysql_live -- --ignored --nocapture
//! ```
#![cfg(feature = "mysql")]

use chrono::{DateTime, TimeZone, Utc};
use dorm_core::db;
use dorm_core::value::{FieldType, Value};
use rust_decimal::Decimal;

fn dsn() -> Option<String> {
    std::env::var("DORM_COMPAT_MYSQL_DSN")
        .or_else(|_| std::env::var("DELONIX_ORM_TEST_DSN"))
        .ok()
        .filter(|s| s.starts_with("mysql:"))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a throwaway MySQL; set DORM_COMPAT_MYSQL_DSN"]
async fn every_value_type_round_trips_and_positional_params_bind() {
    let Some(dsn) = dsn() else {
        eprintln!("skipped: set DORM_COMPAT_MYSQL_DSN to a mysql:// DSN");
        return;
    };
    std::env::set_var(db::DSN_ENV, &dsn);

    db::run_ddl("DROP TABLE IF EXISTS probe").await.unwrap();
    db::run_ddl(
        "CREATE TABLE probe (\
           id BIGINT, name TEXT, price DECIMAL(10,2), flag TINYINT(1), \
           ratio DOUBLE, data LONGBLOB, ts DATETIME(6))",
    )
    .await
    .expect("create the probe table");

    let ts = Utc.with_ymd_and_hms(2024, 1, 2, 3, 4, 5).unwrap();
    let blob = vec![0u8, 1, 2, 250, 255];
    // Numbered `?n` — the MySQL driver converts to bare positional `?`.
    let affected = db::run(
        "INSERT INTO probe (id, name, price, flag, ratio, data, ts) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        &[
            Value::I64(7),
            Value::Text("acme".into()),
            Value::Decimal(Decimal::new(1999, 2)),
            Value::Bool(true),
            Value::F64(1.5),
            Value::Bytes(blob.clone()),
            Value::Timestamp(ts),
        ],
    )
    .await
    .expect("insert one row");
    assert_eq!(affected, 1);

    let rows = db::fetch(
        "SELECT id, name, price, flag, ratio, data, ts FROM probe",
        &[],
    )
    .await
    .expect("read the row back");
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    assert_eq!(r.try_get::<i64>(0).unwrap(), 7);
    assert_eq!(r.try_get::<String>(1).unwrap(), "acme");
    assert_eq!(
        r.try_get::<Decimal>(2).unwrap(),
        Decimal::new(1999, 2),
        "cents kept"
    );
    assert!(
        r.try_get::<bool>(3).unwrap(),
        "TINYINT(1) reads back as true"
    );
    assert_eq!(r.try_get::<f64>(4).unwrap(), 1.5);
    assert_eq!(r.try_get::<Vec<u8>>(5).unwrap(), blob, "blob byte-for-byte");
    assert_eq!(
        r.try_get::<DateTime<Utc>>(6).unwrap(),
        ts,
        "UTC datetime round-trips"
    );

    // The model-erased reader decodes at the declared types too.
    assert_eq!(
        dorm_core::value::read_column(r, 2, FieldType::Decimal, "price").unwrap(),
        Value::Decimal(Decimal::new(1999, 2))
    );
    assert_eq!(
        dorm_core::value::read_column(r, 6, FieldType::Timestamp, "ts").unwrap(),
        Value::Timestamp(ts)
    );

    // Positional binding: `?2 … ?1` must bind the values in textual order.
    let swapped = db::fetch("SELECT ?2 AS a, ?1 AS b", &[Value::I64(1), Value::I64(2)])
        .await
        .expect("positional reorder");
    assert_eq!(
        swapped[0].try_get::<i64>(0).unwrap(),
        2,
        "?2 is the second param"
    );
    assert_eq!(
        swapped[0].try_get::<i64>(1).unwrap(),
        1,
        "?1 is the first param"
    );
}
