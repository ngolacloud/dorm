//! Idempotency keys against a real PostgreSQL (ADR 0032 / L13).
//!
//! ```text
//! DELONIX_ORM_TEST_DSN='host=127.0.0.1 port=5432 user=orm dbname=ormtest' \
//!   cargo test -p dorm-core --test idempotency_live -- --ignored --nocapture
//! ```

use dorm_core::idempotency::{begin, finish, migrations, purge_expired, Begin};
use dorm_core::{db, Error};

fn dsn() -> Option<String> {
    std::env::var("DELONIX_ORM_TEST_DSN")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Postgres (DELONIX_ORM_TEST_DSN)"]
async fn a_key_replays_its_response_blocks_a_different_body_and_purges() {
    let Some(dsn) = dsn() else {
        eprintln!("skipped: DELONIX_ORM_TEST_DSN is not set");
        return;
    };
    std::env::set_var(dorm_core::db::DSN_ENV, &dsn);

    for stmt in migrations() {
        db::execute(&stmt, &[]).await.expect("schema");
    }
    db::execute("TRUNCATE dorm_idempotency", &[])
        .await
        .expect("truncate");

    let resp = serde_json::json!({ "id": 7, "status": "created" });

    // First use: Fresh. Do the work, then store the response.
    assert_eq!(begin("k1", "hash-1", 3600).await.unwrap(), Begin::Fresh);
    finish("k1", &resp).await.unwrap();

    // A retry of the same request replays the stored response, does nothing new.
    match begin("k1", "hash-1", 3600).await.unwrap() {
        Begin::Replay(r) => assert_eq!(r, resp, "the first response is replayed"),
        other => panic!("expected Replay, got {other:?}"),
    }

    // The same key with a *different* body is refused.
    match begin("k1", "hash-DIFFERENT", 3600).await {
        Err(Error::IdempotencyMismatch { key }) => assert_eq!(key, "k1"),
        other => panic!("expected IdempotencyMismatch, got {other:?}"),
    }

    // A concurrent retry while the first is still running: InProgress.
    assert_eq!(begin("k2", "hash-2", 3600).await.unwrap(), Begin::Fresh);
    assert_eq!(
        begin("k2", "hash-2", 3600).await.unwrap(),
        Begin::InProgress,
        "a second call before finish() is in-progress, not a replay"
    );

    // Expiry + purge: a key with a zero TTL is already expired and purged, after
    // which the key is free again.
    assert_eq!(begin("k3", "hash-3", 0).await.unwrap(), Begin::Fresh);
    let purged = purge_expired().await.unwrap();
    assert!(purged >= 1, "the expired key was purged (got {purged})");
    assert_eq!(
        begin("k3", "hash-3", 3600).await.unwrap(),
        Begin::Fresh,
        "after purge the key is free again"
    );
}
