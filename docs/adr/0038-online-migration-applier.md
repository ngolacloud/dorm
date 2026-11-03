# ADR 0038 — O *applier* de migrações online: backfill, expand/contract e `CREATE INDEX CONCURRENTLY` versionado

Status: Accepted · 2026-11-03 — **backfill por keyset, expand/contract de 1ª classe,
`migrate_online` com ledger por statement e `check_compat` implementados e provados
ao vivo; limpeza de índices `INVALID` e lock entre ligações deixados explicitamente
por fazer.**

## Context

O ADR 0022 abriu a frente das migrações online e entregou os primeiros *bricks* —
`lock_timeout` por migração e `dorm migrate lint`. Deixou, porém, escrito e por
fazer o grosso de L2: o *applier* que opera sem *stop-the-world*. O estado que
herdámos:

- **`migrator.rs::migrate()` envolve TODA a cadeia num único `db::atomic`**
  (`migrator.rs:108`, loop `:148-165`). Daí, um `CREATE INDEX CONCURRENTLY`,
  `ALTER TYPE … ADD VALUE` ou `VACUUM` é **proibido dentro de transacção** (erro
  `25001`) e portanto impossível de emitir por este caminho versionado.
- **`ddl.rs::index_on_concurrent`** já emite `CREATE INDEX CONCURRENTLY`, mas só o
  caminho `sync` o aplica fora de transacção (`migrator.rs::sync`, parte
  não-transaccional após o `atomic`). A máquina existia; faltava trazê-la ao
  `migrate` versionado, com *ledger* por statement para ser retomável.
- **Sem backfill em lotes.** Um `ADD COLUMN … NOT NULL` sem default é recusado em
  tabela populada (`ddl.rs`), e não havia caminho para preencher uma coluna em
  *batches* sem um `UPDATE` único que segura um lock e um snapshot longos.
- **Sem expand/contract de 1ª classe.** Não havia vocabulário para tornar uma
  coluna `NOT NULL` sem reescrever a tabela nem segurar um `ACCESS EXCLUSIVE` longo.
- **Sem `check-compat`.** Não havia forma de provar, antes do deploy, que o schema
  novo ainda serve o código N-1 que corre em simultâneo num upgrade rolling.

## Decision

Entregar os quatro *follow-ups* de L2 no crate `dorm-migrations`, por ordem de valor
e do que é provável numa instância única, reutilizando a disciplina que já existe.

### 1. Backfill em lotes por keyset (`online::backfill`)

Um helper que preenche uma coluna em lotes por *keyset* na PK
(`WHERE pk > $cursor ORDER BY pk LIMIT $batch`), **nunca um `UPDATE` único**:

- `BackfillSpec { table, pk_col, set_sql, filter_sql, batch, start_after, throttle }`.
  `table`/`pk_col` são **validados como identificadores** antes de entrarem no SQL
  (I1); `set_sql`/`filter_sql` são expressões do autor (a mesma confiança que
  `ModelMeta::checks` e o SQL raw do ADR 0005 já carregam); o cursor e o tamanho do
  lote ligam como `$1`/`$2` (I2).
- Cada lote é um `WITH batch AS (SELECT pk … LIMIT) UPDATE … FROM batch …
  RETURNING pk`. O maior `pk` devolvido torna-se o cursor seguinte; um lote vazio
  encerra o trabalho.
- **Retomável de um checkpoint persistido**: a tabela `orm_backfill_checkpoints`
  guarda, por `job`, o cursor, o total de linhas e um sinalizador `done`. Cada lote
  corre **na mesma transacção** que grava o seu checkpoint, pelo que o cursor em
  disco nunca fica à frente das linhas efectivamente actualizadas; um *crash*
  retoma do último lote confirmado, e um `job` concluído é um no-op.
- **Throttling** opcional (`throttle: Option<Duration>`) entre lotes, para limitar o
  ritmo de escrita. Reutiliza a ordem estável por PK do ADR 0024. PostgreSQL only.

### 2. Expand/contract de 1ª classe (`ddl::*`)

Vocabulário de DDL para tornar uma coluna `NOT NULL` sem reescrita nem lock longo:

- `add_not_null_check_not_valid(table, col)` → `ALTER TABLE … ADD CONSTRAINT
  <t>_<c>_not_null CHECK (<c> IS NOT NULL) NOT VALID`. O `NOT VALID` adiciona a
  restrição **sem varrer a tabela** (lock breve); linhas novas/actualizadas já são
  verificadas.
- `validate_not_null(table, col)` → `ALTER TABLE … VALIDATE CONSTRAINT …`. A
  validação varre sob um `SHARE UPDATE EXCLUSIVE` que deixa leituras e escritas
  fluir.
- `set_not_null`/`drop_constraint` opcionais: no PostgreSQL 12+ um `CHECK
  (c IS NOT NULL)` **validado** deixa o `ALTER COLUMN … SET NOT NULL` dispensar o
  *scan*, tornando a coluna um `NOT NULL` genuíno e permitindo largar o *check*.
- Formas genéricas `add_check_not_valid`/`validate_constraint` para qualquer
  restrição `CHECK` do autor. Todos os nomes passam pelo validador de identificador.

### 3. `CREATE INDEX CONCURRENTLY` a partir do `migrate` versionado (`online::migrate_online`)

Um *applier* versionado que reconhece as statements que não podem correr dentro de
transacção e as aplica fora dela, com *ledger* por statement:

- `is_non_transactional(sql)` (puro, case-insensitive) classifica tudo o que é
  `CONCURRENTLY` (`CREATE`/`DROP INDEX CONCURRENTLY`, `REINDEX … CONCURRENTLY`,
  `DETACH PARTITION CONCURRENTLY`), `ALTER TYPE … ADD VALUE` e `VACUUM`.
- Para cada versão ainda não registada, as statements aplicam-se **por ordem**: uma
  não-transaccional corre na sua própria ligação em *autocommit* (o que o
  `CONCURRENTLY` exige); qualquer outra corre num `db::atomic` de uma statement. Em
  qualquer dos casos, a conclusão da statement grava-se em `orm_migration_statements`
  (chave `(version, ord)`), pelo que um *re-run* após falha **salta as já feitas** e
  continua. Concluídas todas, a versão entra no *ledger* comum
  `orm_schema_migrations`.
- Idempotente: versão/​statement já registadas são saltadas; as formas
  `CONCURRENTLY`/`IF NOT EXISTS` toleram re-aplicação.

### 4. `check_compat(old, new)` (`online::check_compat`)

Função pura que, dados dois `Snapshot`, prova que o schema novo ainda serve o código
N-1. Assinala, por tabela/coluna do `old`:

- tabela ou coluna removida — N-1 ainda a nomeia;
- coluna apertada de *nullable* para `NOT NULL` **sem default** — um `INSERT` de N-1
  que a omite falharia;
- coluna apertada para `UNIQUE` — um `INSERT` de N-1 de um duplicado falharia;
- coluna cujo **tipo** mudou — leituras/escritas de N-1 podem não fazer *round-trip*.

Resultado vazio = retrocompatível. Adições (tabelas/colunas novas que N-1 desconhece)
são sempre compatíveis e não são reportadas.

### Como preserva os invariantes do `ARCHITECTURE.md`

Tudo vive em `dorm-migrations`, fora do compilador de consultas, que permanece puro
(**I3**). Os identificadores de tabela/coluna/restrição vêm do schema e passam por um
validador antes de qualquer *splice* em DDL (**I1**); o cursor e o lote do backfill
ligam como `$n` (**I2**). `check_compat` lê os mesmos `Snapshot` que o autodetect já
produz, sem base de dados.

## Consequences

- **Prós:** backfill por keyset + expand/contract + `CONCURRENTLY` versionado tornam
  viável evoluir uma tabela grande sem paragem e sob N/N-1 simultâneos; o *ledger*
  por statement torna uma migração online **retomável**, não reiniciável;
  `check_compat` recusa, antes do deploy, um schema que partiria N-1.
- **Contras / custo:** uma statement fora-de-transacção que falhe a meio deixa a
  migração **parcialmente aplicada** — daí o *ledger* por statement ser obrigatório;
  o expand/contract exige disciplina de mais de uma migração por mudança (apertar
  `NOT NULL` são 3-4 passos, não um).
- **Dependências:** `set_local`/backoff do ADR 0023 (já no `migrate`); ordem estável
  por PK do ADR 0024 (keyset do backfill); o `index_on_concurrent` do ADR 0022.

## Proof

Unitária (offline, `cargo test -p dorm-migrations`, sem base de dados):

- `ddl::tests` — o `CHECK … NOT VALID` + `VALIDATE` + `SET NOT NULL` + `DROP
  CONSTRAINT` geram o SQL esperado sob nome de restrição estável; injecção em
  nome de tabela/coluna/restrição é recusada.
- `online::tests` — o SQL de um lote de backfill pagina pela chave e liga
  cursor/lote como `$1`/`$2` (com e sem filtro); injecção recusada;
  `is_non_transactional` acerta em `CONCURRENTLY`/`ALTER TYPE … ADD VALUE`/`VACUUM`
  e **não** em DDL comum; `check_compat` aceita um superconjunto aditivo e **recusa**
  coluna/tabela removida, `NOT NULL` novo sem default e mudança de tipo (e aceita um
  `NOT NULL` novo **com** default).

Ao vivo (`DELONIX_ORM_TEST_DSN`, tabelas `l2_*`, re-executável):

- **backfill** — uma tabela de **50 000 linhas** preenchida em **10 lotes** de 5 000
  (prova `batches > 1`: nunca um `UPDATE` único), valor correcto em todas as linhas,
  re-run é no-op, e um *job* semeado a meio **retoma** do checkpoint até ao fim.
- **expand/contract** — tabela populada (20 000, com NULLs): backfill dos NULLs em
  lotes → `CHECK NOT VALID` → `VALIDATE` → a restrição recusa um `INSERT` NULL (a
  coluna é efectivamente `NOT NULL`) → `SET NOT NULL` + `DROP CONSTRAINT` deixam-na
  `NOT NULL` genuíno (`pg_attribute.attnotnull`), **sem erro**.
- **migrate_online** — uma migração cuja 3ª statement falha deixa `ord 0/1` no
  *ledger* por statement e a versão **não** registada; corrigida (a 3ª statement
  passa a `CREATE INDEX CONCURRENTLY`), o *re-run* **salta** `ord 0/1` (sem linhas
  duplicadas), aplica o índice **fora de transacção sem `25001`**, o índice fica
  **válido** (`pg_index.indisvalid`), e a versão regista-se; 3ª execução é no-op.

Comando dos testes live:

```text
DELONIX_ORM_TEST_DSN='host=/var/run/postgresql port=5432 user=walter dbname=ormtest' \
  cargo test -p dorm-migrations --test online_live -- --ignored --nocapture
```

## O que ficou parcial, e porquê (honesto)

- **Lock de serialização entre ligações no `migrate_online`.** O `migrate`
  transaccional segura um `pg_advisory_xact_lock` para serializar *runners*
  concorrentes. O `migrate_online` corre statements em ligações independentes, pelo
  que um *xact lock* não se mantém entre elas, e um *session lock* exigiria fixar a
  mesma ligação — fora do modelo de *pool*. Entregámos o **caminho mínimo**: assume o
  *deploy* de *runner* único (um Job/pod de migração de cada vez, o caso normal) e
  apoia-se no *ledger* por statement + `IF NOT EXISTS`. O lock entre ligações fica
  para uma fase própria.
- **Limpeza de índices `INVALID`.** O ADR 0022 previa, antes de um `CREATE INDEX
  CONCURRENTLY`, detectar um índice homónimo `INVALID` (de uma tentativa falhada, via
  `pg_index.indisvalid`) e fazer `DROP INDEX CONCURRENTLY` antes de recriar. **Não
  está feito**: o `IF NOT EXISTS` trata o caso "já válido", mas não limpa um `INVALID`
  deixado por uma falha anterior. É uma adição pequena e isolada, deixada por fazer
  para não a declarar sem prova.
- **`check_compat` a partir de statements SQL em bruto.** A função opera sobre
  `Snapshot` (o que o autodetect já produz). Uma variante que *parseia* dois conjuntos
  de statements DDL para inferir o schema não foi feita — o caminho por `Snapshot` é o
  limpo e é o que o projecto tem à mão em `makemigrations`.
- **Backfill só PostgreSQL.** Usa `WITH … UPDATE … FROM … RETURNING` e `ON CONFLICT`.
  SQLite não é alvo destes primitivos online; `migrate_online` corre em ambos (no
  SQLite simplesmente não encontra statements não-transaccionais).
