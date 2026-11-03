# ADR 0022 — Migrações online para upgrades rolling

Status: Proposed · 2026-11-03 — **lock_timeout + migrate lint implementados (Fase 4)**

## Implementação — primeiros bricks (Fase 4)

- **`lock_timeout` por migração** (`DELONIX_ORM_MIGRATE_LOCK_TIMEOUT_MS`): no início
  da transacção de `migrate` (depois do advisory lock, PostgreSQL), via o
  `db::set_local` seguro do ADR 0023 (`migrator.rs::migrate_lock_timeout_ms`). `0`/
  vazio mantém o default "esperar para sempre". Provado live
  (`dorm-models/tests/migrate_lock_timeout_live.rs`): um detentor segura
  `ACCESS EXCLUSIVE`, a migração com timeout 100 ms **falha depressa** (SQLSTATE
  `55P03`) em vez de pendurar, e reverte por completo (a coluna do `ALTER` não fica).
- **`dorm migrate lint`** (`dorm-migrations/src/lint.rs` + CLI): varre o DDL das
  migrações em `migrations/` e assinala operações perigosas sob upgrade rolling —
  `DROP TABLE`/`DROP COLUMN` (perda de dados), `ADD COLUMN NOT NULL` sem default,
  mudança de tipo (reescrita), `CREATE INDEX` não-concorrente (warning), rename
  (warning). Sai `1` num achado **Dangerous** — porta de CI. Operação pura de
  ficheiros (sem BD, sem modelos compilados). Provado por `lint::tests` (6).

**Por fazer (restante de L2):** o refactor do *applier* para statements
fora-de-transacção (`CREATE INDEX CONCURRENTLY` no `migrate`, não só no `sync`) com
limpeza de índices `INVALID`; expand/contract de 1ª classe (`CHECK NOT VALID` +
`VALIDATE`); framework de backfill por keyset retomável; `dorm migrate check-compat
--from`. Cada um é substancial e fica para fases próprias e testadas.

---


## Context

Num upgrade rolling em Kubernetes, a versão N e a versão N-1 da aplicação correm
ao mesmo tempo sobre o mesmo schema. O migrador actual não foi desenhado para
isso:

- **Toda a cadeia corre num único bloco transaccional.** `migrate deploy` itera
  todas as migrações pendentes dentro de um só `db::atomic`
  (`crates/dorm-migrations/src/migrator.rs:96`, loop `:129-146`). Daí: um
  `CREATE INDEX CONCURRENTLY`, `ALTER TYPE … ADD VALUE` ou
  `REFRESH MATERIALIZED VIEW CONCURRENTLY` é **proibido dentro de transacção**
  (erro `25001`) e portanto impossível de emitir por este caminho; e um
  `CREATE INDEX` normal segura um `SHARE` lock até ao `COMMIT` final da cadeia
  inteira, não só durante o índice.
- **Não há `lock_timeout`.** Uma espera por lock pode ser indefinida e empilhar
  ligações; só existe um advisory lock de serialização de chave fixa
  (`migrator.rs:72,100`), sem timeout nem retry/backoff.
- **Renomeação automática é destrutiva.** O autodetect compara colunas por nome
  (`autodetect.rs:184-186`): uma coluna renomeada entra em `DROP COLUMN` + `ADD
  COLUMN` e os dados perdem-se — salvo se o autor declarar o rename à mão
  (`ColumnRename`, `autodetect.rs:192`, provado por `rename_preserves_data_sqlite.rs`).
- **Sem backfill em lotes.** Um `ADD COLUMN … NOT NULL` sem default é rejeitado em
  tabela populada (`ddl.rs:164-166`, "sync is for additive changes, not
  backfills") sem caminho para preencher em *batches*.
- **Sem lint nem check de compatibilidade.** `MigrateAction`
  (`dorm-cli/src/main.rs:320-377`) tem `Make/Dev/Deploy/Resolve/Down/Status/Diff`
  — não há `Lint` nem `check-compat`.

Nota de reutilização: o caminho `sync` **já** emite `CREATE INDEX CONCURRENTLY`
fora de transacção (`ddl.rs:237-245` `index_on_concurrent`; aplicado após o
`db::atomic` em `migrator.rs:253-277`). A máquina existe; falta trazê-la para o
`migrate` versionado.

## Decision

Tornar o migrador versionado capaz de operar sem *stop-the-world*:

- **Statements fora-de-transacção, de primeira classe.** Cada `Statement` ganha um
  rótulo `transactional: bool`. `CREATE INDEX CONCURRENTLY`,
  `REFRESH … CONCURRENTLY`, `ALTER TYPE … ADD VALUE`, `VACUUM` correm **cada uma na
  sua própria ligação, fora do `db::atomic`**, com o *ledger* gravado por
  statement (retomável). Reutiliza o caminho que o `sync` já usa.
- **`lock_timeout` por migração, configurável.** `SET LOCAL lock_timeout` no início
  de cada transacção de migração, de `DELONIX_ORM_MIGRATE_LOCK_TIMEOUT_MS`, com
  retry + backoff limitado por statement DDL (o molde do ADR 0015). Uma espera por
  lock **falha e repete** em vez de empilhar ligações.
- **Limpeza de índices `INVALID`.** Antes de um `CREATE INDEX CONCURRENTLY`,
  detectar um índice homónimo `INVALID` (de uma tentativa falhada, via
  `pg_index.indisvalid`) e `DROP INDEX CONCURRENTLY` antes de recriar.
- **Expand/contract como operações.** Statements rotuladas por fase (`Expand` →
  `Backfill` → `Contract`): adicionar coluna nullable; backfill; `NOT NULL` via
  `ADD CONSTRAINT … CHECK (col IS NOT NULL) NOT VALID` seguido de
  `VALIDATE CONSTRAINT` (que não reescreve a tabela nem segura lock exclusivo);
  renomear em duas versões; `DROP` só depois de N-1 deixar de usar.
- **Framework de backfill.** Preenchimento em lotes por keyset na PK
  (`WHERE pk > $cursor ORDER BY pk LIMIT $batch`), retomável de um checkpoint
  persistido, com throttling entre lotes. Reutiliza a ordem estável do ADR 0024.
- **`dorm migrate lint`.** Classifica cada operação pendente como segura/perigosa
  (lock exclusivo longo, reescrita de tabela, `NOT NULL` directo, mudança de tipo,
  índice não-concorrente, rename ambíguo) e sai com código ≠ 0 em CI.
- **`dorm migrate check-compat --from <versão>`.** Prova que o schema novo serve o
  código N-1 (colunas/constraints de que N-1 depende continuam presentes e
  compatíveis), para validar a janela expand antes do deploy.

### Como preserva os invariantes do `ARCHITECTURE.md`

Tudo vive no `dorm-migrations` (*applier*, *ledger*, autodetect), fora do
compilador de consultas, que permanece puro (**I3**). As migrações lêem o mesmo
`ModelMeta` e geram DDL revisível; os identificadores vêm do schema, não de um
chamador (**I1**); o backfill liga valores como `$n` (**I2**). O oráculo de
paridade de DDL (`schema_parity.rs`) mantém-se.

## Consequences

- **Prós:** índices concorrentes, `lock_timeout`, expand/contract e backfill em
  lotes tornam viável evoluir uma tabela de um milhão de linhas sem paragem e sob
  N/N-1 simultâneos; o lint apanha a operação perigosa em CI, não em produção.
- **Contras / custo:** uma statement fora-de-transacção que falhe a meio deixa a
  migração parcialmente aplicada — o *ledger* por statement e o *applier*
  retomável são obrigatórios; expand/contract exige disciplina de mais de uma
  migração por mudança.
- **Dependências:** `set_local` seguro e backoff do ADR 0023; ordem estável por PK
  do ADR 0024 (keyset do backfill).

## Proof esperada

- **Unit/snapshot:** uma migração com `CREATE INDEX CONCURRENTLY` é rotulada
  não-transaccional e corre fora do `atomic`; o `NOT NULL` gera `CHECK … NOT VALID`
  + `VALIDATE`; `migrate lint` falha numa operação perigosa; `check-compat` recusa
  um schema que remove uma coluna que N-1 usa.
- **Live (`DELONIX_ORM_TEST_DSN`):** uma tabela de **um milhão de linhas** sob
  escrita contínua durante um ciclo expand/contract completo (add nullable →
  backfill em lotes → `VALIDATE` → contract), **sem erros e sem bloqueios acima do
  `lock_timeout`**; um `CREATE INDEX CONCURRENTLY` não dá `25001`; um índice
  `INVALID` deixado por uma falha é limpo e recriado.
