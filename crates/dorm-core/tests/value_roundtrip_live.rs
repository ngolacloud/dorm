//! Round-trip proof for the widened `Value` set (ADR 0016): a `DOUBLE
//! PRECISION`/`REAL` and a `BYTEA`/`BLOB` column bind and read back byte-for-byte
//! on both backends. The SQLite half needs no server; the PostgreSQL half is
//! ignored by default and needs a throwaway database:
//!
//! ```console
//! DELONIX_ORM_TEST_DSN='host=127.0.0.1 port=55432 user=dorm password=dorm dbname=dorm_dev' \
//!   cargo test -p dorm-core --features sqlite --test value_roundtrip_live -- --include-ignored
//! ```

use dorm_core::db;
use dorm_core::value::{FieldType, Value};

/// Create a two-column table, insert one float and one blob, read them back, and
/// assert they survived — against whatever backend `DELONIX_ORM_DSN` names. The
/// declared types work on both: PostgreSQL takes them literally, and SQLite maps
/// `DOUBLE PRECISION` to `REAL` affinity while storing a blob value as a blob
/// regardless of the column's affinity.
async fn roundtrip() {
    let is_pg = db::backend_of(&db::dsn().unwrap()) == db::Backend::Postgres;
    db::run_ddl("DROP TABLE IF EXISTS vt").await.unwrap();
    db::run_ddl("CREATE TABLE vt (x DOUBLE PRECISION, b BYTEA)")
        .await
        .expect("create the probe table");

    let blob = vec![0u8, 1, 2, 250, 255];
    // `$n` on PostgreSQL, `?n` on SQLite — the one placeholder difference.
    let insert = if is_pg {
        "INSERT INTO vt (x, b) VALUES ($1, $2)"
    } else {
        "INSERT INTO vt (x, b) VALUES (?1, ?2)"
    };
    let affected = db::run(insert, &[Value::F64(1.5), Value::Bytes(blob.clone())])
        .await
        .expect("insert the float and the blob");
    assert_eq!(affected, 1);

    let rows = db::fetch("SELECT x, b FROM vt", &[])
        .await
        .expect("read back");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].try_get::<f64>(0).unwrap(),
        1.5,
        "the float survived"
    );
    assert_eq!(
        rows[0].try_get::<Vec<u8>>(1).unwrap(),
        blob,
        "the blob survived byte-for-byte"
    );

    // The model-erased reader decodes them at their declared types too.
    assert_eq!(
        dorm_core::value::read_column(&rows[0], 0, FieldType::F64, "x").unwrap(),
        Value::F64(1.5)
    );
    assert_eq!(
        dorm_core::value::read_column(&rows[0], 1, FieldType::Bytes, "b").unwrap(),
        Value::Bytes(blob)
    );
}

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn float_and_bytes_round_trip_on_sqlite() {
    std::env::set_var(db::DSN_ENV, "sqlite::memory:");
    roundtrip().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a throwaway PostgreSQL; set DELONIX_ORM_TEST_DSN"]
async fn float_and_bytes_round_trip_on_postgres() {
    let Ok(dsn) = std::env::var("DELONIX_ORM_TEST_DSN").or_else(|_| std::env::var(db::DSN_ENV))
    else {
        eprintln!("skipped: set DELONIX_ORM_TEST_DSN");
        return;
    };
    std::env::set_var(db::DSN_ENV, &dsn);
    roundtrip().await;
}
