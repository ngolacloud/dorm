//! Live proof of the event store (ADR 0039 / L12), gated on a real PostgreSQL.
//!
//! ```text
//! DELONIX_ORM_TEST_DSN='host=/var/run/postgresql port=5432 user=walter dbname=ormtest' \
//!   cargo test -p dorm-eventstore --test eventstore_live -- --ignored --nocapture
//! ```
//!
//! It exercises, against the server, every claim the crate makes: an optimistic
//! append, a refused append at a stale version, an ordered load, snapshot + load
//! since snapshot, and a stored v1 event upcasted to v2 on load.

use dorm_core::db;
use dorm_core::error::Error;
use dorm_eventstore::{
    append, load, load_from_snapshot, migrations, save_snapshot, NewEvent, Upcasters, EVENTS,
    SNAPSHOTS,
};
use tokio::runtime::Runtime;

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

#[test]
#[ignore = "needs a Postgres (DELONIX_ORM_TEST_DSN)"]
fn the_event_store_appends_conflicts_loads_snapshots_and_upcasts() {
    let Some(dsn) = dsn() else {
        eprintln!("skipped: DELONIX_ORM_TEST_DSN is not set");
        return;
    };
    std::env::set_var(dorm_core::db::DSN_ENV, &dsn);

    // A unique aggregate id per run, so the test is re-runnable without cross-run
    // interference even before the TRUNCATE below lands.
    let run = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let acct = format!("acct-{run}");
    let kind = "Account";

    runtime().block_on(async {
        for stmt in migrations() {
            db::execute(&stmt, &[]).await.expect("event-store schema");
        }
        // Re-runnable: wipe the two tables at the start.
        for t in [EVENTS, SNAPSHOTS] {
            db::execute(&format!("TRUNCATE {t}"), &[])
                .await
                .expect("truncate");
        }

        // 1) Append three events to a fresh aggregate (expected_version 0).
        let v = append(
            kind,
            &acct,
            0,
            &[
                NewEvent::new("Opened", serde_json::json!({"owner": "ada", "balance": 0})),
                NewEvent::new("Deposited", serde_json::json!({"amount": 100})),
                NewEvent::new("Withdrew", serde_json::json!({"amount": 30})),
            ],
        )
        .await
        .expect("first append");
        assert_eq!(v, 3, "three events → version 3");

        // 2) A stale append (still believing the stream is at version 1) is refused
        //    as a concurrency conflict, and writes nothing.
        let err = append(
            kind,
            &acct,
            1,
            &[NewEvent::new("Deposited", serde_json::json!({"amount": 5}))],
        )
        .await
        .expect_err("stale append must conflict");
        match err {
            Error::Conflict {
                expected, actual, ..
            } => {
                assert_eq!(expected, 1);
                assert_eq!(
                    actual,
                    Some(3),
                    "the store reports the real current version"
                );
            }
            other => panic!("expected a version conflict, got {other:?}"),
        }

        // 3) load reconstructs the stream strictly in version order.
        let stream = load(kind, &acct).await.expect("load");
        assert_eq!(stream.len(), 3);
        assert_eq!(
            stream.iter().map(|e| e.version).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "ordered by version"
        );
        assert_eq!(stream[0].event_type, "Opened");
        assert_eq!(stream[1].data["amount"], serde_json::json!(100));
        // global_seq is monotonic across the stream (total commit order).
        assert!(
            stream[0].global_seq < stream[2].global_seq,
            "global_seq grows"
        );

        // A correct append continues the stream from version 3.
        let v = append(
            kind,
            &acct,
            3,
            &[NewEvent::new(
                "Deposited",
                serde_json::json!({"amount": 50}),
            )],
        )
        .await
        .expect("continuation append");
        assert_eq!(v, 4);

        // 4) Snapshot the folded balance at version 4, then load_from_snapshot
        //    returns the snapshot plus only the events recorded *after* it.
        let balance = 100 - 30 + 50; // folded from the stream (open 0 + 100 − 30 + 50)
        save_snapshot(kind, &acct, 4, &serde_json::json!({"balance": balance}))
            .await
            .expect("save snapshot");
        append(
            kind,
            &acct,
            4,
            &[NewEvent::new("Withdrew", serde_json::json!({"amount": 20}))],
        )
        .await
        .expect("post-snapshot append");

        let (snap, since) = load_from_snapshot(kind, &acct)
            .await
            .expect("load from snapshot");
        let snap = snap.expect("a snapshot exists");
        assert_eq!(snap.version, 4);
        assert_eq!(snap.state["balance"], serde_json::json!(120));
        assert_eq!(since.len(), 1, "only the one event after the snapshot");
        assert_eq!(since[0].version, 5);
        assert_eq!(since[0].data["amount"], serde_json::json!(20));
        // Fast rebuild: fold the tail onto the snapshot.
        let rebuilt =
            snap.state["balance"].as_i64().unwrap() - since[0].data["amount"].as_i64().unwrap();
        assert_eq!(rebuilt, 100, "snapshot(120) − withdraw(20) = 100");

        // 5) Upcasting: store a v1 "Renamed" event, register a v1→v2 upcaster, and
        //    load it already lifted to v2.
        let who = format!("user-{run}");
        append(
            "User",
            &who,
            0,
            &[NewEvent::new("Renamed", serde_json::json!({"name": "Ada Lovelace"})).at_version(1)],
        )
        .await
        .expect("append a legacy v1 event");

        let raw = load("User", &who).await.expect("raw load");
        assert_eq!(raw[0].event_version, 1, "stored on disk as v1");
        assert_eq!(
            raw[0].data.get("first"),
            None,
            "v1 payload has no split name"
        );

        let mut up = Upcasters::new();
        up.register("Renamed", 1, |mut d| {
            let name = d["name"].as_str().unwrap_or("").to_string();
            let (first, last) = name.split_once(' ').unwrap_or((name.as_str(), ""));
            d["first"] = serde_json::json!(first);
            d["last"] = serde_json::json!(last);
            d
        });
        let upcasted = up.load("User", &who).await.expect("upcasting load");
        assert_eq!(upcasted[0].event_version, 2, "lifted to v2 on load");
        assert_eq!(upcasted[0].data["first"], serde_json::json!("Ada"));
        assert_eq!(upcasted[0].data["last"], serde_json::json!("Lovelace"));

        eprintln!("event-store live proof OK");
    });
}
