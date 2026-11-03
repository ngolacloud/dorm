//! MySQL driver, proven live: raw CRUD round-trips every `Value` type through a
//! real MySQL 8, the positional-parameter rewrite (`?n` → bare `?`, reordered)
//! binds correctly (ADR 0018 Inc. 1), and the typed `create` path reads a row
//! back without `RETURNING` (Inc. 2b) — a serial key through `last_insert_id`,
//! and a caller-supplied `TEXT` key by reloading on the key itself.
//!
//! ```console
//! DORM_COMPAT_MYSQL_DSN='mysql://root:dorm@127.0.0.1:3306/dorm' \
//!   cargo test -p dorm-core --features mysql --test mysql_live -- --ignored --nocapture
//! ```
#![cfg(feature = "mysql")]

use chrono::{DateTime, TimeZone, Utc};
use dorm_core::prelude::*;
use dorm_core::value::{FieldType, Value};
use dorm_derive::Model;
use rust_decimal::Decimal;

/// A serial primary key (no caller value) plus a unique column — the shape the
/// no-`RETURNING` create path has to read back from `last_insert_id`, and whose
/// `UNIQUE (name)` lets the duplicate-key phase prove the unique-violation
/// mapping end to end.
#[derive(Model, Debug, Clone)]
#[model(table = "widget", ordering = "id")]
struct Widget {
    #[field(primary_key)]
    id: i64,
    #[field(max_length = 100)]
    name: String,
    qty: i64,
}

/// A caller-supplied `TEXT` primary key — the shape of the DBaaS
/// `tenant_databases.addon_name`. There is no `last_insert_id` to read back
/// (it is `0` for a non-serial insert), so the no-`RETURNING` create path has
/// to reload by the key the caller put in the row (ADR 0018 Inc. 2b follow-up).
#[derive(Model, Debug, Clone)]
#[model(table = "tenant_database", ordering = "addon_name")]
struct TenantDatabase {
    #[field(primary_key, max_length = 100)]
    addon_name: String,
    #[field(max_length = 100)]
    tenant: String,
    size_mb: i64,
}

/// The i64 primary key as a plain integer, for arithmetic on the generated keys.
fn key_of(w: &Widget) -> i64 {
    match w.pk() {
        Value::I64(n) => n,
        other => panic!("a serial i64 key, got {other:?}"),
    }
}

fn dsn() -> Option<String> {
    std::env::var("DORM_COMPAT_MYSQL_DSN")
        .or_else(|_| std::env::var("DELONIX_ORM_TEST_DSN"))
        .ok()
        .filter(|s| s.starts_with("mysql:"))
}

/// The one `#[tokio::test]`: the `mysql_async` pool is a process global whose
/// connections belong to the runtime that built it, so every phase must share one
/// runtime — two `#[tokio::test]` functions would each build their own and the
/// second would find the pool's runtime shut down. (The PostgreSQL live suite
/// shares a runtime for the same reason.)
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a throwaway MySQL; set DORM_COMPAT_MYSQL_DSN"]
async fn mysql_live() {
    let Some(dsn) = dsn() else {
        eprintln!("skipped: set DORM_COMPAT_MYSQL_DSN to a mysql:// DSN");
        return;
    };
    std::env::set_var(db::DSN_ENV, &dsn);
    value_round_trip_and_positional_params().await;
    transactions_commit_rollback_and_savepoint().await;
    typed_create_reads_back_the_generated_key().await;
    typed_create_reads_back_a_text_key().await;
}

async fn value_round_trip_and_positional_params() {
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

/// Count the rows of `t` — a tiny read used by the transaction test.
async fn count_t() -> i64 {
    db::fetch("SELECT count(*) FROM t", &[]).await.unwrap()[0]
        .try_get::<i64>(0)
        .unwrap()
}

async fn transactions_commit_rollback_and_savepoint() {
    db::run_ddl("DROP TABLE IF EXISTS t").await.unwrap();
    db::run_ddl("CREATE TABLE t (n INT NOT NULL)")
        .await
        .unwrap();

    // A committed transaction persists.
    db::atomic(|_tx| async {
        db::run("INSERT INTO t (n) VALUES (?1)", &[Value::I64(1)]).await?;
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(count_t().await, 1, "the committed insert stayed");

    // A transaction that returns Err rolls back.
    let rolled = db::atomic(|_tx| async {
        db::run("INSERT INTO t (n) VALUES (?1)", &[Value::I64(2)]).await?;
        Err::<(), _>(dorm_core::Error::Db("boom".into()))
    })
    .await;
    assert!(rolled.is_err());
    assert_eq!(count_t().await, 1, "the rolled-back insert left nothing");

    // A nested atomic is a savepoint: the inner failure undoes only its own work,
    // and the outer transaction still commits.
    db::atomic(|_tx| async {
        db::run("INSERT INTO t (n) VALUES (?1)", &[Value::I64(3)]).await?;
        let inner = db::atomic(|_tx| async {
            db::run("INSERT INTO t (n) VALUES (?1)", &[Value::I64(4)]).await?;
            Err::<(), _>(dorm_core::Error::Db("inner".into()))
        })
        .await;
        assert!(inner.is_err(), "the inner savepoint failed");
        Ok(())
    })
    .await
    .unwrap();

    // 1 and 3 committed; 2 (rolled back) and 4 (savepoint) did not.
    assert_eq!(count_t().await, 2, "only the outer inserts survived");
    let ns: Vec<i64> = db::fetch("SELECT n FROM t ORDER BY n", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.try_get::<i64>(0).unwrap())
        .collect();
    assert_eq!(ns, vec![1, 3]);
}

/// The typed create path on a backend without `RETURNING` (ADR 0018 Inc. 2b): a
/// bare `INSERT` followed by a reload keyed off `last_insert_id`, all in one
/// transaction. The proof is that a typed `create` hands back the
/// database-generated primary key without the caller supplying it.
async fn typed_create_reads_back_the_generated_key() {
    db::run_ddl("DROP TABLE IF EXISTS widget").await.unwrap();
    db::run_ddl(
        "CREATE TABLE widget (\
           id BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY, \
           name VARCHAR(100) NOT NULL UNIQUE, \
           qty BIGINT NOT NULL)",
    )
    .await
    .expect("create the widget table");

    // create: the generated key comes back even though MySQL has no RETURNING.
    let a = Widget::objects()
        .create(NewWidget {
            name: "a".into(),
            qty: 10,
        })
        .await
        .expect("create a");
    assert!(!a.pk().is_null(), "the generated key came back");
    assert_eq!(a.name, "a");
    assert_eq!(a.qty, 10);

    // A second create advances the auto-increment by one — the reload read the
    // row the insert actually wrote, not a stale or adjacent one.
    let b = Widget::objects()
        .create(NewWidget {
            name: "b".into(),
            qty: 20,
        })
        .await
        .expect("create b");
    assert_eq!(key_of(&b), key_of(&a) + 1, "auto-increment advanced by one");

    // It really persisted, and the reload decoded every column.
    let back = Widget::objects()
        .filter(widget::id.eq(key_of(&b)))
        .first()
        .await
        .expect("read b back")
        .expect("b is there");
    assert_eq!(back.name, "b");
    assert_eq!(back.qty, 20);

    // bulk_create (single chunk): each row's generated key comes back, in order.
    let made = Widget::objects()
        .bulk_create(vec![
            NewWidget {
                name: "c".into(),
                qty: 1,
            },
            NewWidget {
                name: "d".into(),
                qty: 2,
            },
        ])
        .await
        .expect("bulk_create");
    assert_eq!(made.len(), 2);
    assert!(
        made.iter().all(|w| !w.pk().is_null()),
        "every bulk key came back"
    );
    assert_eq!((made[0].name.as_str(), made[1].name.as_str()), ("c", "d"));
    assert_eq!(
        key_of(&made[1]),
        key_of(&made[0]) + 1,
        "the block of keys is contiguous and ascending"
    );

    // get_or_create: creates once, then finds the same row — the Django contract,
    // now on MySQL.
    let (first, created) = Widget::objects()
        .get_or_create(
            widget::name.eq("shared"),
            NewWidget {
                name: "shared".into(),
                qty: 0,
            },
        )
        .await
        .expect("get_or_create creates");
    assert!(created, "the first call created the row");
    let (again, created_again) = Widget::objects()
        .get_or_create(
            widget::name.eq("shared"),
            NewWidget {
                name: "shared".into(),
                qty: 99,
            },
        )
        .await
        .expect("get_or_create finds");
    assert!(!created_again, "the second call found the existing row");
    assert_eq!(first.pk(), again.pk(), "same row, same key");

    // A duplicate key surfaces as a recognised unique violation (code 1062) — the
    // mapping `get_or_create` leans on, proven against a real server.
    Widget::objects()
        .create(NewWidget {
            name: "unique".into(),
            qty: 1,
        })
        .await
        .expect("the first insert of `unique` wins");
    let dup = Widget::objects()
        .create(NewWidget {
            name: "unique".into(),
            qty: 2,
        })
        .await;
    let err = dup.expect_err("a duplicate name is rejected");
    assert!(
        dorm_core::error::is_unique_violation(&err),
        "recognised as a unique violation, got {err:?}"
    );
}

/// The typed create path for a caller-supplied `TEXT` primary key (ADR 0018 Inc.
/// 2b follow-up). There is no serial key to read from `last_insert_id` — it is
/// `0` — so the proof is that `create` reloads and returns the row keyed by the
/// `addon_name` the caller handed in, with its other columns decoded.
async fn typed_create_reads_back_a_text_key() {
    db::run_ddl("DROP TABLE IF EXISTS tenant_database")
        .await
        .unwrap();
    db::run_ddl(
        "CREATE TABLE tenant_database (\
           addon_name VARCHAR(100) NOT NULL PRIMARY KEY, \
           tenant VARCHAR(100) NOT NULL, \
           size_mb BIGINT NOT NULL)",
    )
    .await
    .expect("create the tenant_database table");

    // create: the row comes back keyed by the caller's text key, not by a
    // last_insert_id (which is 0 here).
    let made = TenantDatabase::objects()
        .create(NewTenantDatabase {
            addon_name: "acme-prod".into(),
            tenant: "acme".into(),
            size_mb: 512,
        })
        .await
        .expect("create a text-keyed row");
    assert_eq!(
        made.pk(),
        Value::Text("acme-prod".into()),
        "the caller's text key came back"
    );
    assert_eq!(made.addon_name, "acme-prod");
    assert_eq!(made.tenant, "acme");
    assert_eq!(made.size_mb, 512, "the other columns decoded");

    // It persisted under that key and the reload read the row the insert wrote.
    let back = TenantDatabase::objects()
        .filter(tenant_database::addon_name.eq("acme-prod"))
        .first()
        .await
        .expect("read it back")
        .expect("the row is there");
    assert_eq!(back.tenant, "acme");
    assert_eq!(back.size_mb, 512);

    // bulk_create (single chunk): each text key comes back with its own row.
    let many = TenantDatabase::objects()
        .bulk_create(vec![
            NewTenantDatabase {
                addon_name: "beta-1".into(),
                tenant: "beta".into(),
                size_mb: 1,
            },
            NewTenantDatabase {
                addon_name: "beta-2".into(),
                tenant: "beta".into(),
                size_mb: 2,
            },
        ])
        .await
        .expect("bulk_create text-keyed rows");
    assert_eq!(many.len(), 2);
    let keys: Vec<String> = many.iter().map(|t| t.addon_name.clone()).collect();
    assert!(
        keys.contains(&"beta-1".to_string()) && keys.contains(&"beta-2".to_string()),
        "both text keys came back, got {keys:?}"
    );
}
