# Eixo DESIGN E PADRÕES — achados (nível Fowler, PoEAA)

> Revisão só de leitura. Hipóteses atribuídas: **H1** (pool global único),
> **H10** (descodificação posicional), **H12** (modelos só em tempo de
> compilação; `Plan` não serializável → poliglotismo bloqueado), **H13** (estado
> global de processo). Narrativa em português europeu (ortografia pré-1990);
> identificadores em inglês.
>
> Ficheiros lidos: `crates/dorm-core/src/{compiler,registry,queryset,model,meta,lib,db,row}.rs`
> e `ARCHITECTURE.md`.

---

## Veredicto rápido sobre os padrões de Fowler

| Padrão PoEAA | Implementado? | Onde / qualidade |
|---|---|---|
| **Query Object** | **Sim, exemplar** | `Plan` (`compiler.rs:51`) é dado puro; `compiler` compila-o. É o coração do desenho. |
| **Data Mapper** | **Sim, bem** | `Model`/`NewRow` (`model.rs:21,54`) + `ModelMeta` (`meta.rs:100`); o mapeamento vive fora da struct de domínio. |
| **Registry + Metadata Mapping** | **Sim, bem** | `registry` (`registry.rs`) resolve strings contra `&'static` metadata; `inventory::collect!` (`meta.rs:160`). |
| **Plugin / registo por link** | **Parcial** | `inventory` regista modelos no binário em *link time* — não é plugin em tempo de execução (ver H12). |
| **Repository** | **Parcial / fino** | `Manager<T>` (`queryset.rs:25`) é um *finder*, mas expõe o `QuerySet` em vez de esconder o mecanismo de consulta; sem abstracção da fonte de dados. |
| **Separated Interface / Gateway** | **Meio feito** | `Dialect` é um `enum` (`compiler.rs:133`), não um *trait*; o despacho de backend é um `match` fixo em `db.rs`. |
| **Unit of Work** | **Não** | `atomic` dá âmbito transaccional (task-local), mas sem *change tracking*, sem ordenação/coalescência de escritas. |
| **Identity Map** | **Não** | Cada *fetch* constrói structs novas; `get_or_create` relê a BD; sem deduplicação por chave. |
| **Lazy Load** | **Só a consulta** | `QuerySet` é *lazy* na construção da consulta; relações não têm *lazy load* (proxy/ghost); reversos exigem nova consulta manual. |

O núcleo tipado (Query Object + Data Mapper + Registry + pureza do compilador) é
**de manual e está bem imposto**. Os achados abaixo são sobretudo sobre o que
*falta* para o DORM ser um ORM completo (Fowler) e um motor poliglota, não sobre
defeitos no que existe.

---

## [DESIGN-01] Estado global de processo é a fonte de dados — sem sessão/contexto — Crítico
- **Hipótese:** H1 (**confirmada**) + H13 (**confirmada**).
- **Evidência:**
  - `db.rs:74` `static POOL: OnceLock<Pool>` — um só pool por processo; `dsn()` só lê ambiente (`db.rs:86-97`).
  - `db.rs:75` `static READ_ONLY: AtomicBool` — flag de processo, não por sessão/pedido; `guard_write` lê-a global (`db.rs:248`).
  - `db.rs:876` SQLite `static CONN: OnceLock<Arc<Mutex<rusqlite::Connection>>>` — conexão única partilhada.
  - `db.rs:755` `dialect()` deriva o backend do DSN de ambiente a cada chamada dos terminais do `QuerySet`.
- **Impacto:** não há objecto de *sessão*/*contexto* parametrizável. Compromete
  (a) **multi-tenancy** — impossível rotear por tenant, por primário/réplica ou
  por base; (b) **testabilidade** — testes paralelos partilham pool e `READ_ONLY`
  (o próprio ramo `fix/typed-e2e-readonly-flag-race` do dono é o sintoma); (c)
  **Repository** verdadeiro — `Manager<T>` não pode abstrair a fonte porque a
  fonte é um singleton. É o obstáculo de desenho mais sério para os objectivos de
  control-plane/grau-DBA.
- **Recomendação:** introduzir um `Session`/`Db` handle explícito (pool + dialecto
  + flag read-only + tenant) passado aos terminais, com os `static` como
  *fallback* de compatibilidade. Mínimo imediato: tornar `READ_ONLY` um
  task-local (como `CURRENT_TX` já é, `db.rs:614`) em vez de `AtomicBool` global.
- **Esforço:** L (sessão completa) / S (só a flag read-only→task-local).
- **ADR/Tijolo:** ADR 0010 "Sessão explícita e roteamento de fonte de dados";
  tijolo `read-only-per-task` como primeiro passo.

## [DESIGN-02] Nem Unit of Work nem Identity Map — é mapper + query-builder, não um ORM completo — Alto
- **Hipótese:** nova (**DESIGN**, decorre de H1/H13).
- **Evidência:**
  - Sem *change tracking*/*identity map* em todo o `dorm-core` (procura por `identity`/`dirty`/`unit_of_work` só devolve comentários).
  - `queryset.rs:165-183` `get_or_create`: `SELECT` → `INSERT` → (em corrida) novo `SELECT`; nunca reusa um objecto em memória.
  - `model.rs:60-114` `save_instance` escreve imediatamente, uma `INSERT`/`UPDATE` por chamada.
  - `queryset.rs:366-395` `all()`: cada linha vira uma struct nova via `from_row`; duas leituras da mesma PK devolvem objectos distintos e desligados.
  - `atomic` (`db.rs:1258`) dá só âmbito transaccional (task-local + savepoints), sem coalescência nem ordenação de escritas.
- **Impacto:** sem deduplicação por identidade (risco de *lost update* e de
  objectos incoerentes), sem *write batching*, e cada `QuerySet` vai à BD — o
  caminho directo para N+1 e para round-trips redundantes (`get_or_create` faz 2-3
  idas). Fowler trataria estes dois padrões como o núcleo de um ORM de mapeamento;
  a sua ausência define o DORM como *Data Mapper + Query Object*, não como ORM
  completo. É uma decisão de identidade do produto, não só uma optimização.
- **Recomendação:** decidir explicitamente o âmbito (um "Unit of Work" leve ligado
  ao `atomic` que mantenha um Identity Map por transacção resolve N+1 e lost-update
  sem reescrever a API). Mínimo: documentar a decisão e oferecer um Identity Map
  opcional por âmbito de `atomic`.
- **Esforço:** L.
- **ADR/Tijolo:** ADR 0011 "Unit of Work e Identity Map por âmbito transaccional".

## [DESIGN-03] `Plan`/`Sql` não serializáveis — o Query Object não pode ser o IR poliglota — Alto
- **Hipótese:** H12 (**confirmada**).
- **Evidência:**
  - `compiler.rs:50-74` `Plan` **sem** `derive(Serialize/Deserialize)`; contém `model: &'static ModelMeta` — um ponteiro para metadata de *link time*, insterializável por natureza.
  - `compiler.rs:38-44` `Sql` idem (sem `Serialize`).
  - Os dois front-ends convergem **limpo** num único `Plan`: typed via métodos do `QuerySet` (`queryset.rs:294-358`), string via `Q::new`/`filter_str` (`queryset.rs:302`) e `fetch_value_rows(&Plan)` (`queryset.rs:533`); mesma compilação `compile_select_on`. A convergência existe e é testada por snapshot — o `Plan` *é* o candidato natural a IR.
- **Impacto:** o Query Object vive só dentro de um processo Rust compilado com os
  modelos. Não atravessa fronteira de processo → sem gateway gRPC/HTTP, sem
  clientes noutras linguagens, sem plano enviado a um servidor (os objectivos de
  poliglotismo). Modelos existem só em tempo de compilação.
- **Recomendação:** para o `Plan` virar IR falta: (a) `derive(Serialize)` em
  `Plan`/`Q`/`F`/`OrderBy`/`Aggregate`/`Value`/`FieldType`; (b) substituir
  `&'static ModelMeta` por um `model: String` (nome) resolvido pela `registry` no
  lado que compila; (c) um registo alcançável no servidor (já existe via
  `inventory`, `registry.rs:14`) e versionamento do IR. O compilador já é puro e
  sem I/O, pelo que serve de "servidor" sem mudança.
- **Esforço:** M (serialização + desreferência do modelo por nome) / L (gateway).
- **ADR/Tijolo:** ADR 0012 "IR serializável e versionado do `Plan`"; tijolo
  `plan-model-by-name` como pré-requisito.

## [DESIGN-04] Dialecto como `enum` e despacho de backend fixo — Separated Interface meio feita — Médio
- **Hipótese:** nova (**DESIGN**).
- **Evidência:**
  - `compiler.rs:132-147` `enum Dialect { Postgres, Sqlite }`; diferenças espalhadas por `match self` (`bind` `:165-171`, `ilike` `:141`, `now()` `:904-907`).
  - `db.rs:768-779` `fetch` faz `match backend()` e cada caminho é código à parte; `dialect()` (`db.rs:755`) escolhe por string de DSN.
- **Impacto:** adicionar um terceiro dialecto (o objectivo "grau DBA" multi-base)
  obriga a editar `match`es em todo o compilador e em `db.rs` — viola
  Open/Closed. A Separated Interface só está feita na pureza do compilador, não na
  extensibilidade de backends; cada novo backend é cirurgia no núcleo.
- **Recomendação:** extrair um *trait* `Dialect` (placeholder, operador
  case-insensitive, `now()`, quoting) e um *trait* `Executor`/`Gateway` para o
  despacho de `fetch`/`run`, deixando PG/SQLite como implementações. Não urge
  enquanto só há dois backends, mas é pré-requisito do poliglotismo.
- **Esforço:** M.
- **ADR/Tijolo:** ADR 0013 "Dialecto e executor como *traits* (Separated Interface)".

## [DESIGN-05] Descodificação posicional acopla o resultado à ordem de compilação — Médio
- **Hipótese:** H10 (**confirmada**).
- **Evidência:**
  - `row.rs:46` `pub fn try_get<T: Decode>(&self, idx: usize)` — só por índice; não há acessor por nome.
  - `model.rs:29` `from_row(row, offset)` lê por posição; `select_related` desloca por `offset` (`queryset.rs:379-391`).
  - `compiler.rs:1065-1069` `INSERT … RETURNING` usa `meta.columns()` (ordem de metadata) e descodifica por `meta.fields` na mesma ordem.
  - `queryset.rs:125-128` `raw()` mapeia `from_row(row, 0)` — **depende de o SQL escrito à mão devolver as colunas na ordem dos campos do modelo**.
- **Impacto:** dentro do SQL gerado pelo DORM o risco é **mitigado por desenho** —
  o compilador nunca emite `SELECT *`, nomeia as colunas na ordem de `meta.fields`
  (`compiler.rs:530-540`, nota em `meta.rs:147-155`), logo ordem-do-SELECT ==
  ordem-de-metadata == ordem-de-`from_row`. O ponto frágil é a fronteira `raw()`:
  uma coluna acrescentada por migração, ou um `SELECT` à mão fora de ordem, mapeia
  campos errados **em silêncio** (interage com H16). A descodificação por nome
  (como `decode()` já faz para `ValueRow`, `queryset.rs:582-583`) eliminaria a
  fragilidade ao custo de um lookup por coluna.
- **Recomendação:** oferecer um `from_row` por nome (ou validar o descritor de
  colunas do servidor contra `meta` em `raw()`), mantendo o posicional como via
  rápida interna onde a ordem é garantida pelo compilador.
- **Esforço:** M.
- **ADR/Tijolo:** ADR 0014 "Descodificação por nome na fronteira `raw()`".

## [DESIGN-06] Lazy Load de relações ausente (só a consulta é *lazy*) — Baixo
- **Hipótese:** nova (**DESIGN**).
- **Evidência:**
  - `queryset.rs:1-7,294-358` — `QuerySet` é *lazy* e imutável na **construção** da consulta (bom), mas isso é avaliação diferida do Query Object, não o Lazy Load de Fowler.
  - `select_related` é **eager** e só por FK directa (`compiler.rs:541-570`); não há `prefetch_related` (H8).
  - Reversos (`related_name`) resolvem-se em `filter`/`annotate` (`registry.rs:298`, `compiler.rs:473`), mas não há acessor que os carregue preguiçosamente a partir de uma instância — exige nova consulta manual.
- **Impacto:** navegar relações a partir de um objecto carregado obriga o chamador
  a escrever a consulta, ou arrisca N+1 se o fizer em laço. Sem proxy/ghost nem
  prefetch. É aceitável como escolha (evita o estado mutável e o I/O escondido que
  o Lazy Load clássico traz), mas deve ser decisão consciente e documentada.
- **Recomendação:** documentar a ausência como decisão; se se quiser ergonomia,
  `prefetch_related` (uma segunda consulta `IN (…)` agrupada) é o acréscimo mais
  barato e mantém o I/O explícito.
- **Esforço:** M.
- **ADR/Tijolo:** tijolo `prefetch-related` (opcional); nota em ARCHITECTURE.

---

## Invariantes do ARCHITECTURE.md — verificação

Os três invariantes centrais **são de facto respeitados pelo código**:

1. **"Nenhum identificador vem do chamador."** Imposto em dois pontos: a
   `registry::resolve` devolve `&'static FieldMeta` ou erro (`registry.rs:210`), e
   o `quote` do compilador só recebe strings estáticas e ainda assim valida
   defensivamente (`compiler.rs:105-112`). Os caminhos de escrita confirmam-no:
   `compile_insert` faz `meta.field(name)` e cita `f.column` estático
   (`compiler.rs:1030,1060`); `compile_f`/`compile_update` fazem
   `plan.model.field(name)` e citam `field.column` (`compiler.rs:833,899`). A única
   entrada de string de chamador no SQL é o texto de `raw()` — a saída de emergência
   **documentada** (`queryset.rs:110-128`).
2. **"Nenhum valor é formatado no SQL."** Todo `Value` passa por `binder.bind` →
   `$n`/`?n` (`compiler.rs:165-171`, tabela em `ARCHITECTURE.md:57`). Excepções
   **por desenho, não de chamador**: `LIMIT`/`OFFSET` escritos *inline* por serem
   `i64` (`compiler.rs:704-709`, justificado em `:702-703`) e o `'%' ||` literal do
   `endswith` (`compiler.rs:316`).
3. **"O compilador não faz I/O."** `compiler.rs` não importa `db`, `row`,
   `tokio_postgres` nem `rusqlite` (verificado por *grep*: zero ocorrências); só
   depende de `error`/`expr`/`lookup`/`meta`/`registry`/`value`. Imposto pela
   fronteira de módulo e pelos testes de snapshot (`ARCHITECTURE.md:58-59`).

Os dois front-ends **convergem limpo**: um único tipo `Plan`, um único
`compile_select_on`, com o snapshot
`filter_by_string_key_compiles_the_same_as_the_typed_form` a fixar a igualdade
(`ARCHITECTURE.md:46-47`). A dedução de *joins* é determinística
(`registry.rs:199-201`, `BTreeMap` em `compiler.rs:179-180`), o que mantém os
snapshots estáveis — base sólida para o `Plan` como IR (DESIGN-03).
