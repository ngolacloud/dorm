# DORM — Relatório de revisão de ponta a ponta

> Revisão de design, escala, desempenho, segurança, migrações e adopção do DORM,
> com vista a um ORM poliglota de grau DBA. Narrativa em português europeu;
> identificadores em inglês. Data: 2026-10-31.
>
> Método: Fase 0 discovery (14 hipóteses) → Fase 1 revisão por 7 eixos →
> Fase 2 verificação independente (15 confirmados, 1 refutado, provas vivas em
> PostgreSQL 17 + PgBouncer). Detalhe por eixo em `findings/`; estado de cada
> achado em `VERIFICATION.md`.

---

## 1. Sumário executivo (1 página)

O DORM é, no seu núcleo, um **Data Mapper + Query Object bem desenhado**: dois
front-ends (tipado e por string) convergem num único `Plan` que compila para
`Sql { text, params }`, e os três invariantes de segurança do `ARCHITECTURE.md`
— **nenhum identificador vem do chamador, nenhum valor é formatado no SQL, o
compilador não faz I/O** — **mantêm-se verdadeiros no código** (verificado:
`registry::resolve`→`&'static`, `Binder.bind`→`$n`, compilador sem imports de
I/O). É um alicerce sólido e raro no ecossistema Rust, e a base correcta para a
ambição poliglota.

Sobre esse alicerce, porém, assentam **quatro defeitos críticos** e um conjunto
de lacunas que, hoje, impedem o DORM de cumprir as suas três ambições declaradas
(adoptável por projectos maduros; querido por DBAs em produção; opção poliglota).
Dois dos críticos são **bugs de correcção latentes**, provados ao vivo:

1. **`get_or_create`/`update_or_create` corrompem a transacção que os envolve**
   (DBA-01/H3). Dentro de `atomic()` — uma só conexão dedicada — um `INSERT` que
   perde a corrida dá `23505`, aborta a transacção, e o `SELECT` de recuperação
   devolve `25P02`. A idempotência prometida é **falsa dentro de transacção**:
   passa nos testes isolados, falha sob concorrência. *(Provado em PG 17.)*
2. **Toda a migração corre num único bloco transaccional** (MIG-01/H7): torna
   `CREATE INDEX CONCURRENTLY` impossível (`25001`) e faz qualquer `CREATE INDEX`
   ou `ALTER COLUMN … TYPE` segurar locks até ao `COMMIT` final — um
   *stop-the-world*, o oposto de zero-downtime. *(Provado: `ShareLock` a bloquear
   um `INSERT` concorrente; `CONCURRENTLY` em `BEGIN` → `25001`.)*
3. **Renomear uma coluna perde dados em silêncio** (MIG-02): o autodetect compara
   colunas só por nome, pelo que uma renomeação vira `DROP`+`ADD` (PostgreSQL) ou
   desaparece do rebuild (SQLite).
4. **O estado global de processo é a fonte de dados** (DESIGN-01/H1/H13): pool em
   `static OnceLock`, `READ_ONLY` em `static AtomicBool`, conexão SQLite única,
   DSN só de ambiente. Não há objecto de sessão/contexto — o que bloqueia
   multi-tenancy, routing primário/réplica e testabilidade paralela (o próprio
   ramo `fix/typed-e2e-readonly-flag-race` do dono é o sintoma).

Abaixo dos críticos, as lacunas agrupam-se em três histórias coerentes:

- **Concorrência e cache** (grau DBA): zero controlo de concorrência — sem
  isolamento configurável, sem retry de `40001`/`40P01`, sem `FOR UPDATE`/
  `SKIP LOCKED`, sem `SET LOCAL` de timeouts/GUCs, sem optimistic locking
  (DBA-02/H4); e `prepare_cached` incondicional, sem tecto e **incompatível com
  PgBouncer em modo *transaction*** (`26000 prepared statement does not exist`,
  provado), agravado pelo `IN` expandido e pelo `bulk_create` sem *chunking*
  (que estoura os 65 535 parâmetros — `65 536` falha, provado com o próprio
  `tokio-postgres`). *(DBA-03/PERF-01/PERF-02/H2/H9.)*
- **Objectos de base de dados e tipos**: DDL estreito (só tabelas, colunas e
  índice B-tree de uma coluna; sem views, funções, triggers, RLS, partições,
  sequências, `COMMENT`, `EXTENSION`, grants, nem índices avançados/CONCURRENTLY
  — MIG-03/H6); e apenas 8 tipos escalares (faltam `f64`, `date`/`time`,
  `bytea`, arrays, enums, ranges… — DBA-04/H5).
- **Poliglotismo e produção**: o `Plan` é o IR certo mas **não é serializável**
  (`&'static ModelMeta`, sem `derive(Serialize)` — GW-01/DESIGN-03/H12); não há
  `tonic`/`prost` nem `dorm-server` (GW-03); e a API Axum gerada não é de produção
  — OpenAPI à mão, sem auth, `limit` sem tecto, corpo dinâmico, e um caminho de
  escrita que **contorna o guarda `READ_ONLY`** (SEC-01/SEC-02/GW-04/H11).

**Honestidade de capacidades (um ponto forte a preservar):** os ADRs, o
`cli-contract.md` e a regra "nenhum comando vazio" mostram um projecto que já
documenta o que *não* faz. A maioria das lacunas acima já está nomeada como
follow-up nos ADRs 0004/0007. A revisão não contradiz essa honestidade — dá-lhe
prioridades, evidência e um roteiro.

**Veredicto:** núcleo sólido, bordas por endurecer. O caminho poliglota **não
exige reescrever o núcleo** — exige dar-lhe uma pele externa (IR + descritor +
Data Plane) sem mover para o cliente nenhuma das três decisões de segurança que o
compilador hoje garante. A ordem certa é **corrigir primeiro** (os quatro
críticos e o *chunking*), **endurecer a seguir** (concorrência, cache, tipos,
objectos de BD, migrações zero-downtime) e **externalizar por fim** (IR → Data
Plane → SDKs).

---

## 2. Achados por severidade

Deduplicados entre eixos (ex.: DBA-03≡PERF-01 em H2; DESIGN-03≡GW-01 em H12).
Estado de verificação em `VERIFICATION.md`. ✓viva = prova viva corrida.

### Críticos (4)

| ID | Achado | Hipótese | Evidência | Estado |
|---|---|---|---|---|
| DBA-01 | `get_or_create`/`update_or_create` → `25P02` dentro de `atomic()` | H3 | `queryset.rs:165-183`, `db.rs:1324-1331` | ✓viva |
| MIG-01 | Migração em tx única: bloqueio de escritas + `CONCURRENTLY` impossível | H7 | `migrator.rs:77` | ✓viva |
| MIG-02 | Renomeação de coluna = perda de dados silenciosa (drop+add) | — | `autodetect.rs:250-320` | confirmado |
| DESIGN-01 | Estado global de processo é a fonte de dados (pool/READ_ONLY/SQLite) | H1,H13 | `db.rs:74,75,876` | confirmado |

### Altos (14, deduplicados)

| ID | Achado | Hipótese | Evidência | Estado |
|---|---|---|---|---|
| DBA-02 | Zero controlo de concorrência (isolamento/retry/row-locks/SET LOCAL/optimistic) | H4 | `db.rs:1326-1331`, `error.rs:135-143` | confirmado |
| DBA-03 | `prepare_cached` incondicional; quebra PgBouncer txn mode; cache sem tecto | H2 | `db.rs:665-703` | ✓viva (`26000`) |
| PERF-02 | `bulk_create` sem *chunking* → estoura 65 535 parâmetros | H9 | `queryset.rs:149-158` | ✓viva (65 536 falha) |
| MIG-03 | DDL estreito: sem views/funções/triggers/RLS/partições/índices avançados | H6 | `ddl.rs:208,271` | confirmado |
| MIG-04 | Rebuild SQLite com FK desligada corrompe em renomeação/`NOT NULL` novo | — | `autodetect.rs` | confirmado |
| DESIGN-02 | Sem Unit of Work / Identity Map (N+1, lost-update) | — | `queryset.rs` | confirmado |
| DESIGN-03 / GW-01 | `Plan`/`Sql` não serializáveis (`&'static ModelMeta`) | H12 | `compiler.rs:39,50` | confirmado |
| GW-02 | `ModelMeta` não publicável como descritor em runtime | H12 | `meta.rs:99-160` | confirmado |
| GW-03 | Sem `tonic`/`prost`/`dorm-server`: sem gRPC, streaming, tx remotas | H11,H12 | `Cargo.toml` (grep) | confirmado |
| GW-04 | API Axum não-produção: OpenAPI à mão, sem auth, sem tecto, corpo dinâmico | H11 | `codegen_api.rs:153,280` | confirmado |
| SEC-01 | Create da API gerada contorna `guard_write` (escreve em modo só-leitura) | — | `codegen_api.rs:173` | confirmado |
| SEC-02 | API gerada sem auth/authz; mass-assignment; sem âmbito de tenant | H11 | `codegen_api.rs:242-277` | confirmado |
| SEC-03 | `READ_ONLY` global de processo (um pedido desliga escritas de todos) | H15 | `db.rs:75` | confirmado |
| ADOPT-01/02 | Sem crate-chapéu `dorm` (nome ocupado); `dorm-derive` sem SemVer vs core | — | `Cargo.toml` | confirmado |

### Médios (10) e Baixos (6)

Médios: DBA-04/H5 (tipos estreitos), PERF-03 (`IN` expandido vs `= ANY`),
PERF-04 (sem prefetch, `m2m.add` 2 round-trips), PERF-05 (alocações por célula),
MIG-05 (drift só aditivo), MIG-06 (ledger sem checksum/`down`), SEC-04 (sem
RLS/GUC por tenant), SEC-05 (`limit` sem tecto — DoS), GW-05 (sem objectos
remotos/allow-list), GW-06 (sem SDKs/`buf`), ADOPT-05 (tecto de capacidades).
Baixos: DESIGN-06, PERF-06 (§3.3 sub-evidenciada), PERF-07 (só offset),
SEC-06 (risco posicional só no `raw()`), SEC-07 (TLS sem verificação, opt-in),
MIG-07 (comentário `RETURNING *` desactualizado — **H16 refutada**).

---

## 3. Mapa de calor por eixo

| Eixo | Crít. | Alto | Méd. | Baixo | Leitura |
|---|:--:|:--:|:--:|:--:|---|
| Design (Fowler) | 1 | 2 | 2 | 1 | Bom Data Mapper/Query Object; falta UoW/Identity Map; estado global é o nó górdio |
| DBA (SQL/concorrência/tipos) | 1 | 2 | 1 | 0 | Correcção (H3) + ausência total de concorrência — o mais grave para produção |
| Migrações | 2 | 2 | 2 | 1 | **O eixo mais frágil**: perda de dados e bloqueio; longe de zero-downtime |
| Desempenho | 0 | 1 | 3 | 2 | Cache sem tecto + `IN`/`bulk` sem chunking; §3.3 por medir |
| Segurança | 0 | 3 | 2 | 2 | Invariante nuclear **intacto**; superfície da API gerada é o risco |
| Gateway poliglota | 0 | 4 | 2 | 0 | Tudo por construir, mas o `Plan` é a fundação certa |
| Adopção | 0 | 1 | 1 | 0 | Facade em falta + versionamento; tecto de capacidades limita o público |

**Concentração do risco:** migrações (perda de dados + bloqueio) e DBA
(correcção + concorrência) são onde um projecto de produção se magoa primeiro.
O gateway é esforço, não risco — assenta num núcleo que a revisão considera são.

---

## 4. Riscos de adopção

1. **Tecto de capacidades (ADOPT-05):** público realista hoje é "PostgreSQL
   modesto + equipa Rust". Um projecto maduro bate em H4 (concorrência), H5
   (tipos), H6 (objectos de BD) e H1 (multi-tenancy) cedo.
2. **Facade e versionamento (ADOPT-01/02):** montar 3 crates à mão, com linhas de
   versão independentes e um `dorm-derive` que não depende de `dorm-core`, é
   fricção e risco de combinações que só falham a compilar o código gerado.
3. **Confiança de produção (DBA-01, MIG-01/02):** um DBA que descubra a perda de
   dados em renomeação ou o *stop-the-world* da migração perde a confiança
   depressa — e estes são precisamente os utilizadores que o produto quer atrair
   como contribuidores.
4. **Mitigantes fortes:** ADRs como registo de decisões, honestidade de
   capacidades, `dorm-example` como prova executável de independência, e o
   ecossistema (schema declarativo + CLI Prisma-style + REPL + Studio) que nenhum
   par Rust oferece. É um produto com narrativa — falta-lhe o endurecimento.

---

## 5. Para onde ir

A ordem no `ROADMAP_BRICKS.md` segue **valor/risco**: correcção → concorrência →
conexões/sessão → tipos → consulta → objectos de BD → migrações zero-downtime →
IR → Data Plane. A matriz de capacidades de grau DBA está em
`CAPABILITY_MATRIX.md`; a arquitectura poliglota-alvo, com a preservação explícita
dos invariantes, em `TARGET_ARCHITECTURE.md`; as decisões em `docs/adr/0010`+.
