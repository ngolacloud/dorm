//! Multi-replica read routing: round-robin balancing, ejection of an unhealthy
//! replica, and fallback to the primary (ADR 0050 / L8) against a live streaming
//! primary + two hot standbys.
//!
//! ```text
//! DORM_PRIMARY_DSN='host=127.0.0.1 port=5434 user=walter dbname=ormtest' \
//! DORM_REPLICA1_DSN='host=127.0.0.1 port=5435 user=walter dbname=ormtest' \
//! DORM_REPLICA2_DSN='host=127.0.0.1 port=5436 user=walter dbname=ormtest' \
//!   cargo test -p dorm-core --test multi_replica_routing_live -- --ignored --nocapture
//! ```
//!
//! Each backend answers `inet_server_port()` with the port it listens on, so a
//! read's reply says which server served it — the witness for where routing sent
//! it, no shared state required.

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

async fn primary_port() -> i32 {
    db::query("SELECT inet_server_port()", &[])
        .await
        .expect("primary read")[0]
        .get::<_, i32>(0)
}

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    })
}

#[test]
#[ignore = "needs a live primary + two replicas (DORM_PRIMARY_DSN/DORM_REPLICA1_DSN/DORM_REPLICA2_DSN)"]
fn reads_balance_eject_and_fall_back() {
    let (Some(p), Some(r1), Some(r2)) = (
        var("DORM_PRIMARY_DSN"),
        var("DORM_REPLICA1_DSN"),
        var("DORM_REPLICA2_DSN"),
    ) else {
        eprintln!("skipped: set DORM_PRIMARY_DSN / DORM_REPLICA1_DSN / DORM_REPLICA2_DSN");
        return;
    };
    // A DSN to a port nothing listens on — a deterministically-unhealthy replica.
    let dead = "host=127.0.0.1 port=5999 user=walter dbname=ormtest connect_timeout=1";
    let dead2 = "host=127.0.0.1 port=5998 user=walter dbname=ormtest connect_timeout=1";

    runtime().block_on(async {
        let pport = {
            let db = Arc::new(Database::connect(&p, None, false).expect("primary only"));
            db::with_session(db, async { primary_port().await }).await
        };

        // 1. Balancing: two healthy replicas, reads spread round-robin across both.
        {
            let db = Arc::new(
                Database::connect_replicas(&p, &[&r1, &r2], false).expect("two replicas"),
            );
            assert_eq!(db.replica_count(), 2);
            let ports = db::with_session(db, async {
                let mut seen = std::collections::BTreeSet::new();
                for _ in 0..20 {
                    seen.insert(read_port().await);
                }
                seen
            })
            .await;
            assert_eq!(ports.len(), 2, "reads should balance across both replicas: {ports:?}");
            assert!(!ports.contains(&pport), "a read must not land on the primary: {ports:?}");
        }

        // 2. Ejection: one healthy replica, one dead. Every read still succeeds, and
        //    after the dead one is ejected they are all served by the healthy one.
        {
            let db = Arc::new(
                Database::connect_replicas(&p, &[&r1, dead], false).expect("one dead replica"),
            );
            let ports = db::with_session(db, async {
                let mut seen = std::collections::BTreeSet::new();
                for _ in 0..20 {
                    seen.insert(read_port().await); // never panics → the dead one never errors a read
                }
                seen
            })
            .await;
            assert_eq!(ports.len(), 1, "the dead replica is ejected, reads go to the healthy one: {ports:?}");
            assert!(!ports.contains(&pport), "the healthy replica, not the primary: {ports:?}");
        }

        // 3. All replicas down: reads fall back to the primary rather than erroring.
        {
            let db = Arc::new(
                Database::connect_replicas(&p, &[dead, dead2], false).expect("all dead"),
            );
            let port = db::with_session(db, async { read_port().await }).await;
            assert_eq!(port, pport, "with every replica down a read falls back to the primary");
        }

        // 4. A write (primary route) always hits the primary, replicas or not.
        {
            let db = Arc::new(
                Database::connect_replicas(&p, &[&r1, &r2], false).expect("two replicas"),
            );
            let port = db::with_session(db, async { primary_port().await }).await;
            assert_eq!(port, pport, "the primary route ignores replicas");
        }

        eprintln!("ok: reads balanced across 2 replicas, ejected a dead one, fell back to primary, writes stayed on primary (primary port {pport})");
    });
}
