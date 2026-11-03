//! Durable job queue + leases against a real PostgreSQL (ADR 0027).
//!
//! ```text
//! DELONIX_ORM_TEST_DSN='host=127.0.0.1 port=5432 user=orm dbname=ormtest' \
//!   cargo test -p dorm-jobs --test jobs_live -- --ignored --nocapture
//! ```

use dorm_core::db;
use dorm_core::error::Error;
use dorm_jobs::*;
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
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("a tokio runtime")
    })
}

async fn setup() {
    for stmt in migrations() {
        db::execute(&stmt, &[]).await.expect("jobs schema");
    }
    for t in ["dorm_jobs", "dorm_leases"] {
        db::execute(&format!("TRUNCATE {t}"), &[])
            .await
            .expect("truncate");
    }
}

fn nothing() -> serde_json::Value {
    serde_json::json!({})
}

#[test]
#[ignore = "needs a Postgres (DELONIX_ORM_TEST_DSN)"]
fn the_queue_claims_schedules_prioritises_retries_and_leases() {
    let Some(dsn) = dsn() else {
        eprintln!("skipped: DELONIX_ORM_TEST_DSN is not set");
        return;
    };
    std::env::set_var(dorm_core::db::DSN_ENV, &dsn);

    runtime().block_on(async {
        setup().await;

        // 1) enqueue -> dequeue -> complete, and depth tracks it.
        let id = enqueue(
            "q1",
            "greet",
            &serde_json::json!({"to": "world"}),
            EnqueueOpts::default(),
        )
        .await
        .expect("enqueue")
        .expect("a new id");
        assert_eq!(depth("q1").await.unwrap(), 1);
        let job = dequeue("q1", "w1").await.unwrap().expect("a job");
        assert_eq!(job.id, id);
        assert_eq!(job.kind, "greet");
        assert_eq!(job.payload["to"], "world");
        assert_eq!(job.attempts, 1);
        complete(job.id).await.unwrap();
        assert_eq!(
            depth("q1").await.unwrap(),
            0,
            "a completed job is not pending"
        );
        assert!(dequeue("q1", "w1").await.unwrap().is_none(), "nothing left");

        // 2) unique_key: a duplicate enqueue is absorbed — one active job.
        let opts = EnqueueOpts {
            unique_key: Some("app-42".into()),
            ..Default::default()
        };
        let first = enqueue("q2", "reconcile", &nothing(), opts.clone())
            .await
            .unwrap();
        let second = enqueue("q2", "reconcile", &nothing(), opts).await.unwrap();
        assert!(first.is_some());
        assert!(second.is_none(), "the resource already has an active job");
        assert_eq!(
            depth("q2").await.unwrap(),
            1,
            "only one pending job per resource"
        );

        // 3) priority: the higher-priority job comes out first.
        enqueue(
            "q3",
            "low",
            &nothing(),
            EnqueueOpts {
                priority: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        enqueue(
            "q3",
            "high",
            &nothing(),
            EnqueueOpts {
                priority: 10,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let first = dequeue("q3", "w").await.unwrap().expect("a job");
        assert_eq!(first.kind, "high", "priority 10 before priority 1");

        // 4) scheduling: a future job is not claimed before its time.
        enqueue(
            "q4",
            "later",
            &nothing(),
            EnqueueOpts {
                delay_ms: 60_000,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(dequeue("q4", "w").await.unwrap().is_none(), "not due yet");
        assert_eq!(depth("q4").await.unwrap(), 1, "it is pending, just not due");

        // 5) SKIP LOCKED: two workers racing a two-job queue take different jobs.
        let a = enqueue("q5", "a", &nothing(), EnqueueOpts::default())
            .await
            .unwrap()
            .unwrap();
        let b = enqueue("q5", "b", &nothing(), EnqueueOpts::default())
            .await
            .unwrap()
            .unwrap();
        let (ja, jb) = tokio::join!(dequeue("q5", "wA"), dequeue("q5", "wB"));
        let ja = ja.unwrap().expect("A got a job");
        let jb = jb.unwrap().expect("B got a job");
        assert_ne!(ja.id, jb.id, "two workers never take the same job");
        let mut got = [ja.id, jb.id];
        got.sort();
        let mut want = [a, b];
        want.sort();
        assert_eq!(got, want, "between them they took both jobs");

        // 6) retries then dead-letter.
        enqueue(
            "q6",
            "flaky",
            &nothing(),
            EnqueueOpts {
                max_attempts: 2,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let fail = |_job: Job| async { Err::<(), Error>(Error::Db("boom".into())) };
        assert!(run_once("q6", "w", 0, fail).await.unwrap(), "attempt 1 ran");
        assert!(run_once("q6", "w", 0, fail).await.unwrap(), "attempt 2 ran");
        assert!(
            !run_once("q6", "w", 0, fail).await.unwrap(),
            "now dead — nothing to claim"
        );
        let dead = db::sql(
            "SELECT state FROM dorm_jobs WHERE queue = $1",
            &[dorm_core::value::Value::Text("q6".into())],
        )
        .await
        .unwrap();
        assert_eq!(
            dead[0].try_get::<String>(0).unwrap(),
            "dead",
            "exhausted job is dead"
        );

        // 7) leases: one holder at a time, stolen only after expiry; the holder renews.
        assert!(
            acquire_lease("part-1", "leader-a", 300).await.unwrap(),
            "a takes it"
        );
        assert!(
            !acquire_lease("part-1", "leader-b", 300).await.unwrap(),
            "b is refused while a holds"
        );
        assert!(
            acquire_lease("part-1", "leader-a", 300).await.unwrap(),
            "a renews its own"
        );
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert!(
            acquire_lease("part-1", "leader-b", 300).await.unwrap(),
            "b takes it after expiry"
        );
        assert!(
            !acquire_lease("part-1", "leader-a", 300).await.unwrap(),
            "a is now refused"
        );
        release_lease("part-1", "leader-b").await.unwrap();
        assert!(
            acquire_lease("part-1", "leader-a", 300).await.unwrap(),
            "released, a takes it again"
        );
    });
}
