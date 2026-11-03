# 00 — Discovery (Fase 0)

> Revisão de ponta a ponta do DORM e plano para um ORM poliglota de grau DBA.
> Este documento é o **portão bloqueante**: nenhuma alteração de código antes da
> sua aprovação. Narrativa em português europeu; identificadores em inglês.
>
> Data: 2026-10-31 · Orquestrador: sessão principal · Sem alterações de código.

---

## 1. Inventário

### 1.1 Workspace

Repositório `ngolacloud/dorm`, 10 crates, **20 610 linhas de Rust**, versão de
workspace `0.2.0`, edição 2021, `rust-version = 1.75` (toolchain do host:
`rustc 1.96.0`). Dual-licenciado MIT OR Apache-2.0 (ADR 0073).

| Crate | LoC | Versão publicada | Papel |
|---|---:|---|---|
| `dorm-core` | 7 239 | 0.6.0 | compilador, `db`, queryset, value, registry — o núcleo |
| `dorm-cli` | 3 508 | 0.13.0 | CLI instalável (`cargo install dorm-cli`), binário `dorm` |
| `dorm-migrations` | 2 498 | 0.7.1 | autodetecção, DDL, ledger, advisory lock |
| `dorm-models` | 2 410 | — (interno) | modelos de exemplo + testes de snapshot/paridade |
| `dorm-schema` | 1 861 | 0.3.1 | parser `dorm.schema` + codegen (inclui `codegen_api`) |
| `dorm-derive` | 1 087 | 0.3.0 | `#[derive(Model)]` |
| `dorm-shell` | 998 | — | REPL dinâmico, binário `dorm` (ver H14) |
| `dorm-studio` | 661 | 0.1.1 | UI web estilo Prisma Studio |
| `dorm-sqlite` | 272 | 0.2.1 | executor SQLite (ADR 0002) |
| `dorm-example` | 76 | — | blogue ~40 linhas, prova de independência |

### 1.2 ADRs

Existem **0001–0009** em `docs/adr/`. **Correcção à premissa do prompt:** o ADR
0009 (`accounts-admin`) já está ocupado; os ADRs propostos nesta campanha devem
começar em **0010**, não em 0009.

### 1.3 Dependências estruturantes

`tokio-postgres 0.7` + `deadpool-postgres 0.14` (PG); `rusqlite` atrás de feature
(SQLite); `rust_decimal`, `chrono`, `uuid`, `serde_json` (tipos); `inventory 0.3`
(registo por linker); `tracing` (observabilidade, ADR 0008); TLS `rustls`/`openssl`
feature-gated (ADR 0004); `clap`, `insta`. **Ausentes:** `tonic`/`prost`/`buf`
(sem caminho gRPC/IR — ver H12).

---

## 2. Linha de base de build e testes

- `make check` (fmt `--check`, clippy `-D warnings`, `cargo test --all-targets`):
  **verde**. Corrida incremental (árvore já compilada) rápida; o tempo de build
  limpo fica por medir numa máquina com disco folgado.
- Os testes *live* estão `#[ignore]`/condicionados a DSN (`DELONIX_ORM_DSN`,
  `DELONIX_ORM_TEST_DSN`), por isso **não** correm no `make check`. Suites live
  existentes: `migrate_live`, `typed_e2e`, `m2m_e2e`, `sqlite_e2e`, `sync_live`,
  `observability_live`, `prepared_cache_live`, `statement_timeout_live`,
  `interactive_tx_sqlite`.
- Snapshots SQL (`sql_snapshots.rs`) e paridade de esquema (`schema_parity.rs`)
  são o contrato a manter verde (restrição 3).
- Benchmark existente: `crates/dorm-core/benches/prepared_cache.rs`
  (`bench_select`, `bench_insert`).
- Estado git: ramo `fix/typed-e2e-readonly-flag-race` (trabalho do dono,
  não relacionado); árvore limpa exceto `.claude/` por versionar.

---

## 3. Ambiente de medição (disponível vs em falta)

| Recurso | Estado | Nota |
|---|---|---|
| PostgreSQL 16.15 | **disponível, a correr** | socket `/var/run/postgresql`; conecta e cria BD descartável |
| `pg_stat_statements` | **disponível** (extensão) | por activar na BD de teste |
| `docker` / `podman` | **presentes** | permite PG 17 e PgBouncer em container |
| `psql` | presente | cliente 16 |
| PostgreSQL 17 | **em falta** nativo | obter por container (H2, matriz de versões) |
| PgBouncer | **em falta** (sem binário) | **necessário para H2** (modo *transaction*) → container |
| `sqlite3` CLI | em falta | irrelevante: DORM usa `rusqlite` embebido |
| Disco | **90% (90G livres)** | vigiar em benchmarks/containers (armadilha histórica) |

Conclusão: o ambiente é mais capaz do que o esperado. PG 16 chega para a maioria
da triagem; **PgBouncer e PG 17 exigem containers** (Fase 1/3).

---

## 4. Triagem das hipóteses H1–H14

Metodologia: leitura do código com evidência `ficheiro:linha`. "Confirmado"
significa que o código faz o que a hipótese descreve; a **reprodução por teste
live** (corrida concorrente, 25P02, limite de parâmetros) fica para a Fase 1/2.

| # | Veredicto | Evidência | Nota |
|---|---|---|---|
| **H1** Pool global único | **Confirmado** | `db.rs:74` `static POOL: OnceLock<Pool>`; `dsn()` só de ambiente `db.rs:86` | Sem multi-base, réplicas, routing por tenant, pool por teste |
| **H2** `prepare_cached` incondicional | **Confirmado** | `db.rs:665,669,699,703` | Sem limite; incompatível com PgBouncer *transaction*; `IN` de tamanho variável gera texto distinto (ver H8) → cache ilimitada; invalidação pós-`ALTER` por provar |
| **H3** `get_or_create`/`update_or_create` em tx abortada | **Confirmado (latente)** | `queryset.rs:165-183`, `208-213` | SELECT→INSERT→SELECT sem `SAVEPOINT`/`ON CONFLICT`. Isolado funciona; **dentro de `atomic()`** o unique_violation aborta a tx (25P02) e o SELECT de recuperação falha |
| **H4** Sem controlo de concorrência | **Confirmado** | ausência em `db.rs`/`compiler.rs` | Sem isolamento configurável, sem retry 40001/40P01, sem `FOR UPDATE`/`SKIP LOCKED`/`NOWAIT`, sem `lock_timeout`/`idle_in_transaction`, sem optimistic locking |
| **H5** Conjunto de tipos estreito | **Confirmado** | `value.rs:27-35` (FieldType), `85-94` (Value) | Só I32/I64/Text/Bool/Decimal/Timestamp/Uuid/Json. Faltam f32/f64, DATE/TIME/INTERVAL, BYTEA, arrays, INET/CIDR, enums, ranges, tsvector, domínios, extensões |
| **H6** Objectos de BD não geridos | **Confirmado** | `ddl.rs:209` (índice B-tree de 1 coluna), `ddl.rs` (funções limitadas) | Sem views/MV, funções, procedures, triggers, RLS, partições, sequências, COMMENT, EXTENSION, grants; índices sem CONCURRENTLY/parcial/expressão/INCLUDE/GIN/GiST/BRIN |
| **H7** Migrações num só bloco transaccional | **Confirmado** | `migrator.rs:77` `db::atomic(...)` | Impede `CREATE INDEX CONCURRENTLY`; há rebuild de tabela (`{t}__dorm_new` `autodetect.rs:362`) para mudança de tipo/PK/FK, mas sem rename semântico nem backfill em lotes nem expand/contract |
| **H8** Lacunas de consulta | **Confirmado** | `compiler.rs:333` (`IN` expandido, não `= ANY($1)`) | Sem prefetch_related, Exists/OuterRef/subquery, CTE, janela, LATERAL, DISTINCT ON, UNION, upsert ON CONFLICT, RETURNING selectivo (só `RETURNING *` `compiler.rs:1018`), keyset, streaming, COPY |
| **H9** `bulk_create` sem chunking | **Confirmado** | `queryset.rs:149-158` | Um só `INSERT … VALUES (…),(…)` com todas as linhas → estoura 65 535 parâmetros; cada N distinto polui a cache (H2) |
| **H10** `raw()`/`from_row` por posição | **Confirmado** | `model.rs:27-29`, `row.rs:46` `try_get::<usize>(idx)` | Contrato de colunas por ordem; `RETURNING *`/`SELECT *` após migração pode mapear campos errados em silêncio |
| **H11** API gerada não é de produção | **Confirmado** | `codegen_api.rs:280` (OpenAPI por `json!`), `:153` (`limit.unwrap_or(50)` sem tecto) | Sem auth/authz, paginação por offset, OpenAPI à mão, sem gRPC, sem transacções remotas |
| **H12** Poliglotismo bloqueado | **Confirmado** | `compiler.rs:40,51` `Plan`/`Sql` **sem** `derive(Serialize)`; sem `tonic`/`prost`; sem crate `dorm-server` | Modelos só em tempo de compilação Rust; falta IR serializável/versionado |
| **H13** Estado global de processo | **Confirmado** | `db.rs:75` `static READ_ONLY: AtomicBool`; `db.rs:876` SQLite `Arc<Mutex<Connection>>` | `READ_ONLY` não é por sessão/pedido; SQLite conexão única |
| **H14** Colisão de binários | **Confirmado** | `dorm-cli/Cargo.toml:28` e `dorm-shell/Cargo.toml:16` ambos `name = "dorm"` | `cargo install` de ambos colide |

**14/14 confirmadas por leitura de código.** Nenhuma refutada. As que precisam de
*reprodução viva* para subir de "confirmado por código" a "confirmado por teste"
são **H2** (PgBouncer, `ALTER` com statement em cache), **H3** (corrida concorrente
→ 25P02) e **H9** (70 000 linhas → erro de protocolo).

### 4.1 Hipóteses novas (H15+)

- **H15 — `READ_ONLY` global quebra isolamento de tarefas.** `guard_write` lê o
  `AtomicBool` global (`db.rs:75`, `:239-244`); `set_read_only(true)` numa tarefa
  bloqueia escritas em **todas** as tarefas do processo. Além de impedir routing
  réplica/primário (H1), é um risco de correcção em código concorrente. Elevar de
  dentro de H13. *(Evidência: o próprio ramo actual `fix/...readonly-flag-race`
  corrige uma fuga desta flag entre testes e2e paralelos — sintoma real.)*
- **H16 — `INSERT … RETURNING *` + descodificação posicional.** `create`/`bulk_create`
  compilam sempre `RETURNING *` (`compiler.rs:1018,1073`) e descodificam por
  posição (H10). Uma coluna acrescentada por migração altera posições e pode
  mapear campos errados sem erro. Interage H8 (falta `RETURNING` selectivo) + H10.
- **H17 — Kit de agentes/skills ausente (processo).** O prompt pressupõe
  `.claude/agents/dorm-*` e `.claude/skills/dorm-review-method|dorm-dba-capabilities|…`.
  **Não existem** no repositório (só `.claude/sessions/`). A Fase 1 como escrita
  (agentes especializados por nome) não é executável sem (a) construir o kit ou
  (b) usar agentes genéricos (`general-purpose`/`Explore`/`Plan`) a desempenhar
  os papéis. Decisão necessária no portão.

---

## 5. Plano de trabalho dos agentes (Fase 1)

Dado **H17**, proponho mapear cada eixo a um agente genérico paralelo, com o
ficheiro de saída e os ficheiros a ler já fixados:

| Eixo → `docs/review/findings/<eixo>.md` | Agente | Hipóteses | Ficheiros a ler |
|---|---|---|---|
| `design.md` (padrões Fowler) | general-purpose | H1,H10,H12,H13 | `compiler.rs`, `registry.rs`, `queryset.rs`, `model.rs`, `ARCHITECTURE.md` |
| `dba.md` (correcção SQL, concorrência, tipos) | general-purpose | H2,H3,H4,H5 | `db.rs`, `queryset.rs`, `value.rs`, `compiler.rs` |
| `migrations.md` | general-purpose | H6,H7,H16 | `dorm-migrations/src/{ddl,autodetect,migrator,history}.rs`, `dorm-schema/src/parser.rs` |
| `performance.md` (benchmarks) | general-purpose | H2,H8,H9 | `db.rs`, `queryset.rs`, `benches/prepared_cache.rs` + bench novo em scratch |
| `security.md` | general-purpose | H10,H11,H15 | `codegen_api.rs`, `db.rs`, `error.rs`, ADR 0005 |
| `gateway.md` (poliglota) | general-purpose | H11,H12 | `compiler.rs` (`Plan`/`Sql`), `registry.rs`, `codegen_api.rs`, `meta.rs` |
| `adoption.md` (DX, mercado) | general-purpose | H14 | `Cargo.toml`s, `README.md`, `CONTRIBUTING.md`, `cli-contract.md` |
| verificação independente | general-purpose (sem ter revisto) | Críticos/Altos | reproduz código + teste/medição |

Medições que exigem containers (Fase 1/3): PG 17 e PgBouncer *transaction mode*
(H2), carga de 70 000 linhas (H9), corrida concorrente para 25P02 (H3),
`criterion` comparativo (`tokio-postgres` cru vs `Manager::raw` vs tipado; e vs
`sqlx`/`diesel-async`/`sea-orm`).

---

## 6. Decisões pendentes para o portão

1. **Kit de agentes (H17):** construir `.claude/agents` + `.claude/skills`
   conforme o prompt, **ou** avançar com agentes genéricos a desempenhar os
   papéis? (Recomendo genéricos: entrega a revisão sem um sub-projecto de
   tooling à frente.)
2. **Numeração de ADR:** confirmar início em **0010** (0009 ocupado).
3. **Ramo de trabalho:** o ramo actual é do dono. Criar `review/dorm-e2e` para
   os deliverables D1–D8 (docs apenas, sem código)?
4. **Containers:** autorizar subida de PG 17 + PgBouncer em container para as
   provas vivas de H2/H3/H9 (disco a 90% — um container PG é aceitável)?
5. **Âmbito desta campanha:** a Fase 0–3 (revisão + matriz + arquitectura-alvo +
   ADRs propostos + roteiro) **sem** implementação; a Fase 4 (tijolos) abre
   depois, PR a PR, com aprovação do dono?

---

## 7. Veredicto da Fase 0

As 14 hipóteses estão **confirmadas por evidência de código**, mais 3 novas
(H15–H17). A linha de base está verde. O ambiente suporta a maior parte da prova
viva, faltando apenas PgBouncer/PG 17 por container. O caminho PostgreSQL provado
(snapshots + paridade) está intacto e não será tocado sem intenção deliberada.

**PARAR. Aguardar aprovação explícita antes da Fase 1.**
