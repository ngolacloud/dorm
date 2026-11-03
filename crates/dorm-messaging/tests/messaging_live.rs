// dorm-messaging is deprecated (moved to seiva-messaging, ADR 0054); these
// live tests still exercise it until the removal PR deletes the crate.
#![allow(deprecated)]

//! Outbox-backed messaging against a real PostgreSQL (ADR 0031 / L7).
//!
//! ```text
//! DELONIX_ORM_TEST_DSN='host=127.0.0.1 port=5432 user=orm dbname=ormtest' \
//!   cargo test -p dorm-messaging --test messaging_live -- --ignored --nocapture
//! ```

use std::sync::{Arc, Mutex};

use dorm_core::db;
use dorm_messaging::{CloudEvent, OutboxBus, Publisher};
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
fn events_publish_transactionally_and_are_delivered_exactly_once_in_effect() {
    let Some(dsn) = dsn() else {
        eprintln!("skipped: DELONIX_ORM_TEST_DSN is not set");
        return;
    };
    std::env::set_var(dorm_core::db::DSN_ENV, &dsn);

    runtime().block_on(async {
        for stmt in dorm_outbox::migrations() {
            db::execute(&stmt, &[]).await.expect("outbox schema");
        }
        for t in [
            "dorm_outbox",
            "dorm_outbox_cursor",
            "dorm_inbox",
            "dorm_outbox_dead",
        ] {
            db::execute(&format!("TRUNCATE {t}"), &[])
                .await
                .expect("truncate");
        }

        let bus = OutboxBus;

        // Publish two events in one transaction — committed with the "business
        // write" (here, just the outbox rows).
        db::atomic(|_tx| async {
            bus.publish(
                "deploys",
                &CloudEvent::new(
                    "evt-a",
                    "/compute",
                    "app.deployed",
                    serde_json::json!({"app": "api"}),
                )
                .with_traceparent("00-trace-a-01"),
            )
            .await?;
            bus.publish(
                "deploys",
                &CloudEvent::new(
                    "evt-b",
                    "/compute",
                    "app.deployed",
                    serde_json::json!({"app": "web"}),
                ),
            )
            .await
        })
        .await
        .expect("publish in a transaction");

        // A subscriber records which event ids it handled (and the traceparent it
        // saw, to prove the envelope survived the round trip).
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let traces = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let drain = {
            let seen = seen.clone();
            let traces = traces.clone();
            move || {
                let seen = seen.clone();
                let traces = traces.clone();
                async move {
                    bus.drain(
                        "projector",
                        "deploys",
                        dorm_outbox::RelayConfig::default(),
                        move |ev: CloudEvent| {
                            let seen = seen.clone();
                            let traces = traces.clone();
                            async move {
                                seen.lock().unwrap().push(ev.id.clone());
                                traces.lock().unwrap().push(ev.traceparent.clone());
                                Ok(())
                            }
                        },
                    )
                    .await
                }
            }
        };

        let n = drain().await.expect("drain");
        assert_eq!(n, 2, "both events delivered");
        assert_eq!(*seen.lock().unwrap(), vec!["evt-a", "evt-b"], "in order");
        assert_eq!(
            traces.lock().unwrap()[0],
            Some("00-trace-a-01".to_string()),
            "the traceparent survived the envelope round trip"
        );

        // Re-draining does nothing — the cursor is at the end.
        assert_eq!(drain().await.expect("re-drain"), 0);

        // Exactly-once effect: publish *another* event re-using id `evt-a`. It is a
        // new outbox row (delivered at-least-once), but the inbox has already seen
        // `(projector, evt-a)`, so the handler is skipped — `evt-a` is not handled
        // twice.
        db::atomic(|_tx| async {
            bus.publish(
                "deploys",
                &CloudEvent::new(
                    "evt-a",
                    "/compute",
                    "app.deployed",
                    serde_json::json!({"app": "api-again"}),
                ),
            )
            .await
        })
        .await
        .expect("re-publish");

        let _ = drain().await.expect("drain after duplicate");
        let count_a = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|id| *id == "evt-a")
            .count();
        assert_eq!(
            count_a, 1,
            "evt-a was handled exactly once despite redelivery"
        );
    });
}
