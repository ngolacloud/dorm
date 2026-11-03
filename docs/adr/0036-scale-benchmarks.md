# ADR 0036 — Benchmarks a escala (L16)

Status: Accepted — arnês + 2 cenários medidos a 1M, gate declarado (Fase 7) · 2026-11-03

## Context

As escolhas de desenho (keyset vs OFFSET, índice composto, âmbito de inquilino)
precisam de prova empírica a escala, com regressões a falharem o CI. Não havia
arnês a escala (só o bench de prepared statements do ADR 0007, 100 linhas).

## Decision

Um arnês de benchmark **reproduzível e parametrizável** (`DORM_BENCH_ROWS`,
default 50_000; `10_000_000` num host de bench dedicado) que mede as formas de SQL
que o ORM gera, em `crates/dorm-core/tests/scale_bench_live.rs`. Cada cenário:

1. **Paginação keyset vs OFFSET fundo** (L4, [[0024]]) — o keyset procura no
   índice, o OFFSET varre e descarta, por isso o seu custo cresce com a
   profundidade.
2. **Índice composto vs varrimento** (L8/[[0044]]) — a query "últimos N de um
   inquilino" (`WHERE tenant = … ORDER BY created_at DESC LIMIT 20`) medida **antes
   e depois** de `CREATE INDEX (tenant, created_at)`, com as linhas espalhadas por
   1000 inquilinos (a fatia de um é uma pequena fração da tabela — exactamente
   quando o varrimento dói).

**Gate de CI:** cada cenário imprime uma linha legível-por-máquina
`BENCH scenario=… <métricas em µs>` e tem uma **asserção de regressão** (o keyset
não pode ser mais lento que o OFFSET; o seek do índice não pode ser mais lento que
o varrimento). A asserção é o piso sempre-ligado; um job de CI corre os testes
`#[ignore]` a `DORM_BENCH_ROWS` alto num host dedicado, faz *parse* das linhas
`BENCH` e compara-as com uma baseline comprometida, com tolerância — o sinal de
regressão fino.

## Consequences

- **Prós:** prova empírica e reproduzível das escolhas de desenho; as asserções
  apanham uma regressão que fizesse o keyset varrer ou o índice deixar de ser
  usado; as linhas `BENCH` dão a um gate de CI números para diferenciar contra uma
  baseline.
- **Gate de CI concreto:** `scripts/bench_gate.sh` (`make bench`) corre o bench e
  impõe um **piso de speedup** (`BENCH_MIN_SPEEDUP`, por omissão 5x) em vez de
  comparar microssegundos absolutos — host-independente, logo não-flaky entre
  runners; sai ≠0 numa regressão (provado com um piso forçado). A baseline de
  referência (1M e **10M**) está em `docs/review/bench-baseline.md`.
- **Por fazer (declarado, honesto):** cenários adicionais (update com versão sob
  contenção, consumo de outbox a escala) são aditivos ao mesmo arnês; o job de CI
  a 10M corre num host dedicado (o gate e os números existem, falta só agendá-lo).

## Proof

- **Live (`dorm-core/tests/scale_bench_live.rs`, PostgreSQL 16, `DORM_BENCH_ROWS=1_000_000`):**
  - **keyset vs OFFSET:** OFFSET fundo **102,8 ms** vs keyset **0,34 ms** —
    **~305× mais rápido**; a asserção falha se o keyset deixar de ganhar.
  - **índice composto:** "últimos 20 do inquilino" por varrimento **36,5 ms** vs
    seek do índice composto **0,50 ms** — **~72× mais rápido**; a asserção falha
    se o índice deixar de ser usado.
  - Linhas de gate emitidas:
    `BENCH scenario=keyset_vs_offset rows=1000000 offset_us=102805 keyset_us=337`,
    `BENCH scenario=composite_index_tenant_latest rows=1000000 tenants=1000 scan_us=36470 seek_us=504`.
  - A 50_000 (default) os mesmos cenários passam de forma não-flaky; escala com
    `DORM_BENCH_ROWS` até aos 10M num host dedicado.
