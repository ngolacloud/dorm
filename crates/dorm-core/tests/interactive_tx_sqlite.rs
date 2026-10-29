//! The interactive transaction API (ADR 0006, brick 2) on the SQLite backend.
//!
//! [`db::begin_interactive`] is what lets the shell hold a transaction open
//! across many `block_on`s, which [`db::atomic`]'s single-future scoping cannot
//! do. The shell builds only the PostgreSQL backend, so this is where the SQLite
//! side of the same handle is proven: a transaction opened once, driven through
//! several separate `scope` calls — standing in for the shell's separate REPL
//! lines — then committed, rolled back, or abandoned.
//!
//! Compiled only with the `sqlite` feature; without it the crate has no SQLite
//! backend and the file is empty.
#![cfg(feature = "sqlite")]

use dorm_core::db;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a tokio runtime")
}

#[test]
fn an_interactive_transaction_commits_rolls_back_and_rolls_back_when_abandoned() {
    // The backend follows from the DSN; nothing here names SQLite but the DSN.
    std::env::set_var(db::DSN_ENV, "sqlite::memory:");
    let rt = runtime();

    rt.block_on(async {
        db::run_ddl("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .await
            .expect("create the table");

        // --- commit: several statements across separate `scope`s, one atom ---
        let tx = db::begin_interactive().await.expect("begin");
        tx.scope(async { db::sql_execute("INSERT INTO t (v) VALUES ('a')", &[]).await })
            .await
            .expect("first insert");
        tx.scope(async { db::sql_execute("INSERT INTO t (v) VALUES ('b')", &[]).await })
            .await
            .expect("second insert");
        // The session sees its own uncommitted work.
        let inside = tx
            .scope(async { db::sql("SELECT v FROM t", &[]).await })
            .await
            .expect("read inside the transaction");
        assert_eq!(inside.len(), 2, "both inserts are visible inside the tx");
        tx.commit().await.expect("commit");
        // And they survive the commit for a reader outside any transaction.
        let after = db::sql("SELECT v FROM t", &[])
            .await
            .expect("read after commit");
        assert_eq!(after.len(), 2, "the committed rows persist");

        // --- rollback: a delete seen inside, then discarded ---
        let tx = db::begin_interactive().await.expect("begin");
        tx.scope(async { db::sql_execute("DELETE FROM t", &[]).await })
            .await
            .expect("delete");
        let mid = tx
            .scope(async { db::sql("SELECT v FROM t", &[]).await })
            .await
            .expect("read inside");
        assert_eq!(mid.len(), 0, "the delete is visible inside the tx");
        tx.rollback().await.expect("rollback");
        let after = db::sql("SELECT v FROM t", &[])
            .await
            .expect("read after rollback");
        assert_eq!(after.len(), 2, "rollback restored the rows");

        // --- abandoned: dropped without commit rolls back ---
        {
            let tx = db::begin_interactive().await.expect("begin");
            tx.scope(async { db::sql_execute("INSERT INTO t (v) VALUES ('c')", &[]).await })
                .await
                .expect("insert");
            // `tx` drops here with no commit — its `Drop` issues a best-effort
            // ROLLBACK and releases the connection lock.
        }
        let after = db::sql("SELECT v FROM t", &[])
            .await
            .expect("read after drop");
        assert_eq!(after.len(), 2, "an abandoned transaction wrote nothing");
    });
}
