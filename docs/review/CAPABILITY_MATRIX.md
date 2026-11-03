# DORM — Matriz de capacidades de grau DBA

> O que o DORM suporta hoje, por **abstracção** (API tipada/string), por **SQL
> cru** (`Manager::raw`/`db::sql`, ADR 0005) e o que o **Gateway**-alvo exporá;
> por backend (**PostgreSQL** primeiro, **SQLite** depois); com **teste** que o
> cobre e **custo medido** quando há. Data: 2026-10-31.
>
> Legenda: ✓ suportado · ~ parcial/frágil · ✗ ausente · n/a não aplicável.
> "Gateway" refere-se à arquitectura-alvo proposta (`TARGET_ARCHITECTURE.md`),
> ainda não construída. "Custo medido" só é preenchido onde houve prova viva
> (Fase 2); o resto é **não medido** e fica para a Fase 3/4 (protocolo em
> `findings/performance.md`).

---

## 1. Transacções e concorrência

| Capacidade | Abstr. | SQL cru | Gateway | PG | SQLite | Teste | Custo medido | Evidência / achado |
|---|:--:|:--:|:--:|:--:|:--:|---|---|---|
| Transacção `atomic` + `SAVEPOINT` aninhado | ✓ | ✓ | ✓ | ✓ | ✓ | `interactive_tx_sqlite`, `migrate_live` | — | `db.rs:1258,1281` |
| Nível de isolamento por tx (`READ COMMITTED`…`SERIALIZABLE`) | ✗ | ~ (1º `SET TRANSACTION …` no closure) | ✓ (ADR 0015) | ~ | ✗ | — | — | DBA-02 · `db.rs:1326` |
| `READ ONLY`/`DEFERRABLE` por tx | ~ (`BEGIN READ ONLY`) | ~ | ✓ | ~ | ✗ | — | — | DBA-02 |
| Retry `40001`/`40P01` com backoff | ✗ | ~ (loop + string-match) | ✓ (ADR 0015) | ✗ | n/a | — | — | DBA-02 · `error.rs:135-143` |
| `SELECT … FOR UPDATE/SHARE` | ✗ | ✓ (SELECT cru) | ✓ | só cru | ✗ | — | — | DBA-02 · `compiler.rs:349` |
| `NOWAIT` / `SKIP LOCKED` (filas) | ✗ | ✓ | ✓ | só cru | ✗ | — | — | DBA-02 |
| Advisory locks expostos | ✗ (só interno migrador) | ✓ `pg_advisory_xact_lock` | ✓ | só cru | n/a | — | — | `migrator.rs:51` |
| `SET LOCAL lock_timeout`/`idle_in_transaction` | ✗ | ✓ dentro de `atomic` | ✓ | só cru | ✗ | — | — | DBA-02 |
| `SET LOCAL app.tenant_id` (RLS) | ✗ | ✓ | ✓ (ADR 0014) | só cru | n/a | — | — | SEC-04 |
| `statement_timeout` | ~ (só por processo, DSN) | ✓ (`SET LOCAL`) | ✓ (deadline→timeout) | ~ | ✗ | `statement_timeout_live` | — | `db.rs:282` |
| `application_name` por tx | ✗ (só DSN) | — | ✓ | só DSN | n/a | — | — | DBA-02 |
| Optimistic locking (coluna `version`) | ✗ | ~ (emulável) | ✓ (ADR 0015) | ✗ | ✗ | — | — | DBA-02 |
| **`get_or_create` seguro em tx** | **✗ (bug 25P02)** | — | ✓ (ON CONFLICT) | **✗** | ~ | repro Fase 2 | **✓ vivo: `23505`→`25P02`** | **DBA-01/H3** |

## 2. Objectos de base de dados como código

| Capacidade | Abstr. (migração) | SQL cru | Gateway | PG | SQLite | Teste | Custo medido | Evidência / achado |
|---|:--:|:--:|:--:|:--:|:--:|---|---|---|
| Tabelas, colunas (add/drop), `DROP TABLE` | ✓ | ✓ | ✓ | ✓ | ✓ | `migrate_live`, `schema_parity` | — | `ddl.rs:60-196` |
| Índice B-tree de 1 coluna | ✓ | ✓ | ✓ | ✓ | ✓ | `ddl.rs` tests | — | `ddl.rs:208` |
| Índice composto/parcial/expressão/`INCLUDE` | ✗ | ✓ (DDL à mão) | ✓ (ADR 0016) | só cru | só cru | — | — | MIG-03 · `parser.rs:229` |
| `GIN`/`GiST`/`BRIN`/`hash` | ✗ | ✓ | ✓ | só cru | ✗ | — | — | MIG-03 |
| `CREATE INDEX CONCURRENTLY` | **✗ (impossível)** | ✗ (`25001` na tx) | ✓ (fora-de-tx, ADR 0016) | **✗** | n/a | repro Fase 2 | **✓ vivo: `25001`** | **MIG-01/H7** |
| Views / Materialized Views / `REFRESH CONCURRENTLY` | ✗ | ✓ | ✓ | só cru | ~ | — | — | MIG-03 |
| Funções / procedures (`CALL`) | ✗ | ✓ | ✓ (RPC tipado, ADR 0013) | só cru | n/a | — | — | MIG-03 · GW-05 |
| Triggers / funções de trigger | ✗ | ✓ | ✓ | só cru | ~ | — | — | MIG-03 |
| RLS (`ENABLE RLS`/`POLICY`) | ✗ | ✓ | ✓ (ADR 0014) | só cru | n/a | — | — | MIG-03 · SEC-04 |
| Partições declarativas | ✗ | ✓ | ✓ | só cru | n/a | — | — | MIG-03 |
| Sequências autónomas | ✗ (só `BIGSERIAL`) | ✓ | ✓ | só cru | ~ | — | — | MIG-03 · `ddl.rs:112` |
| Schemas / `search_path` | ✗ (assume `public`) | ✓ | ✓ | só cru | n/a | — | — | MIG-03 · `migrator.rs:212` |
| `COMMENT ON` / `CREATE EXTENSION` / grants | ✗ | ✓ | ✓ | só cru | ~ | — | — | MIG-03 |
| FK `ON DELETE/UPDATE` | ✓ | ✓ | ✓ | ✓ | ~ | `ddl.rs` tests | — | `ddl.rs:141` |
| FK/constraints `DEFERRABLE` | ✗ | ✓ | ✓ | só cru | ✗ | — | — | DBA-02 · `ddl.rs:141` |
| Alteração in-place de coluna (type/null/default) | ✓ (PG) | ✓ | ✓ | ✓ | ~ (rebuild) | `migrate_live` | — | `autodetect.rs:377` |
| Alteração de PK/FK | ✗ (recusa) | ✓ | ✓ | só cru | ✗ | — | — | `autodetect.rs:384` |
| **Detecção de renomeação (sem perda de dados)** | **✗ (drop+add)** | ✓ (DDL à mão) | ✓ (ADR 0016) | **✗** | **✗** | — | — | **MIG-02** |

## 3. Consulta

| Capacidade | Abstr. | SQL cru | Gateway | PG | SQLite | Teste | Custo medido | Evidência / achado |
|---|:--:|:--:|:--:|:--:|:--:|---|---|---|
| `filter`/`exclude`/`order`/`limit`/`offset` | ✓ | ✓ | ✓ | ✓ | ✓ | `sql_snapshots`, `typed_e2e` | — | `compiler.rs` |
| Agregados (`count`/`sum`/`avg`/`min`/`max`) | ✓ | ✓ | ✓ | ✓ | ✓ | `sql_snapshots` | — | `expr.rs` |
| `select_related` (FK directa) | ~ (só por string) | ✓ | ✓ | ~ | ~ | `typed_e2e` | — | DESIGN/PERF-04 |
| `prefetch_related` (reverso/M2M) | ✗ (N+1) | ✓ | ✓ | ✗ | ✗ | — | — | PERF-04 · `m2m.rs` |
| Upsert `ON CONFLICT DO UPDATE/NOTHING` | ✗ | ✓ | ✓ (ADR p/ H8) | só cru | só cru | — | — | DBA-01 · `compiler.rs` |
| `RETURNING` selectivo | ~ (lista fixa de metadata) | ✓ | ✓ | ~ | ~ | `sql_snapshots` | — | MIG-07 · `compiler.rs:1065` |
| Subqueries / `Exists` / `OuterRef` | ✗ | ✓ | ✓ | só cru | só cru | — | — | H8 |
| CTE (incl. recursiva/`MATERIALIZED`) | ✗ | ✓ | ✓ | só cru | só cru | — | — | H8 |
| Funções de janela | ✗ | ✓ | ✓ | só cru | só cru | — | — | H8 |
| `LATERAL` / `DISTINCT ON` / `UNION` | ✗ | ✓ | ✓ | só cru | ~ | — | — | H8 |
| `= ANY($1)` (vs `IN` expandido) | ✗ (expande 1 ph/elem) | ✓ | ✓ | só cru | só cru | `sql_snapshots` | — | PERF-03 · `compiler.rs:332` |
| Agregados `FILTER (WHERE …)` | ✗ | ✓ | ✓ | só cru | ✗ | — | — | H8 |
| JSONB (`->`,`->>`,`@>`,`?`) | ✗ (tipo Json opaco) | ✓ | ✓ | só cru | ~ | — | — | H8 · DBA-04 |
| Full-text (`tsvector`/`tsquery`) | ✗ | ✓ | ✓ | só cru | ✗ | — | — | DBA-04 |
| Paginação *keyset* | ✗ (só offset) | ✓ | ✓ | só cru | só cru | — | — | PERF-07 |
| *Streaming* por cursor/portal | ✗ (materializa tudo) | ~ | ✓ (gRPC stream, ADR 0012) | ✗ | ✗ | — | — | PERF-07 · GW-03 |
| `COPY FROM/TO STDIN` | ✗ | ✗ | ✓ | ✗ | n/a | — | — | H8 |
| `LISTEN/NOTIFY` | ✗ | ~ | ✓ | só cru | n/a | — | — | H8 |
| `EXPLAIN (ANALYZE, BUFFERS, JSON)` | ✗ | ✓ | ✓ (CLI/API) | só cru | ~ | — | — | §3.2 |
| **`bulk_create` > 65 535 parâmetros** | **✗ (sem chunking)** | — | ✓ (chunk) | **✗** | ~ | repro Fase 2 | **✓ vivo: 65 536 falha (`u16`)** | **PERF-02/H9** |

## 4. Operação

| Capacidade | Abstr. | SQL cru | Gateway | PG | SQLite | Teste | Custo medido | Evidência / achado |
|---|:--:|:--:|:--:|:--:|:--:|---|---|---|
| Múltiplas bases / routing (primário/réplica, tenant) | ✗ (pool global) | ✗ | ✓ (ADR 0014) | ✗ | ✗ | — | — | DESIGN-01/H1 |
| Pool por teste / por sessão | ✗ (`static OnceLock`) | ✗ | ✓ | ✗ | ✗ | — | — | DESIGN-01 |
| `READ_ONLY` por sessão/pedido | ✗ (`static AtomicBool`) | ✗ | ✓ | ✗ | ✗ | — | — | SEC-03/H15 |
| **Compatibilidade PgBouncer (transaction mode)** | **✗** | **✗** | ✓ (sem-prepare/ADR 0007+) | **✗** | n/a | repro Fase 2 | **✓ vivo: `26000`** | **DBA-03/H2** |
| Health-check / reconexão | ~ (deadpool) | — | ✓ | ~ | n/a | — | — | `db.rs:422` |
| Cancel de statement ao cair o pedido | ✗ | ✗ | ✓ (deadline, ADR 0012) | ✗ | ✗ | — | — | GW-03 |
| `pg_stat_statements`-friendly (`= ANY`, `application_name`, sqlcommenter) | ~ (`IN` polui) | ~ | ✓ | ~ | n/a | — | — | PERF-03 · DBA-02 |
| Métricas do pool (OpenTelemetry) | ~ (`pool_stats` básico) | — | ✓ | ~ | n/a | `db.rs` tests | — | `db.rs:213` |
| Observabilidade (`tracing` spans) | ✓ | ✓ | ✓ | ✓ | ✓ | `observability_live` | — | ADR 0008 |
| Drift detection (schema ↔ catálogo) | ~ (só aditivo) | — | ✓ | ~ | ~ | `sync_live` | — | MIG-05 · `migrator.rs:206` |
| `db pull` completo | ~ (unidireccional, com perdas) | — | ✓ | ~ | ~ | — | — | MIG-05 · `main.rs:1407` |
| DSN sem password nos logs | ✓ (nunca regista `params`) | ✓ | ✓ | ✓ | ✓ | — | — | SEC-07 · `db.rs:116` |
| TLS (`rustls`/`openssl`) | ✓ (feature-gated) | ✓ | ✓ | ✓ | n/a | — | — | ADR 0004 |
| Verificação de certificado TLS | ✗ (`sslmode=require` sem verify) | — | ✓ | ~ | n/a | — | — | SEC-07 |

---

## 5. Leitura da matriz

- **"Só cru" é o padrão dominante** nas capacidades avançadas: o DORM não as
  esconde (o escape SQL cru parametrizado existe e é seguro), mas **não as compõe**
  com o `QuerySet` nem as descodifica (os tipos de `read_column` são os 8 de H5).
  Promover capacidades de "só cru" para "abstracção" é o grosso do roteiro de
  endurecimento.
- **Os quatro negativos a negrito** (`get_or_create` em tx, `CONCURRENTLY`,
  renomeação, PgBouncer) são os que já estão **provados ao vivo** como defeitos,
  não meras lacunas — e são a prioridade.
- **A coluna "Gateway" é uma promessa, não um estado**: tudo o que lá está ✓
  depende dos ADRs 0010–0014 e preserva os invariantes por construção
  (`TARGET_ARCHITECTURE.md`).
- **"Custo medido" está quase vazio por desenho**: a Fase 2 mediu os *defeitos*
  (erros reproduzidos), não ainda o *desempenho* dos três caminhos de §3.3 — isso
  é a Fase 3 (benchmarks `criterion` vs `tokio-postgres` cru / `sqlx` /
  `diesel-async` / `sea-orm`), com os critérios de aceitação em
  `findings/performance.md`.
