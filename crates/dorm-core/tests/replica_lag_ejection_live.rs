//! Proactive lag-based ejection of a read replica (ADR 0050 follow-up / L8)
//! against a live streaming primary + standby.
//!
//! ```text
//! DORM_PRIMARY_DSN='host=127.0.0.1 port=5444 user=walter dbname=ormtest' \
//! DORM_REPLICA_DSN='host=127.0.0.1 port=5445 user=walter dbname=ormtest' \
//!   cargo test -p dorm-core --test replica_lag_ejection_live -- --ignored --nocapture
//! ```
//!
//! Pausing the standby's WAL replay freezes its last-replayed timestamp, so its
//! lag grows with wall-clock. `Database::eject_lagging_replicas` must then eject
//! it, and reads fall back to the primary until it catches up.

use std::sync::Arc;

use dorm_core::db::{self, Database};

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.trim().is_empty())
}

async fn read_port() -> i32 {
    db::query_read("SELECT inet_server_port()", &[])
        .await
        .expect("read")[0]
        .get::<_, i32>(0)
}

/// Run a statement straight on the replica, bypassing DORM's routing — needed for
/// `pg_wal_replay_pause`/`resume`, which must hit the standby itself (DORM's write
/// route goes to the primary, and an ejected replica is skipped).
async fn on_replica(replica_dsn: &str, sql: &str) {
    let (client, conn) = tokio_postgres::connect(replica_dsn, tokio_postgres::NoTls)
        .await
        .expect("connect to replica");
    let handle = tokio::spawn(conn);
    client.simple_query(sql).await.expect("replica stmt");
    handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live primary + standby (DORM_PRIMARY_DSN / DORM_REPLICA_DSN)"]
async fn a_lagging_replica_is_ejected_and_reads_fall_back_to_the_primary() {
    let (Some(primary), Some(replica)) = (var("DORM_PRIMARY_DSN"), var("DORM_REPLICA_DSN")) else {
        eprintln!("skipped: set DORM_PRIMARY_DSN and DORM_REPLICA_DSN");
        return;
    };

    let db = Arc::new(Database::connect(&primary, Some(&replica), false).expect("connect"));
    let (primary_port, replica_port) = db::with_session(db.clone(), async {
        let p = db::query("SELECT inet_server_port()", &[]).await.unwrap()[0].get::<_, i32>(0);
        let r = read_port().await;
        (p, r)
    })
    .await;
    assert_ne!(
        primary_port, replica_port,
        "the replica must be its own server"
    );

    // While the standby replays, it is caught up: reads go to it.
    let ejected = db
        .eject_lagging_replicas(60.0)
        .await
        .expect("lag check (caught up)");
    assert_eq!(ejected, 0, "a caught-up replica is not ejected");
    let port = db::with_session(db.clone(), async { read_port().await }).await;
    assert_eq!(port, replica_port, "a healthy replica still serves reads");

    // Freeze the standby: pause its WAL replay, then write on the primary so the
    // standby *receives* WAL it cannot *replay* — a real gap (received ≠ replayed)
    // whose time delta then grows with wall-clock.
    on_replica(&replica, "SELECT pg_wal_replay_pause()").await;
    db::with_session(db.clone(), async {
        db::execute("DROP TABLE IF EXISTS lag_probe", &[])
            .await
            .ok();
        db::execute("CREATE TABLE lag_probe (x int)", &[])
            .await
            .ok();
        db::execute("INSERT INTO lag_probe VALUES (1)", &[])
            .await
            .ok();
    })
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    // Now the replica lags more than the threshold → ejected; reads fall back.
    let ejected = db
        .eject_lagging_replicas(0.5)
        .await
        .expect("lag check (lagging)");
    assert_eq!(ejected, 1, "the lagging replica is ejected");
    let port = db::with_session(db.clone(), async { read_port().await }).await;
    assert_eq!(
        port, primary_port,
        "with its only replica ejected, a read falls back to the primary"
    );

    // Resume replay so the box is left healthy for a re-run.
    on_replica(&replica, "SELECT pg_wal_replay_resume()").await;

    eprintln!(
        "ok: a replica paused past the lag threshold was ejected and reads fell back to the primary"
    );
}
