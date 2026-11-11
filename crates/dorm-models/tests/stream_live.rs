//! `QuerySet::stream()` reads a large result set with ~constant memory (C-F1).
//!
//! The materialising terminals (`all`/`rows`) collect the whole result set into a
//! `Vec` — a table-sized read is a table-sized allocation. `stream()` drives
//! `query_raw`/`RowStream` over a bounded channel instead, so a big table is read
//! a window at a time. This proves the ORM path end to end: insert many rows,
//! stream them one by one (never collecting), and confirm every row arrives
//! decoded and in order.
//!
//! ```text
//! DELONIX_ORM_TEST_DSN='host=/var/run/postgresql user=walter dbname=dorm_af1' \
//!   cargo test -p dorm-models --test stream_live -- --ignored --nocapture
//! ```

use dorm_core::model::Model;
use dorm_derive::Model;
use tokio::runtime::Runtime;
use tokio_stream::StreamExt;

/// A minimal row: a primary key and a counter, enough to prove large-scan streaming.
#[derive(Model, Debug, Clone)]
#[model(table = "l5_stream_rows", ordering = "id")]
struct Widget {
    #[field(primary_key)]
    id: i64,
    n: i64,
}

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
fn stream_reads_every_row_of_a_large_table_one_window_at_a_time() {
    let Some(dsn) = dsn() else {
        eprintln!("skipped: DELONIX_ORM_TEST_DSN is not set");
        return;
    };
    std::env::set_var(dorm_core::db::DSN_ENV, &dsn);

    // 20k rows is far more than the 128-row stream buffer, so the whole run never
    // holds more than the window — yet every row must be delivered.
    const ROWS: i64 = 20_000;

    runtime().block_on(async {
        dorm_core::db::execute("DROP TABLE IF EXISTS l5_stream_rows", &[])
            .await
            .expect("clean");
        dorm_core::db::execute(
            "CREATE TABLE l5_stream_rows (id BIGINT PRIMARY KEY, n BIGINT NOT NULL)",
            &[],
        )
        .await
        .expect("table");
        // Bulk-insert via generate_series — one statement, no per-row round trip.
        dorm_core::db::execute(
            "INSERT INTO l5_stream_rows (id, n) SELECT g, g * 2 FROM generate_series(1, $1::bigint) AS g",
            &[&ROWS],
        )
        .await
        .expect("seed rows");

        // Stream the whole table, consuming one row at a time — NEVER collecting
        // into a Vec. Assert every row arrives, decoded, in `ordering` (id) order.
        let mut stream = Widget::objects()
            .all()
            .stream()
            .await
            .expect("stream opens");
        let mut count: i64 = 0;
        let mut sum_n: i128 = 0;
        let mut last_id = 0i64;
        while let Some(row) = stream.next().await {
            let w = row.expect("each streamed row decodes");
            count += 1;
            sum_n += i128::from(w.n);
            assert!(w.id > last_id, "rows arrive in id order: {} after {}", w.id, last_id);
            last_id = w.id;
        }

        assert_eq!(count, ROWS, "every row of the large table was streamed");
        // n = id*2, so the sum is a cheap end-to-end checksum that each row decoded.
        let expected_sum: i128 = (1..=i128::from(ROWS)).map(|g| g * 2).sum();
        assert_eq!(sum_n, expected_sum, "every streamed row decoded correctly");

        dorm_core::db::execute("DROP TABLE IF EXISTS l5_stream_rows", &[])
            .await
            .ok();
    });
}

/// `stream()` refuses the shapes its first cut does not decode: a `values()`
/// projection and `select_related()` joins. Pure/offline (no database).
#[test]
fn stream_rejects_values_projection_offline() {
    // `.values(...)` sets the projection; `stream()` must refuse it up front rather
    // than mis-decode. This runs without a DB: the guard fires before any I/O.
    let err = runtime().block_on(async { Widget::objects().values(&["id"]).stream().await.err() });
    let msg = format!("{}", err.expect("stream() rejects a values() projection"));
    assert!(msg.contains("values()"), "actionable message: {msg}");
}
