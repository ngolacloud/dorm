# ADR 0049 — Deteção de pool-mode do PgBouncer (L15)

Status: Accepted — deteção + autoconfiguração + auto-cura implementadas (Fase 7) · 2026-11-03

## Context

Um pooler em **transaction mode** (PgBouncer `pool_mode = transaction`) multiplexa
cada ligação de cliente sobre várias ligações de servidor, uma por transação. Um
*prepared statement* nomeado (`sN`) vive numa ligação de servidor; a transação
seguinte pode cair noutra, e o `prepare_cached` do deadpool (ADR 0007) — que
guarda o `Statement` por ligação de cliente e o reutiliza sem voltar a fazer
`Parse` — bate então em dois erros, as duas faces do mesmo problema:

- **`26000`** `prepared statement "sN" does not exist` — o `sN` foi preparado num
  backend e o reuso caiu noutro (**em falta**).
- **`42P05`** `prepared statement "sN" already exists` — duas ligações de cliente
  multiplexadas no mesmo backend tentaram preparar o mesmo nome (**colisão**).

Medição vs. ponto de partida: o DORM já tinha a flag `DELONIX_ORM_NO_PREPARE`
(ADR 0007) e classificava o `26000` com um erro accionável (ADR 0034,
`error.rs`), mas **não detetava** o pooler nem tratava o `42P05`, e o operador
tinha de descobrir e definir a flag à mão. Medido ao vivo com PgBouncer 1.22 em
frente ao PostgreSQL 16 (confirmado em `pg_prepared_statements` / `SHOW CONFIG`).

## Decision

Três peças, da mais autoritativa para a mais defensiva:

1. **Fix autoritativo (configuração do pooler), documentado:** o PgBouncer ≥1.21
   com `max_prepared_statements >= N` rastreia e remapeia os nomes dos prepared
   statements por cliente — com ele, o caminho preparado-e-cacheado do DORM
   funciona através do transaction pooling **sem alterações**, mesmo sob
   concorrência. É a recomendação de produção.
2. **Deteção proactiva** (`db::detect_pgbouncer_pool_mode(admin_dsn)` +
   `db::autoconfigure_from_pgbouncer(admin_dsn)`): liga-se à base virtual
   `pgbouncer` (a consola de admin) e lê o `pool_mode` via **protocolo simples**
   (`SHOW CONFIG` — a consola não implementa o protocolo estendido); um PostgreSQL
   real recusa o comando, lido como "sem pooler". Em `transaction`/`statement`, e
   se o operador não fixou `DELONIX_ORM_NO_PREPARE`, liga o modo não-preparado do
   processo.
3. **Auto-cura reactiva + classificação:** `classify_db` passa a reconhecer
   **ambos** `26000` e `42P05` (sobre "prepared statement") com um erro accionável
   que nomeia `max_prepared_statements` **e** `DELONIX_ORM_NO_PREPARE`; o caminho de
   execução (`db.rs`) apanha qualquer um dos dois na reutilização de um statement
   cacheado, liga o não-preparado do processo e repete o statement — uma vez, sem
   configuração.

O `no_prepare` passou de um `OnceLock<bool>` para "env explícito manda; senão a
flag auto-detetada decide", para que a deteção e a auto-cura o possam ligar em
runtime sem desfazer uma escolha explícita do operador.

## Consequences

- **Prós:** o DORM **deteta** que está atrás de um pooler em transaction mode e
  adapta-se — proactivamente (admin console) e reactivamente (26000/42P05) — em vez
  de falhar com um erro cru. Com o `max_prepared_statements` do pooler, nem precisa
  de se adaptar.
- **Honesto, o que NÃO é possível só do lado do cliente:** para queries
  **parametrizadas** (protocolo estendido, `$n`), nenhuma mitigação do cliente é à
  prova de bala sob rotação agressiva — o próprio `query(&str)` "não-preparado" do
  driver faz `Parse`+`Execute` em dois exchanges, que um pooler pode separar. Por
  isso a recomendação nº 1 (`max_prepared_statements`) é a resposta correcta; a
  deteção/auto-cura reduzem o dano quando ela falta, não o eliminam. Declarado.
- A deteção usa uma ligação `NoTls` directa à consola (endpoint local e de
  confiança); uma consola remota com TLS fica para um follow-up.

## Proof

- **Unit (offline, `error.rs`):**
  `a_dropped_prepared_statement_points_at_the_pooler_fix` (26000) e
  `a_colliding_prepared_statement_also_points_at_the_pooler_fix` (42P05) — ambos
  dão um `Error::Config` que nomeia `max_prepared_statements` e
  `DELONIX_ORM_NO_PREPARE`; um 26000/42P05 não-relacionado com prepared statements
  fica `Error::Db`.
- **Live (`pgbouncer_live.rs`, PgBouncer 1.22 → PostgreSQL 16):**
  - `detects_pool_mode_from_the_admin_console_and_none_for_direct_postgres` — a
    consola reporta `pool_mode = transaction`; o PostgreSQL directo reporta `None`.
  - `operates_through_transaction_pooling` — com `max_prepared_statements = 200`,
    50 queries preparadas concorrentes sobre 8 ligações do pool passam todas
    (0 × 26000/42P05); `autoconfigure_from_pgbouncer` devolve `Some("transaction")`
    e, a seguir, o trabalho continua a fluir no modo não-preparado.
  - Medido também o perigo cru (sem `max_prepared_statements`): o reuso de um
    statement cacheado dá `26000`/`42P05` — exactamente a assinatura que a nº 3
    passa a reconhecer.
