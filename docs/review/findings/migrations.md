# Eixo MIGRAÇÕES — achados (zero-downtime, drift, introspecção)

> Revisão só-leitura. Hipóteses do eixo: **H6** (objectos de BD não geridos),
> **H7** (migração num único bloco transaccional; sem expand/contract, sem
> detecção de renomeação, sem backfill), **H16** (`INSERT … RETURNING *` +
> descodificação posicional).
> Ficheiros: `crates/dorm-migrations/src/{ddl,autodetect,migrator,history,lib}.rs`,
> `crates/dorm-schema/src/parser.rs`, `docs/adr/0001-declarative-schema.md`.
> Ortografia pré-1990; identificadores em inglês.

---

## Síntese das capacidades pedidas pelo DBA

| Capacidade | Estado | Evidência |
|---|---|---|
| Índice B-tree de 1 coluna | **suportado** | `ddl.rs:208` `index_on` |
| Índice composto / parcial (`WHERE`) / expressão / `INCLUDE` / `GIN`/`GiST`/`BRIN`/`hash` | **ausente** | `ddl.rs:208` só `CREATE INDEX … (col)`; `indexes()` `ddl.rs:271` só itera `f.index` por coluna isolada |
| `CREATE INDEX CONCURRENTLY` | **ausente** (impossível hoje) | tudo corre dentro de `db::atomic` — `migrator.rs:77` |
| Views / Materialized Views / `REFRESH … CONCURRENTLY` | **ausente** | nenhuma ocorrência em `ddl.rs` |
| Funções / procedures (`CALL`) | **ausente** | idem |
| Triggers | **ausente** | idem |
| RLS (`ENABLE ROW LEVEL SECURITY` / `POLICY`) | **ausente** | idem |
| Partições (`PARTITION BY`) | **ausente** | idem |
| Sequências autónomas (`CREATE SEQUENCE`) | **ausente** (só `BIGSERIAL` implícito) | `ddl.rs:112` |
| Schemas / `search_path` | **ausente** (assume `public`) | `migrator.rs:212`, `main.rs:1410` |
| `COMMENT ON` | **ausente** | nenhuma ocorrência |
| `CREATE EXTENSION` | **ausente** | nenhuma ocorrência |
| Grants / roles | **ausente** | nenhuma ocorrência |
| `UNIQUE`/`CHECK` compostos (`@@unique`/`@@index`) em migração | **parcial** (recusa explícita no autodetect) | `autodetect.rs:274-281`; parser ignora `@@index` `parser.rs:229` |
| Alteração in-place de coluna (type/null/default/unique) em PG | **suportado** | `autodetect.rs:377-451` |
| Alteração de PK / FK | **ausente** (recusa explícita) | `autodetect.rs:384-395` |

---

## [MIG-01] Migração inteira num único bloco transaccional — bloqueia escritas e impede `CONCURRENTLY` — Crítica
- **Hipótese:** H7 (confirmada)
- **Evidência:** `migrator.rs:77`
  ```rust
  let applied = db::atomic(|_tx| async move {
      if is_postgres { db::run("SELECT pg_advisory_xact_lock($1)", …).await?; }
      … for m in migrations { for stmt in &m.statements { db::run(stmt, &[]).await?; } …
  ```
  `db::atomic` abre `BEGIN`/`SAVEPOINT` numa ligação dedicada (`db.rs:1258`,
  `atomic_pg` `db.rs:1274`). Todas as statements de **todas** as migrações
  pendentes correm dentro da mesma transacção. Não há `lock_timeout`/
  `statement_timeout` posto para a migração (só o `statement_timeout` global de
  ligação, `db.rs:282`, que *mata* a statement — não evita o bloqueio). Não
  existe a palavra `CONCURRENTLY` em todo o crate (grep vazio).
- **Impacto:**
  1. `CREATE INDEX CONCURRENTLY`, `ALTER TYPE … ADD VALUE`, `REFRESH MATERIALIZED
     VIEW CONCURRENTLY` e `VACUUM` são **proibidos dentro de uma transacção** pelo
     PostgreSQL (25001) — logo, impossíveis de emitir por este migrador.
  2. Um `CREATE INDEX` normal pega `SHARE` lock (bloqueia escritas na tabela) e,
     por estar na mesma transacção da migração toda, o lock mantém-se até ao
     `COMMIT` final — não só durante o índice.
  3. `ALTER TABLE … ALTER COLUMN … TYPE` (gerado em `autodetect.rs:402`) faz
     *table rewrite* sob `ACCESS EXCLUSIVE`, bloqueando **leituras e escritas**
     durante todo o rewrite; o advisory lock e a transacção única agravam a
     janela (nenhuma outra migração/sync pode progredir, e sem `lock_timeout`
     uma espera por lock pode ser indefinida, empilhando ligações).
  Resultado: qualquer migração não trivial é um *stop-the-world* — o oposto de
  zero-downtime.
- **Reprodução:** aplicar uma migração que crie um índice numa tabela grande e,
  noutra sessão, observar `SELECT * FROM pg_locks WHERE NOT granted` e tentar um
  `INSERT` concorrente — fica a bloquear até ao `COMMIT`. Confirmar que injectar
  um `CREATE INDEX CONCURRENTLY` na lista de statements falha com
  `25001 (… cannot run inside a transaction block)`.
- **Recomendação (mínima):** marcar statements "fora-de-transacção" (índices
  concorrentes, refresh concorrente) e, no applier, executá-las **cada uma na sua
  própria ligação sem `atomic`**, gravando o ledger separadamente (padrão de
  Django `atomic = False`/`AddIndexConcurrently`). Pôr `SET LOCAL lock_timeout`
  no início da transacção de migração. A longo prazo: expand→migrate→contract com
  statements rotuladas por fase.
- **Esforço:** M
- **ADR/Tijolo:** novo ADR "migrações não-transaccionais e `lock_timeout`"; revisão de `migrator.rs`.

## [MIG-02] Renomeação de coluna/tabela tratada como drop+add — perda de dados silenciosa — Crítica
- **Hipótese:** H7 (confirmada; sub-hipótese nova sobre perda de dados)
- **Evidência:** o autodetect compara colunas **só por nome** (`autodetect.rs:250-253`
  constrói os mapas por `c.name`; `autodetect.rs:290-321`). Uma coluna cujo nome
  mudou não casa em `prev_cols`/`cur_cols`, logo:
  - PostgreSQL: a antiga entra em `col_drops` (`autodetect.rs:316-320` →
    `DROP COLUMN IF EXISTS`) e a nova em `adds` (`autodetect.rs:299`
    `ADD COLUMN`). **Os dados da coluna antiga são destruídos.**
  - SQLite (rebuild): `shared` só inclui nomes presentes em ambos (`autodetect.rs:346-351`),
    pelo que a coluna renomeada **não é copiada** para `{table}__dorm_new`
    (`autodetect.rs:353-359`) e desaparece no `DROP TABLE` (`autodetect.rs:361`).
  Não há `RenameColumn`/`RenameModel`/sugestão interactiva (como o
  `makemigrations` do Django faz ao perguntar "did you rename…?"). O mesmo vale
  para renomear tabela: tabela antiga sem modelo → `DROP TABLE` (`autodetect.rs:201-205`),
  tabela nova → `CREATE TABLE`.
- **Impacto:** uma renomeação trivial no `dorm.schema` apaga a coluna/tabela e
  recria-a vazia, sem aviso. Além disso **não há backfill em lotes**: um
  `ADD COLUMN … NOT NULL` sem default é rejeitado em tabela populada
  (`ddl.rs:155-160`) e não há caminho para preencher em *batches* (grep
  `backfill|batch|chunk` vazio no src).
- **Reprodução:** criar tabela com dados, renomear uma coluna no schema, correr
  `dorm migrate make` + `deploy`; `SELECT` confirma a coluna nova toda NULL/ausente
  e a antiga ida. Para SQLite, o teste `a_sqlite_rebuild_runs_and_keeps_the_rows`
  (`autodetect.rs:789`) só prova preservação de colunas *partilhadas* — não cobre
  renomeação.
- **Recomendação (mínima):** detecção heurística de renomeação (mesmo tipo +
  posição, uma só coluna sumiu e uma só apareceu) com confirmação do operador, ou
  um atributo explícito `@@renamed_from`/operação `RenameColumn` na história;
  emitir `ALTER TABLE … RENAME COLUMN`. Enquanto não existir, **documentar o
  risco** e recusar a combinação drop+add ambígua em vez de a executar.
- **Esforço:** M
- **ADR/Tijolo:** ADR "renomeações e operações nomeadas na história de migração".

## [MIG-03] Objectos de BD não geridos (views, funções, triggers, RLS, partições, sequências, COMMENT, EXTENSION, grants) e índices só B-tree de 1 coluna — Alta
- **Hipótese:** H6 (confirmada)
- **Evidência:** `ddl.rs` só sabe gerar `CREATE TABLE` (`ddl.rs:70`), `ADD/DROP
  COLUMN` (`ddl.rs:161,187`), `DROP TABLE` (`ddl.rs:196`) e um índice B-tree de
  uma coluna (`ddl.rs:208` `CREATE INDEX IF NOT EXISTS {t}_{c}_idx ON {t} ({c})`).
  `indexes()` (`ddl.rs:271`) só itera campos com `f.index` — nunca compostos,
  parciais, por expressão, `INCLUDE`, ou método não-default. Grep por
  `view|trigger|function|policy|partition|sequence|comment on|extension|grant|
  materialized` em `ddl.rs` → **nenhuma ocorrência**. O parser nem sequer modela
  estes objectos: `@@index`/`@@unique` são **consumidos e ignorados**
  (`parser.rs:222-240`), e `@db.Text` é `Ignored` (`parser.rs:339`).
  `UNIQUE`/`CHECK` compostos existem no `ModelMeta` mas a sua *alteração* é
  recusada (`autodetect.rs:274-281`).
- **Impacto:** o DBA não pode exprimir no DORM a maior parte do catálogo de um
  esquema de produção. Tudo isto fica obrigatoriamente em SQL à mão fora do
  migrador (via `Manager::raw`/`db::sql`, ADR 0005), sem versão, sem snapshot, sem
  ledger — o drift entra pela porta das traseiras (ver MIG-06).
- **Reprodução:** inspecção de código (acima). Confirmável: declarar `@@index([a,b])`
  no schema e verificar que a migração gerada não cria índice algum.
- **Recomendação (mínima):** começar pelos dois de maior alavanca e menor risco —
  (a) índices compostos/parciais/por expressão via `@@index(..., type:, where:)`;
  (b) `COMMENT ON` (barato, sem bloqueio). Views/MV, funções, triggers, RLS,
  partições são tijolos próprios; cada um deve, por ora, **falhar explicitamente**
  (como já faz `UnsupportedChange`) em vez de ser silenciosamente omitido no diff.
- **Esforço:** L (conjunto); S por objecto isolado (COMMENT, índice parcial)
- **ADR/Tijolo:** ADR "objectos de esquema para lá da tabela"; roadmap por objecto.

## [MIG-04] Rebuild de tabela no SQLite — janelas de perda de dados e FK desligadas — Alta
- **Hipótese:** H7 (confirmada)
- **Evidência:** `sqlite_rebuild` (`autodetect.rs:340-365`): cria `{table}__dorm_new`,
  copia só colunas partilhadas, `DROP TABLE`, `RENAME`. Comentário assume
  `PRAGMA foreign_keys` **sempre desligado** (`autodetect.rs:332-335`); colunas só
  da nova forma ficam com default/NULL (`autodetect.rs:337-339`) — um `NOT NULL`
  sem default só é seguro em tabela vazia.
- **Impacto:** (1) perda de dados por renomeação (ver MIG-02, caminho SQLite);
  (2) se o processo *tiver* FK enforcement ligado por outra via, o `DROP TABLE`
  parte referências de entrada — o código conta com a FK estar desligada, o que é
  frágil; (3) qualquer `NOT NULL` novo sem default rebenta em tabela populada.
  O rebuild é correcto *desde que* não haja renomeações e FK esteja off — um
  conjunto de pressupostos não verificados em runtime.
- **Reprodução:** o teste `a_sqlite_rebuild_runs_and_keeps_the_rows`
  (`autodetect.rs:789`) cobre o caminho feliz; falta um teste com coluna renomeada
  (perde dados) e com `PRAGMA foreign_keys=ON` + FK de entrada.
- **Recomendação (mínima):** envolver o rebuild em `PRAGMA foreign_keys=OFF;` …
  `PRAGMA foreign_key_check;` … `ON` (procedimento canónico do SQLite) e recusar
  colunas novas `NOT NULL` sem default num rebuild de tabela com linhas.
- **Esforço:** S
- **ADR/Tijolo:** endurecimento de `sqlite_rebuild`.

## [MIG-05] Detecção de drift ausente/parcial; `db pull` unidireccional e com perdas — Média
- **Hipótese:** H7 (confirmada, vertente drift/introspecção)
- **Evidência:**
  - `sync`/`diff` comparam modelos com o catálogo **só aditivamente**:
    `introspect` lê apenas nomes de tabela e de coluna de `information_schema`
    (`migrator.rs:206-236`), e `changes_for` (`migrator.rs:242-262`) só acrescenta
    tabelas/colunas em falta. **Não detecta** colunas a mais, tipos divergentes,
    nullability, constraints perdidas, índices — e nunca remove nada.
  - `db pull` (`main.rs:1407`) é geração **unidireccional** schema→ficheiro,
    assumidamente aproximada e com perdas: "Not recovered: unique/index, column
    lengths, checks, ordering, and relation onDelete" (`main.rs:1420-1423`,
    `:1402-1406`). Assume sempre schema `public`.
  - Não existe comando que compare o `Snapshot`/`dorm.schema` com o catálogo vivo
    e reporte drift (o equivalente ao `prisma migrate status` com detecção de
    drift, ou `migrate diff --from-schema-datamodel --to-schema-datasource`). O
    `Snapshot` (`autodetect.rs:56`) é comparado só contra outro `Snapshot`, nunca
    contra a BD.
- **Impacto:** alterações feitas fora do migrador (incluindo todos os objectos de
  MIG-03) passam despercebidas; `sync`/`diff` dão falso "em dia" mesmo com o
  esquema divergente em tipo/constraint. O operador não tem como saber se a BD
  corresponde à história de migração.
- **Reprodução:** criar coluna à mão com tipo diferente do modelo; `dorm migrate
  diff`/`push` reporta "sem alterações". `db pull` → `dorm generate` → `diff`
  não fecha o ciclo (perde unique/index/checks).
- **Recomendação (mínima):** um `dorm migrate status` que introspeccione tipos e
  constraints e compare com o último `Snapshot`, reportando drift (sem o corrigir).
- **Esforço:** M
- **ADR/Tijolo:** ADR "detecção de drift contra catálogo".

## [MIG-06] Ledger sem checksum das statements e sem rollback/down — Média
- **Hipótese:** H7 (confirmada, vertente ledger)
- **Evidência:** `orm_schema_migrations(version BIGINT PK, name TEXT, applied_unix
  BIGINT)` (`migrator.rs:85-92`). Guarda só o facto de uma versão estar aplicada;
  **não guarda checksum/hash das statements** (o Prisma guarda `checksum` para
  detectar uma migração editada após aplicada). Cada `StoredMigration` tem só
  `statements` + `snapshot` (`history.rs:29-38`), sem `down`. `migrate` só avança
  (`migrator.rs:110-127`); não há `revert`/`down`. O advisory lock
  `pg_advisory_xact_lock(0x00720d61)` (`migrator.rs:53,80`) é correcto e serializa
  processos, e `migrate resolve` (`main.rs:756`) permite marcar aplicado sem correr
  — ponto positivo.
- **Impacto:** uma migração já aplicada mas editada no disco é reaplicada de forma
  incoerente sem detecção (a versão está no ledger → é saltada, ficando a BD e o
  ficheiro dessincronizados em silêncio). Sem *down*, não há reversão assistida —
  só `migrate resolve` + SQL manual.
- **Reprodução:** aplicar v2; editar `0002_*.json`; reaplicar → saltada sem aviso,
  apesar de divergir.
- **Recomendação (mínima):** acrescentar coluna `checksum` ao ledger e ao
  `StoredMigration`, e avisar quando o checksum do disco diverge do registado.
- **Esforço:** S
- **ADR/Tijolo:** evolução do schema do ledger.

## [MIG-07] H16 refutada como enunciada — o `INSERT` usa lista `RETURNING` explícita e ordenada, não `RETURNING *` — Baixa (informativa)
- **Hipótese:** H16 (refutada na forma literal; risco residual anotado)
- **Evidência:** apesar do comentário "`INSERT … RETURNING *`" (`compiler.rs:1018`),
  o SQL gerado é uma **lista explícita** de colunas em ordem de metadados:
  ```rust
  let returning = meta.columns().map(quote)…join(", ");  // compiler.rs:1065
  "INSERT INTO {} ({columns}) VALUES {} RETURNING {returning}"  // :1073
  ```
  `meta.columns()` devolve `self.fields.iter().map(|f| f.column)` (`meta.rs:153`),
  e o `ColumnSpec` de descodificação também vem de `meta.fields` na mesma ordem
  (`compiler.rs:1078-1086`). Logo a ordem do `RETURNING` **é igual** à ordem de
  descodificação posicional, independentemente da disposição física da tabela.
  Uma coluna acrescentada por migração entra na metadata e aparece em ambas as
  listas na mesma posição — não há desalinhamento neste caminho.
- **Impacto:** o cenário catastrófico de H16 (coluna nova desloca posições num
  `RETURNING *`) **não ocorre** na escrita compilada. O risco posicional real vive
  no caminho `raw()`/`SELECT *` (H10), fora deste eixo. Risco residual aqui: se a
  tabela física divergir da metadata (drift, MIG-05) tendo colunas que o modelo
  não tem, o `RETURNING` explícito ignora-as — comportamento seguro, não bug.
- **Reprodução:** ler o texto SQL gerado por `compile_insert_on`; confirmar que é
  lista nomeada e não `*`. Snapshot tests (`sql_snapshots.rs`) fixam-no.
- **Recomendação (mínima):** corrigir o comentário `compiler.rs:1018` ("RETURNING
  *") para reflectir a lista explícita, evitando propagar a premissa errada.
- **Esforço:** S
- **ADR/Tijolo:** —

---

### Contagem por severidade
- **Crítica:** 2 (MIG-01 bloqueio/CONCURRENTLY, MIG-02 perda de dados em renomeação)
- **Alta:** 2 (MIG-03 objectos não geridos + índices pobres, MIG-04 rebuild SQLite)
- **Média:** 2 (MIG-05 drift/pull, MIG-06 ledger sem checksum/down)
- **Baixa:** 1 (MIG-07 H16 refutada)

### Veredicto das hipóteses do eixo
- **H6 — confirmada** (`ddl.rs:208,271`; parser ignora `@@index` `parser.rs:229`).
- **H7 — confirmada** (`migrator.rs:77` transacção única; `autodetect.rs:290-320`
  drop+add sem renomeação; sem backfill/expand-contract; sem `CONCURRENTLY`).
- **H16 — refutada na forma literal** (`compiler.rs:1065` lista `RETURNING`
  explícita em ordem de metadata; só o comentário `:1018` diz "`*`").
