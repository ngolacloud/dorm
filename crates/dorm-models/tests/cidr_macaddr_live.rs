//! `CIDR` + `MACADDR` columns against a real PostgreSQL (ADR 0048 / L5-rede).
//!
//! ```text
//! DELONIX_ORM_TEST_DSN='host=/var/run/postgresql port=5432 user=walter dbname=ormtest' \
//!   cargo test -p dorm-models --test cidr_macaddr_live -- --ignored --nocapture
//! ```
//!
//! `CIDR` → `cidr::IpCidr`, `MACADDR` → `eui48::MacAddress`, both native on
//! PostgreSQL (bound and read through `postgres-types`' `with-cidr-0_3` /
//! `with-eui48-1`). The network-containment lookups span `inet` and `cidr`.

use cidr::IpCidr;
use dorm_core::model::Model;
use dorm_core::prelude::*;
use dorm_derive::Model;
use eui48::MacAddress;
use tokio::runtime::Runtime;

/// A network block keyed by its `CIDR`, with the gateway's `MACADDR`.
#[derive(Model, Debug, Clone)]
#[model(table = "l5_netblocks", ordering = "id")]
struct NetBlock {
    #[field(primary_key)]
    id: i64,
    block: IpCidr,
    mac: MacAddress,
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

/// Pure shape, no database: the SQL types are `CIDR`/`MACADDR` and the generated
/// DDL uses them as the column types.
#[test]
fn the_sql_types_are_cidr_and_macaddr_and_the_ddl_uses_them() {
    assert_eq!(FieldType::Cidr.sql_type(), "CIDR");
    assert_eq!(FieldType::MacAddr.sql_type(), "MACADDR");
    let ddl = dorm_migrations::ddl::create_table(NetBlock::meta()).expect("ddl");
    assert!(ddl.contains("\"block\" CIDR"), "cidr column: {ddl}");
    assert!(ddl.contains("\"mac\" MACADDR"), "macaddr column: {ddl}");
}

#[test]
#[ignore = "needs a Postgres (DELONIX_ORM_TEST_DSN)"]
fn cidr_and_macaddr_round_trip_and_containment_spans_cidr() {
    let Some(dsn) = dsn() else {
        eprintln!("skipped: DELONIX_ORM_TEST_DSN is not set");
        return;
    };
    std::env::set_var(dorm_core::db::DSN_ENV, &dsn);

    runtime().block_on(async {
        dorm_core::db::execute("DROP TABLE IF EXISTS l5_netblocks", &[])
            .await
            .expect("clean");
        let ddl = dorm_migrations::ddl::create_table(NetBlock::meta()).expect("ddl");
        dorm_core::db::execute(&ddl, &[])
            .await
            .expect("create table");

        for (block, mac) in [
            ("10.0.0.0/24", "08:00:2b:01:02:03"),
            ("10.0.1.0/24", "08:00:2b:01:02:04"),
            ("192.168.0.0/16", "aa:bb:cc:dd:ee:ff"),
        ] {
            NetBlock::objects()
                .create(NewNetBlock {
                    block: block.parse::<IpCidr>().unwrap(),
                    mac: mac.parse::<MacAddress>().unwrap(),
                })
                .await
                .expect("insert");
        }

        // Round-trip: the CIDR and the MAC read back as the same native values.
        let one = NetBlock::objects()
            .filter(net_block::block.eq("10.0.0.0/24".parse::<IpCidr>().unwrap()))
            .get()
            .await
            .expect("exact cidr match");
        assert_eq!(one.block, "10.0.0.0/24".parse::<IpCidr>().unwrap());
        assert_eq!(one.mac, "08:00:2b:01:02:03".parse::<MacAddress>().unwrap());

        // A MAC round-trips through an equality filter too.
        let by_mac = NetBlock::objects()
            .filter(net_block::mac.eq("aa:bb:cc:dd:ee:ff".parse::<MacAddress>().unwrap()))
            .get()
            .await
            .expect("exact mac match");
        assert_eq!(by_mac.block, "192.168.0.0/16".parse::<IpCidr>().unwrap());

        // Containment spans cidr: which blocks contain 10.0.0.5? Only 10.0.0.0/24.
        // (`block >> '10.0.0.5'`, the value bound as text and cast — ADR 0040.)
        let containing = NetBlock::objects()
            .filter_str("block__net_contains", "10.0.0.5")
            .all()
            .await
            .expect("net_contains on a cidr column");
        assert_eq!(containing.len(), 1, "one block contains 10.0.0.5");
        assert_eq!(
            containing[0].block,
            "10.0.0.0/24".parse::<IpCidr>().unwrap()
        );

        eprintln!("ok: cidr/macaddr round-trip and cidr containment work natively");
    });
}
