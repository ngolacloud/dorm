# Eixo DBA — correcção SQL, concorrência, transacções, tipos

> Revisão apenas de leitura (Fase 1). Hipóteses atribuídas: H2, H3, H4, H5.
> Evidência por `ficheiro:linha`. Veredicto por hipótese no fim.
> Português europeu; identificadores em inglês.

Contagem: **1 Crítico · 2 Altos · 1 Médio** (4 achados; 3 hipóteses novas
embebidas — H4a/H4b na capacidade, nenhuma refuta as atribuídas).

---

## [DBA-01] `get_or_create`/`update_or_create` dentro de `atomic()` abortam a transacção (25P02) — Crítico

- **Hipótese:** H3 (confirmada).
- **Evidência:**
  - `crates/dorm-core/src/queryset.rs:165-183` — `get_or_create` faz
    `SELECT` (`first()`), depois `create()` (um `INSERT`), e ao apanhar
    `is_unique_violation` recorre a **outro `SELECT`** (`first()`):
    ```rust
    if let Some(found) = QuerySet::<T>::new().filter(lookup.clone()).first().await? {
        return Ok((found, false));
    }
    match self.create(new).await {
        Ok(created) => Ok((created, true)),
        Err(e) if crate::error::is_unique_violation(&e) => {
            let found = QuerySet::<T>::new().filter(lookup).first().await?.ok_or(e)?;
            Ok((found, false))
        }
        Err(e) => Err(e),
    }
    ```
  - `crates/dorm-core/src/queryset.rs:199-215` — `update_or_create` tem o mesmo
    padrão (SELECT → `create` → em 23505, `apply_and_fetch` que volta a correr
    `UPDATE`+`SELECT`).
  - **Não há `SAVEPOINT` próprio** em nenhuma das duas funções. O único savepoint
    existe quando se *aninha* `atomic()` (`db.rs:1281-1320`), não aqui.
  - `crates/dorm-core/src/db.rs:1324-1335` — `atomic_pg` (ramo externo) adquire
    **uma** conexão dedicada (`acquire()`) e emite `BEGIN` directamente; todas as
    consultas internas (`queryset` → `run_query` → `db::query` → `query_inner`,
    `db.rs:660-666`) são roteadas para **essa mesma conexão** via `CURRENT_TX`.
  - Não existe caminho `ON CONFLICT` em todo o compilador
    (`grep "ON CONFLICT"` → 0 ocorrências em `compiler.rs`).
- **Impacto:** quando o `INSERT` perde a corrida e falha com `23505`
  (unique_violation), a transacção **na conexão dedicada** fica em estado
  abortado. O `SELECT` de recuperação corre nessa mesma conexão e o servidor
  responde `25P02 current transaction is aborted, commands ignored until end of
  transaction block`. `from_pg` mapeia-o para `Error::Db` (não é
  unique_violation), o `?` propaga-o, e **toda a `atomic()` aborta**. A promessa
  de idempotência (documentada em `queryset.rs:162-164`) é **falsa dentro de uma
  transacção**: em vez de devolver a linha do vencedor, devolve um erro opaco e
  desfaz trabalho não relacionado no mesmo bloco. Fora de `atomic()` funciona
  (cada statement é a sua própria tx), o que torna o defeito latente e traiçoeiro
  — passa nos testes isolados e falha sob concorrência em produção.
- **Reprodução:** teste live concorrente.
  1. `db::atomic(|_| async { Tenant::objects().get_or_create(Q::new("slug","acme"), new).await }).await`.
  2. Numa segunda conexão (outro processo/tarefa), inserir e **committar**
     `slug="acme"` na janela entre o `SELECT` e o `INSERT` do passo 1 (forçável
     com um `BEGIN; INSERT; COMMIT` externo, ou um barrier/`tokio::join!` de dois
     `get_or_create` na mesma `slug`).
  3. Observado: o `INSERT` interno dá `23505`; o `SELECT` de recuperação dá
     `ERROR [25P02] ...`; `get_or_create` devolve `Err(Db("...25P02..."))` e a
     `atomic` faz `ROLLBACK`. Sem o passo em `atomic`, devolve `(row, false)`.
- **Recomendação (mínima):** tornar o `INSERT` de recuperação atómico ao nível do
  servidor. Duas vias, por ordem de preferência:
  1. compilar `INSERT ... ON CONFLICT (<lookup cols>) DO NOTHING RETURNING ...`
     (ou `DO UPDATE` para `update_or_create`) — elimina a janela de corrida e o
     `SELECT` de recuperação por completo; **um** statement, nunca aborta a tx;
  2. se ON CONFLICT ficar para um tijolo maior, envolver a tentativa de `INSERT`
     num `SAVEPOINT` (reutilizar `atomic` aninhado já existente), de modo que o
     `23505` só faça `ROLLBACK TO SAVEPOINT` e o `SELECT` de recuperação corra
     numa transacção de novo utilizável.
- **Esforço:** M (via 2 — reutiliza o savepoint existente; via 1 exige ON CONFLICT
  no compilador + lista de colunas do conflito).
- **ADR/Tijolo:** tijolo próprio "upsert / get_or_create seguro em transacção";
  encosta ao ADR de ON CONFLICT (H8) e ao controlo de concorrência (ADR 0004
  follow-up).

---

## [DBA-02] Sem controlo de concorrência: isolamento, retry, row-locks, SET LOCAL, optimistic locking, DEFERRABLE — Alto

- **Hipótese:** H4 (confirmada).
- **Evidência:** ausências confirmadas por varrimento
  (`grep -rniE "FOR UPDATE|SKIP LOCKED|NOWAIT|ISOLATION|SERIALIZABLE|40001|40P01|lock_timeout|idle_in_transaction|ON CONFLICT|optimistic|DEFERRABLE" crates/dorm-core/src crates/dorm-migrations/src` → só o advisory lock *interno* do migrador e um `application_name` de teste).
  - `crates/dorm-core/src/db.rs:1326-1331` — a transacção abre sempre com `BEGIN`
    literal (ou `BEGIN READ ONLY`); **nunca** `SET TRANSACTION ISOLATION LEVEL`.
    Não há parâmetro de nível de isolamento em `atomic`.
  - `crates/dorm-core/src/error.rs:135-143` — `classify_db` só distingue `57014`
    (timeout); `40001` (serialization_failure) e `40P01` (deadlock_detected) caem
    em `Error::Db` genérico. Não há `is_serialization_failure` análogo a
    `is_unique_violation` (`error.rs:154`), logo **nenhum retry possível sem
    string-matching**.
  - `crates/dorm-core/src/compiler.rs` — `compile_select*` não tem `FOR UPDATE`/
    `FOR SHARE`/`NOWAIT`/`SKIP LOCKED` (os lookups terminam em `compiler.rs:349`;
    não há cláusula de bloqueio em lado nenhum).
  - `crates/dorm-migrations/src/migrator.rs:51,80,151` — `pg_advisory_xact_lock`
    existe **apenas** para serializar migrações, com chave fixa; não é API geral.
  - `statement_timeout` é só **de processo** via DSN (`db.rs:59,283-288`); não há
    `SET LOCAL lock_timeout`/`idle_in_transaction_session_timeout`/
    `application_name` por transacção, nem GUC de sessão para RLS
    (`SET LOCAL app.tenant_id`).
  - Sem coluna `version`/optimistic locking em `FieldMeta` ou `QuerySet`.
  - `crates/dorm-migrations/src/ddl.rs:141-150` — `REFERENCES ... ON DELETE` mas
    **nunca** `DEFERRABLE`/`INITIALLY DEFERRED`.
- **Matriz de capacidades (abstracção / SQL cru ADR 0005 / ausente):**

  | Capacidade | Abstracção | SQL cru (`Manager::raw`/`db::sql`) | Veredicto |
  |---|---|---|---|
  | Nível de isolamento por tx | ✗ | parcial/frágil: 1º `db::execute("SET TRANSACTION ISOLATION LEVEL ...")` dentro do closure de `atomic` (tem de ser o primeiro statement após `BEGIN`) | **~ausente** |
  | Retry 40001/40P01 c/ backoff | ✗ (nem classificação) | caller faz loop + string-match "40001" | **ausente** |
  | `SAVEPOINT` explícito | ✓ (`atomic` aninhado, `db.rs:1281`) | — | **suportado** |
  | `FOR UPDATE/SHARE` + `NOWAIT`/`SKIP LOCKED` | ✗ | ✓ só via SELECT cru completo (não compõe com `QuerySet`) | **só cru** |
  | Advisory locks | ✗ (só interno migrador) | ✓ `db::sql("SELECT pg_advisory_xact_lock($1)", ...)` | **só cru** |
  | `SET LOCAL lock_timeout`/`statement_timeout`/`idle_in_transaction` | ✗ (timeout só por processo) | ✓ `db::execute("SET LOCAL ...")` dentro de `atomic` | **só cru** |
  | `application_name` | só via DSN | — | **só DSN** |
  | GUC de sessão p/ RLS (`SET LOCAL app.tenant_id`) | ✗ | ✓ via `db::execute` em `atomic` | **só cru** |
  | Optimistic locking (coluna `version`) | ✗ | emulável: `filter(version=x).update(...)` + checar linhas afectadas | **ausente** |
  | Constraints `DEFERRABLE` | ✗ | ✓ só via `db::run_ddl` à mão | **só cru** |

- **Impacto:** DORM é inseguro para cargas concorrentes de grau DBA. Sem
  isolamento configurável não há `SERIALIZABLE`/`REPEATABLE READ` para
  invariantes; sem retry 40001 o padrão canónico "serializável + repetir" é
  impossível pela abstracção; sem `FOR UPDATE`/`SKIP LOCKED` não há filas de
  trabalho nem reserva pessimista; sem `SET LOCAL app.tenant_id` o RLS
  multi-tenant não é utilizável; `lock_timeout`/`idle_in_transaction` ausentes
  deixam transacções penduradas a segurar locks. Tudo o que existe é só por SQL
  cru, perdendo a segurança/composição da abstracção.
- **Reprodução:** duas tarefas em `SERIALIZABLE` sobre a mesma linha → uma recebe
  `40001`; o código não tem como classificar nem repetir — o erro sobe opaco.
  Duas `atomic` a tocar linhas em ordem inversa → `40P01` deadlock, idem.
- **Recomendação (mínima):**
  1. `atomic_with(IsolationLevel, read_only)` que emita `BEGIN ISOLATION LEVEL ...`;
  2. `is_serialization_failure`/`is_deadlock` em `error.rs` + `classify_db` a
     nomear `40001`/`40P01`, e um helper `retry_on_serialization(n, backoff, f)`;
  3. `.lock_for_update()`/`.skip_locked()`/`.nowait()` no `QuerySet` →
     cláusula no `compile_select`;
  4. `set_local(key, value)` seguro (chave validada) para GUCs/RLS;
  5. optimistic locking por coluna `version` (increment + `WHERE version = $old`,
     erro se 0 linhas).
- **Esforço:** L (vários tijolos; 1–2 são S/M e desbloqueiam o essencial).
- **ADR/Tijolo:** ADR 0004 nomeia explicitamente "retry/reconnect with backoff"
  como follow-up (`0004:80`); isolamento + row-locks + RLS pedem ADR próprio
  "controlo de concorrência".

---

## [DBA-03] `prepare_cached` incondicional: incompatível com PgBouncer transaction mode, cache sem limite, plano obsoleto pós-`ALTER` — Alto

- **Hipótese:** H2 (confirmada).
- **Evidência:**
  - `crates/dorm-core/src/db.rs:665,669` (`query_inner`) e `699,703`
    (`execute_inner`) — **todo** o caminho PostgreSQL chama
    `conn.prepare_cached(sql)`, dentro e fora de transacção. Não há variante sem
    prepare nem flag de desactivação.
  - Nenhum limite de cache: `prepare_cached` usa o `StatementCache` por conexão do
    deadpool, que **não tem tecto** nem política de evicção; `RecyclingMethod::Fast`
    (`db.rs:422-423`) preserva a cache através de checkouts.
  - Interacção com H8/H9: `IN` expande a um placeholder por elemento
    (`compiler.rs:332-343`) e `bulk_create` emite `VALUES` com N tuplos
    (`queryset.rs:149-158`, `compiler.rs:1039-1056`) → **texto SQL distinto por cada
    N**, logo uma entrada de cache nova por cada cardinalidade → crescimento sem
    limite da cache por conexão.
  - `docs/adr/0007-prepared-statement-cache-and-benchmarks.md:111-121` reconhece
    o risco `cached plan must not change result type` pós-DDL mas delega-o ao
    chamador ("owns that interaction"); não há invalidação nem `DEALLOCATE`.
- **Impacto:**
  - **PgBouncer em `transaction` mode:** os named prepared statements do cliente
    são por-sessão no servidor; em pooling de transacção o PgBouncer multiplexa
    conexões de servidor por transacção, pelo que um handle preparado numa
    transacção não existe na conexão de servidor atribuída à seguinte →
    `prepared statement "sN" does not exist` / `already exists`. Só é seguro com
    PgBouncer ≥ 1.21 **e** `max_prepared_statements>0` configurado — o que o DORM
    não negoceia nem documenta como requisito. Sem opção de desligar o cache
    (`conn.query(sql, …)` directo), DORM **não corre atrás de PgBouncer txn mode**.
  - **Cache ilimitada:** com `IN`/`bulk_create` de cardinalidade variável a cache
    por conexão cresce indefinidamente — fuga de memória no servidor e no cliente
    em cargas com listas de tamanhos diversos.
  - **Plano obsoleto:** um `ALTER TABLE` que mude o tipo de resultado de um
    statement em cache faz a conexão viva rejeitar com
    `cached plan must not change result type` — reconhecido mas não mitigado.
- **Reprodução:**
  - PgBouncer: subir PG 17 + PgBouncer `pool_mode=transaction` sem
    `max_prepared_statements`; apontar `DELONIX_ORM_DSN` ao PgBouncer; correr duas
    consultas/execuções → erro de prepared statement inexistente (medição de
    Fase 3, container).
  - Cache: num só connection do pool, correr `filter(x__in=[...])` com N=1..1000 e
    inspeccionar `pg_prepared_statements` → N entradas distintas acumuladas.
  - ALTER: preparar um `SELECT *`, `ALTER TABLE ADD COLUMN` noutra conexão,
    re-executar na conexão que preparou → `cached plan must not change result type`.
- **Recomendação (mínima):**
  1. opção de execução sem cache (env/flag) para PgBouncer txn mode — cair em
     `conn.query(sql, …)` quando ligada;
  2. normalizar `IN` para `= ANY($1)` (um único placeholder de array) para colapsar
     a explosão de texto (resolve também H8/H9 na raiz) — texto estável = cache
     limitada;
  3. documentar o requisito PgBouncer (`max_prepared_statements`) e, opcionalmente,
     um tecto LRU na cache.
- **Esforço:** M.
- **ADR/Tijolo:** amplia ADR 0007 (secção "What this is not"); encosta ao tijolo
  `= ANY($1)` do eixo performance (H8/H9).

---

## [DBA-04] Conjunto de tipos estreito: 8 `FieldType`, faltam os tipos de produção PostgreSQL — Médio

- **Hipótese:** H5 (confirmada).
- **Evidência:**
  - `crates/dorm-core/src/value.rs:26-36` — `FieldType` tem exactamente 8 variantes:
    `I32, I64, Text, Bool, Decimal, Timestamp, Uuid, Json`.
  - `crates/dorm-core/src/value.rs:84-95` — `Value` espelha as mesmas 8.
  - Mapeamento por dialecto:
    - PostgreSQL — `value.rs:40-54` `sql_type()`:
      `INTEGER, BIGINT, TEXT, BOOLEAN, NUMERIC, TIMESTAMPTZ, UUID, JSONB`.
    - SQLite — `crates/dorm-migrations/src/ddl.rs:249-256` colapsa em
      `INTEGER` (I32/I64/Bool), `TEXT` (Text/Timestamp/Uuid/Json), `NUMERIC`
      (Decimal).
  - `FieldType` é deliberadamente **fechado** (`value.rs:23-25`): um tipo novo
    exige variante em `FieldType` **e** em `Value`, braço em `to_sql`
    (`value.rs:196-209`), `read_column` (`value.rs:331-364`), `coerce`
    (`value.rs:133-168`) e `sql_type`.
- **Impacto:** inutilizável para esquemas reais que precisem de ponto flutuante
  (`REAL`/`DOUBLE PRECISION`), datas/horas puras (`DATE`, `TIME`, `INTERVAL`),
  binário (`BYTEA`), inteiros pequenos (`SMALLINT`/`i16`), rede
  (`INET`/`CIDR`/`MACADDR`), `enum`s nativos, arrays, ranges, `tsvector`/`tsquery`,
  `MONEY`, `NUMERIC(p,s)` com precisão explícita, domínios, e extensões
  (PostGIS `geometry`, `pgvector`). Um DBA de produção bate nisto quase de imediato;
  o escape por SQL cru (ADR 0005) devolve linhas mas `from_row`/`read_column` não
  sabem descodificar o tipo → `UnexpectedColumnType`.
- **O que um DBA de produção precisaria de acrescentar (prioridade):**
  1. `F32`/`F64` (`REAL`/`DOUBLE PRECISION`) e `I16` (`SMALLINT`);
  2. `Date`/`Time`/`Interval` (chrono `NaiveDate`/`NaiveTime`/`Duration`);
  3. `Bytes` (`BYTEA`);
  4. `NUMERIC(p,s)` com precisão/escala (hoje `NUMERIC` nu, `value.rs:46-49`);
  5. arrays (`= ANY`/`int[]`/`text[]`), `INET`/`CIDR`/`MACADDR`;
  6. `enum` nativo (ligado ao tijolo de tipos/enums do eixo migrações);
  7. ranges, `tsvector`/`tsquery`, e um ponto de extensão para PostGIS/pgvector.
- **Reprodução:** declarar um modelo com campo `f64`/`NaiveDate`/`Vec<u8>` →
  não há `ColumnType`/`FieldType` que o derive possa atribuir (falha em
  compilação no `dorm-derive`); por SQL cru, um `SELECT col::float8` devolve linha
  mas `read_column` não tem braço → erro.
- **Recomendação (mínima):** começar pelos tipos escalares sem ambiguidade de
  descodificação (grupo 1–3), cada um como variante em `FieldType`+`Value` com os
  cinco braços; precisão de `NUMERIC` via atributo (já existe `max_length`,
  reutilizável). Arrays/enums/extensões são tijolos maiores subsequentes.
- **Esforço:** M (grupos 1–3); L (arrays/enums/ranges/extensões).
- **ADR/Tijolo:** ADR 0004 já nomeia "the wider `Value` set (`f64`, `bytea`,
  `date`/`time`, arrays)" como follow-up (`0004:82`); merece ADR próprio de
  "sistema de tipos" por envolver derive + migrações + descodificação.

---

## Veredicto por hipótese

| Hipótese | Veredicto | Achado |
|---|---|---|
| **H2** `prepare_cached` incondicional | **Confirmada** | DBA-03 |
| **H3** get_or_create/update_or_create em tx abortada (25P02) | **Confirmada** | DBA-01 |
| **H4** sem controlo de concorrência | **Confirmada** | DBA-02 |
| **H5** conjunto de tipos estreito | **Confirmada** | DBA-04 |

Nenhuma hipótese refutada. H3 sobe a **Crítico** por ser bug de correcção latente
(passa isolado, falha concorrente dentro de `atomic`), não mera lacuna de
funcionalidade. A reprodução viva de DBA-01 (corrida → 25P02) e DBA-03 (PgBouncer
txn mode, explosão de cache) fica para a Fase 2/3 com container, como previsto no
discovery §5.
