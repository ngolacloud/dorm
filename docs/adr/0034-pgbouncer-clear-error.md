# ADR 0034 — Compatibilidade PgBouncer: erro claro (L15)

Status: Accepted — implementado (Fase 7) · 2026-11-03

## Context

O cache de prepared statements (ADR 0007) é incompatível com um pooler em modo
transacção sem prepared statements ao nível do protocolo: o servidor prepara `sN`
num backend e a statement seguinte cai noutro, dando `prepared statement "sN" does
not exist` (SQLSTATE `26000`). Já existe o escape `DELONIX_ORM_NO_PREPARE=1`
(provado em `no_prepare_pgbouncer_live.rs`), mas no modo incompatível o operador
via só o `26000` cru, sem saber o que fazer (achado da medição L15).

## Decision

`classify_db` passa a reconhecer o `26000` **quando a mensagem menciona
`prepared statement`** e devolve um `Error::Config` acionável que nomeia a causa
(pooler em modo transacção) e o fix (`DELONIX_ORM_NO_PREPARE=1`). Um `26000` por
outra razão continua um `Error::Db` genérico.

## Consequences

- **Prós:** o modo incompatível falha com uma mensagem que diz o que fazer, em vez
  do erro cru do driver.
- **Contras / custo:** é uma heurística sobre a mensagem (o `26000` é genérico);
  por isso só reclassifica quando o texto fala de `prepared statement`.
- **Follow-up:** auto-deteção do `pool_mode` e negociação de prepared statements
  ao nível do protocolo (PgBouncer ≥1.21 `max_prepared_statements>0`) — precisa de
  um PgBouncer a correr para provar.

## Proof

- **Unit (`error::tests`):** um `26000` de prepared statement vira um `Config` que
  contém `DELONIX_ORM_NO_PREPARE`; um `26000` de outra coisa fica `Db`.
- **Live (já existente):** `no_prepare_pgbouncer_live.rs` prova o caminho
  `NO_PREPARE` contra PgBouncer em modo transacção.
