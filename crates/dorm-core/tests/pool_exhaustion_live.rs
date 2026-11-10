//! A saturated pool fails fast, it does not retry the timeout (B-1).
//!
//! `acquire` retries a transient connect/backend fault with backoff, but a
//! `PoolError::Timeout` means the pool is *full right now* — backpressure, not a
//! fault. Retrying it turned a ~`wait_timeout` failure into a
//! `RETRY_MAX × wait_timeout` one (~1.5 s for the defaults), amplifying contention
//! upstream. This proves the fix end to end: against a live `max_size = 1` pool
//! whose single connection is held, a second acquisition errors in about one
//! `wait_timeout`, not three.
//!
//! ```text
//! DELONIX_ORM_TEST_DSN='host=/var/run/postgresql user=walter dbname=dorm_af1' \
//!   cargo test -p dorm-core --test pool_exhaustion_live -- --ignored --nocapture
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use dorm_core::db::{self, ConnectOptions, Database};
use dorm_core::Error;

fn dsn() -> Option<String> {
    std::env::var("DELONIX_ORM_TEST_DSN")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a Postgres (DELONIX_ORM_TEST_DSN)"]
async fn a_saturated_pool_fails_fast_without_retrying_the_timeout() {
    let Some(dsn) = dsn() else {
        eprintln!("skipped: DELONIX_ORM_TEST_DSN is not set");
        return;
    };

    // One connection, a short 300 ms wait: the knobs that make the timing legible.
    let wait = Duration::from_millis(300);
    let opts = ConnectOptions {
        pool_max: 1,
        acquire_timeout_ms: 300,
        ..ConnectOptions::default()
    };
    let database =
        Arc::new(Database::connect_with(&dsn, &[], false, None, &opts).expect("connect"));

    // Hold the single connection for a second inside a transaction on another task.
    let holder = {
        let database = database.clone();
        tokio::spawn(async move {
            db::with_session(database, async {
                db::atomic(|_tx| async {
                    tokio::time::sleep(Duration::from_millis(1000)).await;
                    Ok::<(), Error>(())
                })
                .await
            })
            .await
        })
    };
    // Let the holder check out the one connection before we contend for it.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // The second acquisition cannot get the connection. It must FAIL FAST — about
    // one wait_timeout, not RETRY_MAX × wait_timeout (~1.5 s before the fix).
    let started = Instant::now();
    let result =
        db::with_session(database.clone(), async { db::fetch("SELECT 1", &[]).await }).await;
    let elapsed = started.elapsed();

    // `Vec<Row>` is not `Debug`, so unwrap to the error (which is) before asserting.
    let err = match result {
        Ok(_) => panic!("a saturated pool must error, not hand out the held connection"),
        Err(e) => e,
    };
    assert!(
        matches!(err, Error::PoolExhausted(_)),
        "a saturated pool is reported as exhaustion, not a generic pool error: {err:?}"
    );
    assert!(
        elapsed < wait * 2,
        "a saturated pool must fail in ~one wait_timeout ({wait:?}), not retried 3× — took {elapsed:?}"
    );

    holder
        .await
        .expect("join the holder")
        .expect("the holder tx commits");
}
