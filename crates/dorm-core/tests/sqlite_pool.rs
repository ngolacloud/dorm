//! The SQLite reader pool on a file database (ADR 0017): WAL is on, reads run
//! concurrently through pooled reader connections while the writer writes, and a
//! read still sees a committed write. No server — just a temp file.
//!
//! ```text
//! cargo test -p dorm-core --features sqlite --test sqlite_pool
//! ```
#![cfg(feature = "sqlite")]

use dorm_core::db;
use dorm_core::value::Value;

fn temp_db() -> String {
    // A fixed name in the temp dir, cleared up front so a rerun starts clean.
    let base = std::env::temp_dir().join("dorm_pool_test.db");
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", base.display()));
    }
    base.display().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn file_backed_sqlite_pools_readers_under_wal() {
    let path = temp_db();
    std::env::set_var(db::DSN_ENV, format!("sqlite:{path}"));

    // Writer: create the table and insert rows (one shared writer connection).
    db::run_ddl("CREATE TABLE n (v INTEGER NOT NULL)")
        .await
        .unwrap();
    for i in 0..50 {
        db::run("INSERT INTO n (v) VALUES (?1)", &[Value::I64(i)])
            .await
            .unwrap();
    }

    // The database is in WAL mode — the property that lets readers not block the
    // writer. A pooled reader reports it.
    let mode = db::sql("PRAGMA journal_mode", &[]).await.unwrap()[0]
        .try_get::<String>(0)
        .unwrap();
    assert_eq!(
        mode.to_lowercase(),
        "wal",
        "the file database is in WAL mode"
    );

    // Many concurrent reads: each is served by a pooled reader connection, all in
    // flight at once, and every one sees all 50 rows.
    let tasks: Vec<_> = (0..16)
        .map(|_| {
            tokio::spawn(async {
                let rows = db::sql("SELECT v FROM n ORDER BY v", &[]).await.unwrap();
                rows.len()
            })
        })
        .collect();
    for t in tasks {
        assert_eq!(t.await.unwrap(), 50, "every concurrent read sees all rows");
    }

    // Read-your-writes across autocommit: a write on the writer is visible to the
    // very next pooled read, because a WAL reader sees the latest commit.
    db::run("INSERT INTO n (v) VALUES (?1)", &[Value::I64(999)])
        .await
        .unwrap();
    let count = db::sql("SELECT count(*) FROM n", &[]).await.unwrap()[0]
        .try_get::<i64>(0)
        .unwrap();
    assert_eq!(count, 51, "the committed write is visible to the next read");

    // A read inside a transaction stays on the writer and sees its own uncommitted
    // write, then the rollback undoes it.
    let outcome = db::atomic(|_tx| async {
        db::run("INSERT INTO n (v) VALUES (?1)", &[Value::I64(-1)]).await?;
        let seen = db::sql("SELECT count(*) FROM n", &[]).await?[0].try_get::<i64>(0)?;
        assert_eq!(
            seen, 52,
            "a read inside the tx sees its own uncommitted write"
        );
        Err::<(), _>(dorm_core::Error::Db("roll it back".into()))
    })
    .await;
    assert!(outcome.is_err());
    let after = db::sql("SELECT count(*) FROM n", &[]).await.unwrap()[0]
        .try_get::<i64>(0)
        .unwrap();
    assert_eq!(after, 51, "the rolled-back insert left nothing");
}
