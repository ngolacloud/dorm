# DORM — Roteiro de tijolos (bricks)

> Ordenados por **valor/risco**: corrigir primeiro, endurecer a seguir,
> externalizar por fim. Cada tijolo tem um **critério de aceitação testável**
> (um teste que falha antes e passa depois, ou uma medição). Um PR por tijolo,
> `make check` verde, *snapshots* SQL só mudam com intenção explícita.
> Esforço: S (horas) · M (dias) · L (semana+). Data: 2026-10-31.
>
> Regra de honestidade (roadmap): um backend sem a capacidade devolve **erro
> explícito**, nunca resultado errado.


> **Alignment note (2026-11-03) — [ADR 0054, the ORM boundary](../adr/0054-orm-boundary.md).**
> These waves predate the boundary. The 2026-10-31 proposals they cite were
> renumbered to resolve collisions (0010–0017 → 0055–0062). Waves whose work
> landed now point at the accepted ADR; **Wave 8 (polyglot externalization) is
> rejected by ADR 0054** — broker/HTTP/data-plane capabilities move to the
> framework layer, not DORM.

---

## Vaga 1 — Correcção (defeitos provados ao vivo; sem desestabilizar o núcleo)

### B-01 · `get_or_create`/`update_or_create` seguros em transacção — Crítico · M
- **Achado:** DBA-01/H3. **ADR:** encosta a upsert (H8).
- **Mudança:** compilar `INSERT … ON CONFLICT (<lookup>) DO NOTHING RETURNING …`
  (e `DO UPDATE` para `update_or_create`); eliminar o `SELECT` de recuperação. Em
  alternativa mínima, envolver o `INSERT` num `SAVEPOINT` (reutilizar `atomic`
  aninhado, `db.rs:1281`).
- **Aceitação:** teste live `get_or_create_inside_atomic_survives_a_concurrent_insert`
  — dois `get_or_create` concorrentes sobre a mesma chave, ambos dentro de
  `atomic()`; antes: um devolve `Err(…25P02…)` e a tx faz rollback; depois: ambos
  devolvem `(row, false|true)` sem `25P02`. Novo *snapshot* do `INSERT … ON CONFLICT`.

### B-02 · `bulk_create` com *chunking* — Alto · S
- **Achado:** PERF-02/H9.
- **Mudança:** partir `rows` em lotes de `floor(65535 / n_cols)` (margem de
  segurança), um `INSERT` por lote, dentro de uma transacção; concatenar os
  `RETURNING`.
- **Aceitação:** teste live `bulk_create_of_70000_rows_succeeds` — antes: erro de
  overflow `u16` no Bind; depois: 70 000 linhas inseridas, ordem preservada.

### B-03 · `= ANY($1)` em vez de `IN` expandido — Alto · M
- **Achado:** PERF-03/DBA-03/H8 (raiz da explosão de cache H2).
- **Mudança:** compilar `col = ANY($1)` com um único placeholder de array (o
  `FieldType` já está em `compiler.rs:251`); manter `IN` só onde o backend não
  suporte arrays (SQLite → mantém expansão, com erro honesto se preciso).
- **Aceitação:** *snapshot* SQL mostra `= ANY($1)` em PG; teste
  `in_lists_of_varying_size_produce_one_cached_statement` — correr `x__in` com
  N=1..1000 numa conexão e verificar `pg_prepared_statements` **constante** (hoje
  cresce com N).

### B-04 · Modo de execução sem *prepare* (compatibilidade PgBouncer) — Alto · M
- **Achado:** DBA-03/H2 (provado: `26000` pelo PgBouncer txn mode).
- **Mudança:** flag/env que troca `prepare_cached` por `conn.query(sql, …)`
  directo; documentar o requisito `max_prepared_statements` do PgBouncer;
  opcionalmente tecto LRU na cache.
- **Aceitação:** `test-live` com `DELONIX_ORM_DSN` a apontar ao PgBouncer
  (`127.0.0.1:6432`, transaction mode) — antes: `26000`; depois (flag ligada):
  consultas sucessivas passam.

### B-05 · Create da API gerada respeita `guard_write` — Alto · S
- **Achado:** SEC-01.
- **Mudança:** o handler de escrita gerado (`codegen_api.rs:173`) chama o caminho
  guardado (`QuerySet::create`/`guard_write`) em vez de `run_query` cru.
- **Aceitação:** teste `generated_api_post_is_rejected_when_read_only` — com
  `set_read_only(true)`, um `POST` devolve erro de só-leitura; hoje insere.

### B-06 · Renomear o binário do REPL (fim da colisão `dorm`) — Baixo · S
- **Achado:** H14/ADOPT (colisão só local do contribuidor).
- **Mudança:** `dorm-shell` passa a `[[bin]] name = "dorm-shell"` (ou `dormsh`).
- **Aceitação:** `cargo build --workspace` sem aviso de *output filename
  collision*; `cargo run --bin dorm` não-ambíguo.

---

## Vaga 2 — Concorrência (grau DBA) · ADR 0060 (partially superseded by ADR 0023)

### B-07 · Nível de isolamento por transacção — Alto · M
- **Mudança:** `atomic_with(IsolationLevel, read_only)` → `BEGIN ISOLATION LEVEL …`.
- **Aceitação:** teste `serializable_transaction_sees_a_snapshot` (duas tx em
  `SERIALIZABLE` sobre a mesma linha → uma recebe `40001`).

### B-08 · Classificar e repetir `40001`/`40P01` — Alto · M
- **Mudança:** `is_serialization_failure`/`is_deadlock` em `error.rs:154`;
  `classify_db` a nomeá-los; helper `retry_on_serialization(n, backoff, f)`.
- **Aceitação:** teste `n_tasks_under_serializable_all_commit_with_retry` — N
  tarefas concorrentes convergem sem erro visível (hoje o `40001` sobe opaco).

### B-09 · `FOR UPDATE`/`FOR SHARE` + `NOWAIT`/`SKIP LOCKED` no `QuerySet` — Alto · M
- **Mudança:** `.lock_for_update()`/`.skip_locked()`/`.nowait()` → cláusula no
  `compile_select`.
- **Aceitação:** *snapshot* SQL; teste `skip_locked_processes_each_row_once` — fila
  de trabalho com N consumidores, nenhum item processado duas vezes.

### B-10 · `set_local` seguro (timeouts/GUCs/RLS) — Alto · S
- **Mudança:** `set_local(key, value)` com chave validada (allow-list) dentro de
  `atomic` → `SET LOCAL lock_timeout`/`idle_in_transaction`/`app.tenant_id`.
- **Aceitação:** teste `set_local_lock_timeout_aborts_a_blocked_write`; o valor é
  ligado como parâmetro, nunca interpolado (I2).

### B-11 · Optimistic locking por coluna `version` — Médio · M
- **Mudança:** incremento + `WHERE version = $old`; erro se 0 linhas afectadas.
- **Aceitação:** teste `concurrent_update_detects_lost_update`.

---

## Vaga 3 — Conexões e sessão (fim do estado global) · ADR 0059 (parte)

### B-12 · `Database` explícito substitui o pool global — Crítico (habilitador) · L
- **Achado:** DESIGN-01/H1/H13, SEC-03/H15.
- **Mudança:** introduzir um handle `Database`/`Session` passado explicitamente (ou
  via *task-local*), removendo `static POOL`/`static READ_ONLY`; `READ_ONLY` e DSN
  passam a ser por sessão. Manter uma fachada de conveniência para o caminho
  embebido simples, mas sem estado global partilhado entre tarefas.
- **Aceitação:** teste `two_sessions_have_independent_read_only_flags` (reproduz e
  fecha o bug do ramo `fix/typed-e2e-readonly-flag-race`); `make check` com testes
  e2e paralelos sem fuga de flag.

### B-13 · Routing primário/réplica e por tenant — Alto · L
- **Mudança:** o `Database` resolve a conexão por papel (escrita→primário,
  leitura→réplica) e/ou por tenant.
- **Aceitação:** teste `read_queries_route_to_replica` (com dois DSNs de teste).

---

## Vaga 4 — Sistema de tipos · ADR 0062 (superseded by ADR 0025)

### B-14 · Tipos escalares sem ambiguidade (`f32`/`f64`/`i16`/`date`/`time`/`bytea`) — Médio · M
- **Achado:** DBA-04/H5.
- **Mudança:** novas variantes em `FieldType`+`Value` com os cinco braços
  (`to_sql`/`read_column`/`coerce`/`sql_type`/derive).
- **Aceitação:** modelo com `f64`/`NaiveDate`/`Vec<u8>` compila, migra e faz
  round-trip; *snapshots* de DDL nos dois dialectos.

### B-15 · `NUMERIC(p,s)`, arrays, `INET`/`CIDR`, enums nativos — Médio · L
- **Aceitação:** por tipo, um teste de round-trip + *snapshot*; backend sem suporte
  → erro explícito.

---

## Vaga 5 — Consulta (promover "só cru" para abstracção) · ADR (H8)

### B-16 · Upsert `ON CONFLICT DO UPDATE/NOTHING` + `RETURNING` selectivo — Alto · M
### B-17 · `prefetch_related` (reverso/M2M) — fim do N+1 — Alto · M
- **Aceitação:** teste `prefetch_related_issues_two_queries_not_n_plus_one`
  (contar statements via `pg_stat_statements`/span).
### B-18 · Subqueries/`Exists`/`OuterRef` tipados — Médio · L
### B-19 · Paginação *keyset* + *streaming* por cursor — Médio · M
- **Aceitação:** `keyset_pagination_is_stable_under_inserts`; *streaming* não
  materializa (memória constante para 1M linhas).
### B-20 · CTE / funções de janela / `COPY` — Médio · L

---

## Vaga 6 — Objectos de BD como código · ADR 0061 (parte)

### B-21 · Índices avançados (composto/parcial/expressão/`INCLUDE`/GIN/GiST/BRIN) — Alto · M
### B-22 · `CREATE INDEX CONCURRENTLY` fora da transacção — Crítico · M
- **Achado:** MIG-01/H7 (provado: `25001` + `ShareLock`).
- **Mudança:** marcar statements "fora-de-transacção"; o applier corre-as cada uma
  na sua ligação, fora do `db::atomic` de `migrator.rs:77`; `lock_timeout` por
  migração.
- **Aceitação:** teste `concurrent_index_does_not_block_writes` — mede `pg_locks`
  durante a migração; um `INSERT` concorrente não bloqueia (hoje bloqueia).
### B-23 · Views/MV, funções, procedures (`CALL`), triggers, RLS, sequências, `COMMENT`, `EXTENSION` — Alto · L
- **Aceitação:** por objecto, aplicar→reverter→reaplicar + *snapshot*.

---

## Vaga 7 — Migrações zero-downtime · ADR 0061 (superseded by ADR 0022/0038)

### B-24 · Detecção de renomeação (fim da perda de dados) — Crítico · M
- **Achado:** MIG-02.
- **Mudança:** autodetect reconhece renomeação (hint explícito no esquema e/ou
  heurística revista pelo utilizador) → `ALTER TABLE … RENAME COLUMN`, nunca
  drop+add; SQLite preserva a coluna no rebuild.
- **Aceitação:** teste `renaming_a_column_preserves_its_data` — antes: dados
  perdidos (drop+add); depois: `RENAME COLUMN`, dados intactos; nos dois dialectos.
### B-25 · Ledger com checksum + `down`/rollback — Médio · M
- **Aceitação:** `editing_an_applied_migration_is_detected` (checksum diverge).
### B-26 · Expand/contract + backfill em lotes — Médio · L
### B-27 · Drift detection schema ↔ catálogo (não só aditivo) + `db pull` completo — Médio · L

---

## Vaga 8 — Externalização poliglota · ADRs 0055–0058 (REJECTED — ADR 0054)

### B-28 · Crate `dorm-ir`: `Plan` ↔ Query IR (`dorm.v1` proto) — Alto · L
- **Achado:** GW-01/DESIGN-03/H12. **ADR:** 0010.
- **Mudança:** `derive(Serialize)` onde aplicável; tipos proto; conversão pura
  IR→`Plan` com `model` por nome lógico resolvido por `registry::model`.
- **Aceitação:** *snapshots* IR→`Plan`→SQL idênticos aos *snapshots* tipados
  (prova de que os três front-ends convergem); `proptest` de que nenhum
  identificador do IR escapa a `registry::resolve`.
### B-29 · `SchemaDescriptor` gerado de `dorm.schema` (catálogo em runtime) — Alto · L
- **ADR:** 0011. **Aceitação:** o descritor reconstrói um registo equivalente à
  *distributed slice*; `registry::validate` passa igual sobre ambos.
### B-30 · Crate `dorm-server` (tonic): gRPC + streaming + deadlines — Alto · L
- **ADR:** 0012. **Aceitação:** teste de conformidade gRPC; *deadline* → cancel
  provado; streaming de 1M linhas com memória constante.
### B-31 · Transacções interactivas remotas (TTL + idempotency keys) — Alto · L
- **Aceitação:** sessão órfã expira e faz rollback; reenvio de escrita com a mesma
  idempotency key não duplica.
### B-32 · OpenAPI 3.1 + REST derivados da mesma definição — Médio · M
- **Achado:** GW-04. **Aceitação:** `schemathesis` contra `/openapi.json`; sem
  divergência handler↔documento; auth + tecto de `limit` + saneamento de erros.
### B-33 · Objectos de BD remotos + allow-list de SQL nomeado — Médio · M
- **ADR:** 0013. **Aceitação:** `proptest` de que texto SQL livre do cliente é
  rejeitado; só *statements* nomeados registados correm.
### B-34 · SDKs gerados (TS, Python, …) + `buf lint`/`buf breaking` no CI — Médio · L
- **ADR:** 0010. **Aceitação:** SDK TS e Python geram a mensagem `Query`; `buf
  breaking` falha o CI numa mudança incompatível de proto.

---

## Ordem e dependências

```
Vaga1 (correcção)  ──► independente, faz-se já, PR a PR
Vaga2 (concorrência) ─┐
Vaga3 (conexões)     ─┼─► B-12 é habilitador de multi-tenancy e de B-13/B-30
Vaga4 (tipos)        ─┘   (também desbloqueia JSONB/full-text em consulta)
Vaga5 (consulta) ──► usa Vaga4 (tipos) para arrays/JSONB
Vaga6 (objectos) ──► B-22 depende do applier fora-de-tx (partilha com B-24)
Vaga7 (zero-downtime) ──► B-24 é Crítico, sobe junto com a Vaga1 se houver risco vivo
Vaga8 (poliglota) ──► B-28/B-29 primeiro (tipos+conversão pura), depois B-30+
```

**Recomendação de arranque:** Vaga 1 inteira (seis tijolos, maioria S/M, fecham os
defeitos provados) + **B-24** e **B-22** antecipados da Vaga 6/7 por serem
Críticos com perda de dados/bloqueio. Só depois abrir a Vaga 8, que é esforço e
não risco e assenta num núcleo entretanto endurecido.
