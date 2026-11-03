# Scale-benchmark baseline (ADR 0036 / L16)

Reference numbers for the reproducible scale bench
(`crates/dorm-core/tests/scale_bench_live.rs`), measured on PostgreSQL 16. The CI
gate (`scripts/bench_gate.sh`, `make bench`) does **not** compare absolute
microseconds — those vary by host — it enforces a host-independent **speedup
floor** (`BENCH_MIN_SPEEDUP`, default 5x). These numbers are the evidence that the
design choices pay off, and the context for choosing the floor; the two scenarios'
in-test assertions are the always-on correctness signal.

The gaps **widen** with scale, because the losers are O(n) (a deep `OFFSET` scans
and discards; the tenant-latest query scans and sorts the whole table) while the
winners are O(log n) (a keyset seek on the primary key; an index seek on
`(tenant, created_at)`).

| scenario | rows | slow | fast | speedup |
|---|---|---|---|---|
| keyset vs deep OFFSET | 1,000,000 | OFFSET 102.8 ms | keyset 0.34 ms | ~305x |
| keyset vs deep OFFSET | 10,000,000 | OFFSET 938 ms | keyset 0.41 ms | ~2271x |
| composite-index tenant-latest | 1,000,000 | scan 36.5 ms | seek 0.50 ms | ~72x |
| composite-index tenant-latest | 10,000,000 | scan 219.5 ms | seek 0.45 ms | ~490x |

Reproduce (the 10M run wants a few GB of disk and ~40 s):

```text
DORM_BENCH_ROWS=10000000 \
DELONIX_ORM_TEST_DSN='host=/var/run/postgresql port=5432 user=walter dbname=ormtest' \
  cargo test -p dorm-core --test scale_bench_live -- --ignored --nocapture

# or the gate (defaults to 50k rows, floor 5x):
make bench DSN='host=/var/run/postgresql port=5432 user=walter dbname=ormtest'
```

The dedicated-host CI job runs `make bench` with `DORM_BENCH_ROWS=10000000` and a
stricter `BENCH_MIN_SPEEDUP`, so a change that made the keyset scan or the index
go unused trips the gate.
