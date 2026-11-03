# ADR 0036 — Benchmarks a escala (L16)

Status: Proposed · 2026-11-03 — **arnês implementado (Fase 7)**

## Context

As escolhas de desenho (keyset vs OFFSET, âmbito de inquilino, consumo do outbox)
precisam de prova empírica a escala, com regressões a falharem o CI. Não havia
arnês a escala (só o bench de prepared statements do ADR 0007, 100 linhas).

## Decision

Um arnês de benchmark **reproduzível e parametrizável** (`DORM_BENCH_ROWS`,
default 50_000; 10_000_000 num host de bench dedicado) que mede as formas de SQL
que o ORM gera. O primeiro cenário entregue: paginação **keyset vs OFFSET fundo** —
o keyset procura no índice, o OFFSET varre e descarta, por isso o seu custo cresce
com a profundidade. O número medido vai para esta ADR; a asserção é o sinal de
regressão que um job de CI compara com uma baseline.

## Consequences

- **Prós:** prova empírica e reproduzível; a asserção apanha uma regressão que
  fizesse o keyset varrer.
- **Por fazer:** os cenários a 10M/100-inquilinos (update com versão, query com
  tenant+RLS, consumo de outbox) e o **job de CI com baseline+limiar** correm num
  host de bench dedicado — a instância de teste única mede-os a escala modesta.

## Proof

- **Live (`dorm-core/tests/scale_bench_live.rs`):** a 50 000 linhas, OFFSET fundo
  ~8,7 ms vs keyset ~0,36 ms (**~24× mais rápido**); a asserção falha se o keyset
  deixar de ganhar. Escala com `DORM_BENCH_ROWS`.
