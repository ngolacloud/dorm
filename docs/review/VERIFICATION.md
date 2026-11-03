# Verificação independente (Fase 2)

> Verificador independente — **não** participou na revisão da Fase 1. Objectivo:
> reproduzir cada achado Crítico e Alto, tentando **refutá-lo**, e marcá-lo
> `confirmed` / `refuted` / `needs-evidence`. Os achados live-reproduzíveis foram
> corridos contra bases de dados vivas; os restantes foram confirmados por leitura
> de código (`ficheiro:linha`). Português europeu (ortografia pré-1990);
> identificadores em inglês.
>
> Data: 2026-10-31. Ambiente: PostgreSQL 17.11 em `127.0.0.1:55432`, PgBouncer em
> `127.0.0.1:6432` (modo *transaction*, provado abaixo), PostgreSQL 16.15 local.
> `pg_stat_statements` activo. Nenhuma alteração ao código do repositório.

---

## Tabela-resumo

| Achado | Severidade (proposta) | Estado | Método | Nota |
|---|---|---|---|---|
| **DBA-01 / H3** get_or_create em tx aborta (25P02) | Crítico | **confirmed** | prova viva (23505→25P02) + leitura `queryset.rs:165-183`, `db.rs:1324-1331` | Severidade correcta. Bug de correcção latente, não lacuna. |
| **MIG-01 / H7** migração num só bloco transaccional | Crítico | **confirmed** | prova viva (25001 + lock_timeout) + leitura `migrator.rs:77` | Severidade correcta. |
| **MIG-02** renomeação = drop+add (perda de dados) | Crítico | **confirmed** | leitura `autodetect.rs:290-321`, `:340-365` | Severidade correcta. |
| **DESIGN-01 / H1+H13** estado global é a fonte de dados | Crítico | **confirmed** | leitura `db.rs:74,75,876` | Severidade correcta. |
| **DBA-03 / H2 / PERF-01** `prepare_cached` incondicional vs PgBouncer txn | Alto | **confirmed** | prova viva (26000 "prepared statement does not exist") + leitura `db.rs:665,669,699,703` | Severidade correcta. |
| **PERF-02 / H9** `bulk_create` sem chunking (tecto 65535) | Alto | **confirmed** | prova viva (65535 OK / 65536 ERRO, tokio-postgres) + leitura `queryset.rs:149-158`, `compiler.rs:1039-1056` | Severidade correcta. |
| **DBA-02 / H4** sem controlo de concorrência | Alto | **confirmed** | leitura (ausência) `error.rs:135-143`, `compiler.rs`, `db.rs:1326-1331` | Severidade correcta. |
| **MIG-04 / H7** rebuild SQLite (FK/janelas de perda) | Alto | **confirmed** | leitura `autodetect.rs:326-365` | Severidade correcta. |
| **SEC-01** POST da API gerada contorna `guard_write` | Alto | **confirmed** | leitura `codegen_api.rs:171-176`, `model.rs:158-160`, `db.rs:642-705` | Severidade defensável; **ver nota** (gatilho estreito). |
| **SEC-02 / H11** API gerada sem auth/tenant/mass-assignment | Alto | **confirmed** | leitura (ausência) `codegen_api.rs:145-257` | Severidade correcta. |
| **SEC-03 / H15** `READ_ONLY` global de processo | Alto | **confirmed** | leitura `db.rs:75,238-253` | Severidade correcta. |
| **DESIGN-03 / GW-01 / H12** `Plan`/`Sql` sem `Serialize` | Alto | **confirmed** | leitura `compiler.rs:38-74`; grep sem `Serialize` | Severidade correcta. |
| **GW-03 / H11+H12** sem `tonic`/`prost`/`buf`, sem `dorm-server` | Alto | **confirmed** | grep (zero) em todos os `Cargo.toml`; `ls crates/` | Severidade correcta. |
| **ADOPT-01** sem crate-chapéu `dorm` (nome ocupado) | Alto | **confirmed** | leitura `dorm-cli/Cargo.toml:26`, `README.md`; estrutura de 10 crates | Severidade de adopção, não técnica — ver nota. |
| **ADOPT-02** `dorm-derive` não depende de `dorm-core` (sem contrato SemVer) | Alto | **confirmed** | leitura `dorm-derive/Cargo.toml` (deps = `syn`/`quote`/`proc-macro2`) | Severidade de adopção — ver nota. |
| **H16** `INSERT … RETURNING *` + descodificação posicional | — | **refuted** | leitura `compiler.rs:1065-1069` (lista explícita, não `*`) | Confirma a refutação da Fase 1 (MIG-07, SEC-06). |

**Totais:** 15 confirmados · 1 refutado (H16) · 0 sem evidência.

---

## Provas vivas

### DBA-01 / H3 — unique_violation aborta a transacção (25P02) — `confirmed`

**Leitura primeiro.** `queryset.rs:165-183` (`get_or_create`): `SELECT` (`first()`) →
`create()` (um `INSERT`) → ao apanhar `is_unique_violation` corre **outro `SELECT`**
(`first()`). Não há `SAVEPOINT` próprio nem `ON CONFLICT` (grep `ON CONFLICT` →
zero). `db.rs:1324-1331` (`atomic_pg`, ramo externo) adquire **uma** conexão
dedicada e emite `BEGIN`; todas as consultas internas são roteadas para essa mesma
conexão via `CURRENT_TX` (`db.rs:660-666`, `693-705`). Logo, o `SELECT` de
recuperação corre na conexão já abortada.

**Prova viva** (PG 17, `55432`), reproduzindo a sequência que o `get_or_create`
executa dentro de `atomic()` — INSERT perdedor seguido do SELECT de recuperação na
mesma transacção:

```
BEGIN
ERROR:  23505: duplicate key value violates unique constraint "h3_tenant_slug_key"
DETAIL:  Key (slug)=(acme) already exists.
ERROR:  25P02: current transaction is aborted, commands ignored until end of transaction block
ROLLBACK
```

Observado: o `INSERT` dá **23505**; o `SELECT` de recuperação, na mesma conexão/tx,
dá **25P02** e é ignorado. `classify_db` (`error.rs:135-143`) mapeia o 25P02 para
`Error::Db` (não é `is_unique_violation`), o `?` propaga-o e **toda a `atomic()`
aborta**. Fora de `atomic` cada statement é a sua própria tx e o padrão funciona —
daí o defeito passar nos testes isolados. **Severidade Crítica confirmada.**

### MIG-01 / H7 — migração num só bloco transaccional — `confirmed`

**Leitura.** `migrator.rs:77` corre `db::atomic(|_tx| …)` envolvendo **todas** as
statements de **todas** as migrações pendentes numa única transacção dedicada
(`db.rs:1324-1331`); a palavra `CONCURRENTLY` não existe no crate; não há
`SET LOCAL lock_timeout`.

**Prova viva (a) — `CREATE INDEX CONCURRENTLY` dentro de `BEGIN`:**

```
BEGIN
ERROR:  25001: CREATE INDEX CONCURRENTLY cannot run inside a transaction block
ROLLBACK
```

**Prova viva (b) — um `CREATE INDEX` normal numa tx aberta segura um lock que
bloqueia escritas.** Sessão A: `BEGIN; CREATE INDEX … ; pg_sleep(6); COMMIT`.
Sessão B, concorrente, com `lock_timeout=3s`, tenta `INSERT`:

```
SET
ERROR:  canceling statement due to lock timeout
```

`pg_locks` mostra o `ShareLock` (granted) na relação durante a janela. O `INSERT`
ficou bloqueado até expirar — e numa migração real o lock manter-se-ia até ao
`COMMIT` final de **toda** a migração, não só durante o índice. **Severidade
Crítica confirmada.**

### DBA-03 / H2 / PERF-01 — PgBouncer *transaction mode* quebra prepared statements — `confirmed`

**Leitura.** `db.rs:665,669` (`query_inner`) e `699,703` (`execute_inner`) chamam
`conn.prepare_cached(sql)` **incondicionalmente**, dentro e fora de transacção, sem
variante sem-prepare nem flag de desactivação.

**Prova viva.** Confirmado primeiro que o PgBouncer em `6432` expõe múltiplos
backends de servidor concorrentes (PIDs `81, 113, 114, 115, 116` — pool ≥ 5),
característica de *pooling* de transacção. Depois, prova determinística da perda do
*prepared statement* entre transacções:

- Cliente A (via bouncer): `BEGIN; PREPARE p AS SELECT 42;` e mantém a tx **aberta**,
  fixando o seu backend — `A-backend=81`.
- Cliente B (via bouncer, concorrente, transacção separada) cai forçosamente noutro
  backend e executa `p`:

```
B-backend=113
ERROR:  26000: prepared statement "p" does not exist
```

- Contraprova (directo ao PG 17, sem bouncer, mesma sessão): `PREPARE … ; EXECUTE …`
  devolve `42` sem erro.

O named prepared statement vive na conexão de servidor onde foi preparado; em
*pooling* de transacção a transacção seguinte é multiplexada para outra conexão de
servidor que nunca viu o `Parse` → erro 26000. Como o DORM prepara em cada
`query`/`execute` sem opção de desligar, não corre com segurança atrás de PgBouncer
*transaction mode* (sem `max_prepared_statements` negociado). **Severidade Alta
confirmada.**

### PERF-02 / H9 — `bulk_create` estoura o tecto de 65 535 parâmetros — `confirmed`

**Leitura.** `queryset.rs:149-158` (`bulk_create`) e `compiler.rs:1039-1056`
(`compile_insert_on`) emitem **um** `INSERT … VALUES (…),(…),…` com um placeholder
por célula (`N_linhas × colunas`), sem partição em lotes.

**Prova viva** (programa `tokio-postgres` 0.7.18 — o mesmo driver do DORM — contra
o PG 17, protocolo estendido, tal como o DORM usa):

```
n_params= 65535 => OK (65535 rows)
n_params= 65536 => ERROR: error parsing response from server
```

O tecto é exactamente **65 535** (contagem de parâmetros é `int16`/`u16` no Bind do
protocolo estendido); em 65 536 o contador transborda e o driver falha. Logo o
tecto de linhas por `bulk_create` é `floor(65535 / colunas_por_linha)` (p.ex. 4
colunas → 16 383 linhas). Não é um erro do ORM que explique o limite — é falha de
protocolo crua. **Prova viva** (não por construção). **Severidade Alta confirmada.**

---

## Achados confirmados por leitura de código

### DESIGN-01 / H1 + H13 — estado global de processo é a fonte de dados — `confirmed`
`db.rs:74` `static POOL: OnceLock<Pool>`; `db.rs:75` `static READ_ONLY: AtomicBool`;
`db.rs:876` SQLite `static CONN: OnceLock<Arc<Mutex<rusqlite::Connection>>>` (conexão
única partilhada). `dsn()` (`db.rs:86-97`) só lê ambiente. Não há objecto de sessão
parametrizável. **Crítico confirmado.**

### DBA-02 / H4 — sem controlo de concorrência — `confirmed`
Grep por `FOR UPDATE|SKIP LOCKED|NOWAIT|SET TRANSACTION|ISOLATION|40001|40P01|is_serialization|is_deadlock|lock_timeout|idle_in_transaction`
em `dorm-core/src` e `dorm-migrations/src` devolve **apenas** um comentário de
documentação (`db.rs:855`) — nenhuma funcionalidade. `classify_db` (`error.rs:135-143`)
só distingue `57014`; `40001`/`40P01` caem em `Error::Db` genérico, logo sem retry
possível sem *string-match*. `BEGIN`/`BEGIN READ ONLY` literais (`db.rs:1326-1331`),
nunca `SET TRANSACTION ISOLATION LEVEL`. **Alto confirmado.**

### MIG-02 — renomeação tratada como drop+add (perda de dados silenciosa) — `confirmed`
`autodetect.rs:250-253` constrói os mapas de colunas **por nome** (`c.name`).
`autodetect.rs:290-321`: coluna nova (ausente em `prev_cols`) → `adds`
(`ADD COLUMN`); coluna antiga (ausente em `cur_cols`) → `col_drops`
(`DROP COLUMN`). Não há detecção de renomeação em lado nenhum (sem
`RenameColumn`/`@@renamed_from`). No SQLite, `sqlite_rebuild` (`autodetect.rs:346-351`)
só copia colunas partilhadas por nome — a renomeada não é copiada e desaparece no
`DROP TABLE`. Uma renomeação apaga a coluna e recria-a vazia, sem aviso. **Crítico
confirmado.**

### MIG-04 — rebuild de tabela SQLite: FK desligadas e janelas de perda — `confirmed`
`autodetect.rs:326-365` (`sqlite_rebuild`): o comentário assume `PRAGMA foreign_keys`
**sempre desligado** (`:332-335`) — pressuposto não verificado em runtime; colunas só
da nova forma tomam default/NULL, logo um `NOT NULL` sem default só é seguro em
tabela vazia (`:337-339`). Perda de dados por renomeação (ver MIG-02, caminho SQLite).
**Alto confirmado.**

### SEC-01 — POST/create da API gerada contorna `guard_write` — `confirmed`
`codegen_api.rs:171-176`: o handler `create_*` compila com `compiler::compile_insert`
e corre `dorm_core::model::run_query(&sql)`. `run_query` é `db::fetch` (`model.rs:158-160`),
e nem `fetch`/`query` nem `execute` chamam `guard_write` (`db.rs:642-705`, `768-799`).
O `guard_write` só é invocado pelos terminais tipados do `QuerySet` (`queryset.rs:133,
150,199,489,496,552,561,568`), por `model.rs:61,118` e por `db::sql_execute`
(`db.rs:824`). Como o `INSERT` da API usa `RETURNING`, viaja por `fetch` (caminho de
leitura) e **escapa ao guarda**. Os handlers `update_*`/`delete_*` gerados vão pelo
`QuerySet` (`.update(...)`/`.delete()`) e **são** barrados; o `create_*` não.
**Confirmado.**

> **Nota de severidade.** O achado é real e correcto, mas o seu gatilho é estreito:
> só se manifesta quando alguém chama `set_read_only(true)`, e o `serve` gerado
> nunca o activa por omissão. É uma quebra silenciosa de uma propriedade documentada
> (ADR 0005, provada pelo teste `raw_execute_is_refused_in_a_read_only_session`,
> `db.rs:1603`), pelo que **Alto é defensável**; quem quiser uma escala
> estritamente por impacto-em-produção-por-omissão poderia classificá-lo **Médio**.
> Recomendo manter Alto mas registar o gatilho.

### SEC-02 / H11 — API gerada sem auth/authz, mass-assignment, sem âmbito de tenant — `confirmed`
Grep por `auth|jwt|bearer|token|middleware|tower|layer` em `codegen_api.rs` →
**zero**. O router (`codegen_api.rs:242-257`) regista `/health`, `/openapi.json` e o
CRUD, sem camada de autenticação. O corpo de `create`/`update` é um
`serde_json::Map` dinâmico sem *allow-list* (`:168-170`, `:210`); `list_*`
(`:151-155`) e `get/update/delete` por PK não aplicam qualquer predicado de tenant.
**Alto confirmado.**

### SEC-03 / H15 — `READ_ONLY` é estado global de processo — `confirmed`
`db.rs:75` `static READ_ONLY: AtomicBool`; `set_read_only`/`is_read_only`/`guard_write`
(`db.rs:238-253`) operam sobre esse único átomo de processo. Não é por sessão/pedido;
um `set_read_only(true)` numa tarefa afecta todas. **Alto confirmado.**

### DESIGN-03 / GW-01 / H12 — `Plan`/`Sql` não serializáveis — `confirmed`
`compiler.rs:50` `#[derive(Debug, Clone)] pub struct Plan` e `compiler.rs:38`
`#[derive(Debug, Clone, PartialEq)] pub struct Sql` — **sem** `Serialize`. `Plan.model`
é `&'static ModelMeta` (`compiler.rs:51`), ponteiro para metadata de *link time*,
insterializável. O único `Serialize` em todo o `dorm-core` é `db.rs:195` (um enum não
relacionado). **Alto confirmado.**

### GW-03 / H11 + H12 — sem `tonic`/`prost`/`buf`, sem `dorm-server` — `confirmed`
Grep por `tonic|prost|buf` em todos os `crates/*/Cargo.toml` → **zero**. Os 10 crates
não incluem `dorm-server`. Sem caminho gRPC/IR/streaming. **Alto confirmado.**

### ADOPT-01 — sem crate-chapéu `dorm`, nome ocupado em crates.io — `confirmed`
`dorm-cli/Cargo.toml:26` em comentário: *"the name `dorm` is taken on crates.io"*. O
consumidor monta três crates à mão (`dorm-core`/`dorm-derive`/`dorm-migrations`),
cada um com linha de versão independente. Não existe crate-chapéu. **Confirmado.**

> **Nota de severidade.** É uma severidade de **adopção/DX e marca**, não técnica de
> correcção/segurança. "Alto" lê-se como "maior atrito de adopção", coerente com o
> eixo `adoption.md`; não é comparável, na mesma escala, a um DBA-01. Registo a
> distinção sem recomendar mudança numérica.

### ADOPT-02 — `dorm-derive` não depende de `dorm-core` (sem contrato SemVer) — `confirmed`
`dorm-derive/Cargo.toml` depende **só** de `syn`/`quote`/`proc-macro2` — nenhuma
dependência de `dorm-core`. O emparelhamento de versões é só um comentário, não
imposto pelo Cargo; uma combinação desalinhada resolve e só falha no código gerado
pela macro, com erro obscuro. **Confirmado** (mesma nota de severidade-de-adopção de
ADOPT-01).

---

## H16 — refutada — `refuted`

`compiler.rs:1065-1069` constrói uma **lista explícita** de colunas em ordem de
metadata (`meta.columns().map(quote)…join(", ")`) e emite
`INSERT … RETURNING {returning}` (`:1073`) — **não** `RETURNING *`. Os `ColumnSpec`
de descodificação vêm de `meta.fields` na mesma ordem (`:1078-1086`), pelo que a
ordem do `RETURNING` é sempre igual à ordem de descodificação posicional,
independentemente da disposição física da tabela. O cenário catastrófico de H16
(coluna nova desloca posições num `RETURNING *`) **não ocorre** no caminho compilado.
Só o **comentário** em `compiler.rs:1018` diz "RETURNING \*" — texto enganador, não
código. Confirma as conclusões independentes de MIG-07 e SEC-06 da Fase 1. O risco
posicional residual vive apenas no escape hatch `raw()` (H10/SEC-06), fora do âmbito
Crítico/Alto. **H16 = refuted.**

---

## Conclusão

- **15 achados Críticos/Altos confirmados**, 4 deles com **prova viva** (DBA-01,
  MIG-01, DBA-03/H2, PERF-02/H9) e os restantes por leitura de código com
  `ficheiro:linha`.
- **1 refutado:** H16 (o compilador emite lista `RETURNING` explícita, não `*`).
- **0 sem evidência.** Nenhum achado Crítico/Alto sai do relatório final por falta de
  prova.
- **Severidades:** todas as severidades técnicas propostas confirmam-se. Duas
  observações, sem recomendação forte de alteração: **SEC-01** (Alto defensável, mas
  de gatilho estreito — só sob `set_read_only(true)`, que o `serve` gerado nunca
  activa; argumentável Médio) e **ADOPT-01/ADOPT-02** (severidade de adopção/DX, não
  técnica — não comparável na mesma escala a um achado de correcção).
