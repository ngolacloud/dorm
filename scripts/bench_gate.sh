#!/usr/bin/env bash
#
# The scale-benchmark CI gate (ADR 0036 / L16).
#
# Runs the reproducible scale bench against a PostgreSQL and enforces the
# regression signal as a **speedup floor** — host-independent, so it is not
# flaky across CI runners the way absolute microseconds would be: the keyset
# page must stay at least BENCH_MIN_SPEEDUP times faster than a deep OFFSET, and
# the composite-index seek at least that much faster than the scan. At 1M rows
# both ratios are ~70–300x and at 10M ~490–2270x (see docs/review/bench-baseline.md),
# so the default floor of 5 catches a real regression while tolerating a small,
# cheap CI row count.
#
# Usage:
#   DSN='host=/var/run/postgresql port=5432 user=walter dbname=ormtest' \
#   DORM_BENCH_ROWS=200000 BENCH_MIN_SPEEDUP=5 scripts/bench_gate.sh
set -euo pipefail

DSN="${DSN:-host=/var/run/postgresql port=5432 user=walter dbname=ormtest}"
ROWS="${DORM_BENCH_ROWS:-50000}"
MIN="${BENCH_MIN_SPEEDUP:-5}"

echo "bench gate: ${ROWS} rows, floor ${MIN}x — $DSN"
out="$(DORM_BENCH_ROWS="$ROWS" DELONIX_ORM_TEST_DSN="$DSN" \
  cargo test -p dorm-core --test scale_bench_live -- --ignored --nocapture 2>&1)" || {
    echo "$out" | tail -20
    echo "bench gate: the benchmark itself failed" >&2
    exit 2
}

echo "$out" | grep '^BENCH ' || { echo "bench gate: no BENCH lines emitted" >&2; exit 2; }

# Pull `key=value` out of the BENCH lines and check each scenario's speedup.
check() {
    # $1 scenario, $2 slow field, $3 fast field
    local line slow fast ratio
    line="$(echo "$out" | grep "scenario=$1" || true)"
    [ -n "$line" ] || { echo "bench gate: scenario $1 missing" >&2; return 1; }
    slow="$(echo "$line" | grep -oE "$2=[0-9]+" | cut -d= -f2)"
    fast="$(echo "$line" | grep -oE "$3=[0-9]+" | cut -d= -f2)"
    [ -n "$slow" ] && [ -n "$fast" ] && [ "$fast" -gt 0 ] || {
        echo "bench gate: $1 unparseable ($2=$slow $3=$fast)" >&2; return 1; }
    ratio=$(( slow / fast ))
    if [ "$ratio" -lt "$MIN" ]; then
        echo "bench gate: FAIL $1 — ${3%_us} only ${ratio}x faster than ${2%_us} (floor ${MIN}x)" >&2
        return 1
    fi
    echo "bench gate: ok $1 — ${ratio}x (floor ${MIN}x)"
    return 0
}

rc=0
check keyset_vs_offset offset_us keyset_us || rc=1
check composite_index_tenant_latest scan_us seek_us || rc=1
[ "$rc" -eq 0 ] && echo "bench gate: PASS" || echo "bench gate: regression detected" >&2
exit "$rc"
