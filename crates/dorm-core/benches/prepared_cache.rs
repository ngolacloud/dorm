//! ADR 0007 — the performance proof.
//!
//! Two questions, measured against a real PostgreSQL:
//!
//! 1. **Does the per-connection statement cache pay off?** The `prepared-vs-adhoc`
//!    group answers it on raw `tokio-postgres`: the same SELECT/INSERT run once
//!    through `client.query(sql, …)` — which Parses an unnamed statement every
//!    call, the shape DORM's `db::query` had before this ADR — and once through a
//!    statement prepared a single time and then Bound/Executed. The gap is the
//!    cost of the Parse round trip the cache now removes.
//!
//! 2. **What does the typed ORM add on top of the wire?** The `dorm-vs-raw` group
//!    runs the typed `QuerySet` — which now rides the cache through `db::query` —
//!    next to the equivalent hand-written `tokio-postgres` call.
//!
//! The benchmark no-ops unless `DELONIX_ORM_DSN` names a throwaway PostgreSQL, so
//! `cargo bench` on a machine with no database is a clean skip rather than a
//! failure. To run it:
//!
//! ```bash
//! createdb dorm_bench
//! DELONIX_ORM_DSN=postgresql:///dorm_bench \
//!   cargo bench -p dorm-core --bench prepared_cache
//! dropdb dorm_bench
//! ```

// The model's fields are read by the derive-generated metadata and by row
// decoding, not named directly here.
#![allow(dead_code)]

use std::env;
use std::sync::OnceLock;

use criterion::Criterion;
use dorm_core::db;
use dorm_core::prelude::*;
use dorm_derive::Model;
use tokio::runtime::Runtime;
use tokio_postgres::{Client, NoTls, Statement};

/// A deliberately plain row: a serial key, an indexed text column to filter on,
/// and an integer. Enough to exercise a parameterized SELECT and an INSERT …
/// RETURNING without dragging in types whose encoding would dominate the timing.
#[derive(Model, Debug, Clone)]
#[model(table = "bench_widgets")]
struct Widget {
    #[field(primary_key)]
    id: i64,
    #[field(max_length = 120, index)]
    name: String,
    quantity: i64,
}

/// How many `name = 'seed'` rows the SELECT reads. Fixed so the measurement does
/// not drift as the INSERT benchmarks append their own rows.
const SEED_ROWS: i64 = 100;
const SELECT_LIMIT: i64 = 50;

/// The hand-written SELECT, column list in the model's field order so a raw row
/// decodes the same way the typed path's would.
const RAW_SELECT: &str =
    "SELECT id, name, quantity FROM bench_widgets WHERE name = $1 ORDER BY id LIMIT 50";
const RAW_INSERT: &str =
    "INSERT INTO bench_widgets (name, quantity) VALUES ($1, $2) RETURNING id, name, quantity";

/// One process-wide runtime — the rule `db.rs` documents: the pool is a global
/// initialised on first use, so every `.await` here must run on the same runtime.
fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("a tokio runtime")
    })
}

/// Connect a raw client, stand the table up, and seed it. Returns the client the
/// `tokio-postgres` arms run on.
async fn setup(dsn: &str) -> Client {
    let (client, connection) = tokio_postgres::connect(dsn, NoTls)
        .await
        .expect("connect a raw tokio-postgres client to DELONIX_ORM_DSN");
    // The connection drives the protocol; it must be polled for the client to
    // make progress, so it lives on its own task for the benchmark's duration.
    tokio::spawn(async move {
        let _ = connection.await;
    });

    client
        .batch_execute(
            "DROP TABLE IF EXISTS bench_widgets;\n\
             CREATE TABLE bench_widgets (\n\
                 id        BIGSERIAL PRIMARY KEY,\n\
                 name      VARCHAR(120) NOT NULL,\n\
                 quantity  BIGINT NOT NULL\n\
             );\n\
             CREATE INDEX bench_widgets_name_idx ON bench_widgets (name);",
        )
        .await
        .expect("create the bench table");

    for i in 0..SEED_ROWS {
        client
            .execute(
                "INSERT INTO bench_widgets (name, quantity) VALUES ($1, $2)",
                &[&"seed", &i],
            )
            .await
            .expect("seed a row");
    }

    client
}

fn bench_select(c: &mut Criterion, rt: &'static Runtime, client: &Client) {
    // Prepared once, reused every iteration — the cached side of the comparison.
    let cached: Statement = rt.block_on(async {
        client
            .prepare(RAW_SELECT)
            .await
            .expect("prepare the cached select")
    });

    let mut group = c.benchmark_group("select");

    // 1. Raw, ad-hoc: a Parse on every call. This is what `db::query` used to do.
    group.bench_function("raw-adhoc", |b| {
        b.to_async(rt).iter(|| async {
            let rows = client
                .query(RAW_SELECT, &[&"seed"])
                .await
                .expect("raw ad-hoc select");
            assert_eq!(rows.len(), SELECT_LIMIT as usize);
        });
    });

    // 2. Raw, cached: Bind/Execute only. This is the win the ORM's cache captures.
    group.bench_function("raw-cached", |b| {
        b.to_async(rt).iter(|| async {
            let rows = client
                .query(&cached, &[&"seed"])
                .await
                .expect("raw cached select");
            assert_eq!(rows.len(), SELECT_LIMIT as usize);
        });
    });

    // 3. Typed DORM: the `QuerySet`, now riding the per-connection cache.
    group.bench_function("dorm-typed", |b| {
        b.to_async(rt).iter(|| async {
            let rows = Widget::objects()
                .filter(widget::name.eq("seed"))
                .order_by([widget::id.asc()])
                .limit(SELECT_LIMIT)
                .all()
                .await
                .expect("typed select");
            assert_eq!(rows.len(), SELECT_LIMIT as usize);
        });
    });

    group.finish();
}

/// Prepare the table for the INSERT group. The three INSERT arms run in sequence
/// and each appends thousands of rows, so a growing structure would charge later
/// arms for rows earlier ones added — masking the Parse difference entirely. The
/// index on `name` is the worst of it: every arm inserts the same value, piling
/// duplicates onto one b-tree key. Dropping the index and truncating first makes
/// each INSERT a constant-time heap append, so the only thing left varying
/// between the arms is the one thing under test: whether the statement is cached.
async fn reset_for_insert(client: &Client) {
    client
        .batch_execute("DROP INDEX IF EXISTS bench_widgets_name_idx; TRUNCATE bench_widgets;")
        .await
        .expect("reset the table before the insert group");
}

fn bench_insert(c: &mut Criterion, rt: &'static Runtime, client: &Client) {
    rt.block_on(reset_for_insert(client));

    let cached: Statement = rt.block_on(async {
        client
            .prepare(RAW_INSERT)
            .await
            .expect("prepare the cached insert")
    });

    let mut group = c.benchmark_group("insert");

    group.bench_function("raw-adhoc", |b| {
        b.to_async(rt).iter(|| async {
            client
                .query(RAW_INSERT, &[&"ins", &1i64])
                .await
                .expect("raw ad-hoc insert");
        });
    });

    group.bench_function("raw-cached", |b| {
        b.to_async(rt).iter(|| async {
            client
                .query(&cached, &[&"ins", &1i64])
                .await
                .expect("raw cached insert");
        });
    });

    group.bench_function("dorm-typed", |b| {
        b.to_async(rt).iter(|| async {
            Widget::objects()
                .create(NewWidget {
                    name: "ins".into(),
                    quantity: 1,
                })
                .await
                .expect("typed insert");
        });
    });

    group.finish();
}

fn main() {
    let Ok(dsn) = env::var("DELONIX_ORM_DSN").or_else(|_| env::var("PAAS_DATABASE_URL")) else {
        eprintln!(
            "skipped: set DELONIX_ORM_DSN to a throwaway PostgreSQL to run the ADR 0007 benchmark"
        );
        return;
    };
    // The pool reads this on first use; set it before any `db::*` call.
    env::set_var(db::DSN_ENV, &dsn);

    let rt = runtime();
    let client = rt.block_on(setup(&dsn));

    let mut c = Criterion::default().configure_from_args();
    bench_select(&mut c, rt, &client);
    bench_insert(&mut c, rt, &client);
    c.final_summary();
}
