# gateway.md — Eixo API nativa poliglota (Query IR, gRPC, OpenAPI, SDKs)

> Revisor do eixo poliglota. Hipóteses atribuídas: **H11** (API gerada não é de
> produção) e **H12** (poliglotismo bloqueado). Narrativa em português europeu
> pré-1990; identificadores em inglês. **Sem alterações de código** — apenas
> achados e desenho para a Fase 3.
>
> Ficheiros lidos: `crates/dorm-core/src/{compiler.rs, registry.rs, meta.rs}`,
> `crates/dorm-schema/src/codegen_api.rs`, `docs/adr/0001-declarative-schema.md`,
> `ARCHITECTURE.md`. Negativos confirmados por `grep` sobre todo o workspace.

---

## Achados

### [GW-01] `Plan` e `Sql` não são serializáveis — não há contrato de consulta que atravesse um processo — Alto
- **Hipótese:** H12 (confirmada)
- **Evidência:** `crates/dorm-core/src/compiler.rs:39` (`#[derive(Debug, Clone, PartialEq)] pub struct Sql`) e `compiler.rs:50` (`#[derive(Debug, Clone)] pub struct Plan`); `grep` em todo o `dorm-core` não encontra `derive(Serialize)` em `Plan`, `Sql`, nem sequer em `value::Value` (`crates/dorm-core/src/value.rs` sem qualquer `Serialize`).
- **Impacto:** o `Plan` é o centro da arquitectura ("a plan in, parameterised SQL out", `ARCHITECTURE.md:13`), mas vive só na memória do processo Rust. Não existe representação externa, versionada e validável de uma consulta. Logo, nenhum cliente fora do binário Rust pode construir uma consulta segura: o poliglotismo está bloqueado na raiz. Além disso, `Plan.model` é `&'static ModelMeta` (`compiler.rs:52`) — um ponteiro para dados do binário, impossível de desserializar de um pedido remoto; teria de passar a ser uma referência por **nome lógico** resolvida contra o registo no servidor.
- **Recomendação:** definir um **Query IR** em Protocol Buffers (`dorm.v1`) que é o `Plan` serializado por nome lógico; `Sql` nunca atravessa a fronteira (é saída pura do compilador, produzida só no servidor). Ver Parte B §1.
- **Esforço:** L
- **ADR/Tijolo:** ADR novo 0010 (Query IR `dorm.v1`); tijolo `dorm-ir` (tipos proto + conversão `Plan`↔IR).

### [GW-02] `ModelMeta` não é publicável como descritor em runtime — o Data Plane só conhece modelos compilados no seu próprio binário — Alto
- **Hipótese:** H12 (confirmada)
- **Evidência:** `crates/dorm-core/src/meta.rs:99-119` (`ModelMeta` é `&'static`, todos os campos `&'static str`/`&'static [..]`); `meta.rs:160` `inventory::collect!(ModelMeta)` — o catálogo é montado pelo *linker*; `registry::models()` itera uma *distributed slice* (`registry.rs:14-16`). Não há serialização de `ModelMeta` nem forma de o exportar; `meta.rs` não deriva `Serialize`.
- **Impacto:** um servidor de dados só pode servir os modelos que foram compilados para dentro dele (ADR 0001: "a recompile, not an edit"). Não há descritor que descreva os modelos a um cliente poliglota nem a um Data Plane multi-esquema. O resolvedor de identificadores (`registry::resolve`, `registry.rs:210`), que é o guardião do invariante "nenhum identificador vem do cliente", opera exclusivamente sobre essa *slice* estática.
- **Impacto secundário:** sem descritor exportável, os SDKs poliglotas não têm fonte de verdade sobre campos/tipos/relações — teriam de ser escritos à mão (o mesmo defeito do OpenAPI, GW-04).
- **Recomendação:** gerar um **descritor de modelos** (`dorm.schema.v1.SchemaDescriptor`) a partir de `dorm.schema`/`ModelMeta`, carregável em runtime pelo Data Plane, **sem** abandonar o caminho compilado: os dois convivem (o compilado para quem embebe o ORM; o descritor para o servidor multi-esquema e para os SDKs). Ver Parte B §2.
- **Esforço:** L
- **ADR/Tijolo:** ADR novo 0011 (catálogo em runtime); tijolo `dorm-descriptor`.

### [GW-03] Não existe `tonic`/`prost`/`buf` nem crate `dorm-server` — sem caminho gRPC, streaming ou transacções remotas — Alto
- **Hipótese:** H11 (confirmada), H12 (confirmada)
- **Evidência:** `grep` por `tonic|prost|buf` em todos os `Cargo.toml` do workspace: **zero** resultados; `ls crates/` → 10 crates, nenhum `dorm-server` (confirmado o inventário do discovery `docs/review/00-discovery.md:19-30`). `grep` por `stream|cursor|copy_in|copy_out` em `db.rs` só devolve um comentário (`db.rs:175`), não um caminho de streaming.
- **Impacto:** o único modo de rede é a API CRUD gerada em Axum (`codegen_api.rs`). Não há Data Plane de grau de produção: sem gRPC, sem streaming de linhas, sem *deadlines* mapeados a `statement_timeout`, sem transacções interactivas remotas por sessão. Tudo o que não seja um embedder Rust fica sem caminho.
- **Recomendação:** criar o Data Plane `dorm-server` sobre `tonic` como caminho crítico (gRPC + streaming + sessões transaccionais com TTL e *idempotency keys*); REST/OpenAPI gerados **da mesma** definição. Ver Parte B §3.
- **Esforço:** L
- **ADR/Tijolo:** ADR novo 0012 (Data Plane gRPC); tijolo `dorm-server`.

### [GW-04] A API gerada em Axum não é de produção: OpenAPI à mão, sem auth/authz, sem tecto de paginação, corpo dinâmico — Alto
- **Hipótese:** H11 (confirmada)
- **Evidência:**
  - OpenAPI construído à mão por `json!` (`codegen_api.rs:280-341`, versão `"openapi": "3.0.3"` em `:336`), separado da implementação — pode divergir dos handlers em silêncio.
  - Sem autenticação nem autorização: `grep` por `auth|jwt|bearer|token|tls|mtls` em `codegen_api.rs` → **zero**; o `main_fn` (`:242-277`) só regista `/health`, `/openapi.json` e as rotas CRUD, sem *middleware* nenhum.
  - Paginação por offset sem tecto: `limit.unwrap_or(50)` sem limite máximo (`codegen_api.rs:153`) + `offset.unwrap_or(0)` — paginação profunda cara e sem travão de custo.
  - Corpo de escrita é `serde_json::Map` dinâmico (`codegen_api.rs:131`, `:163-183`), não um esquema de pedido tipado (o próprio cabeçalho admite "typed request schemas are on the roadmap", `codegen_api.rs:8-9`).
  - Erros devolvem o `Display` interno cru ao cliente (`err()` em `codegen_api.rs:77-79` → `StatusCode::INTERNAL_SERVER_ERROR` com `e.to_string()`): fuga de detalhe interno.
- **Impacto:** a API gerada serve demonstrações, não produção. Multi-tenancy, custo de consulta, autorização por modelo/operação e um contrato OpenAPI fiável estão todos ausentes. O ADR 0001 já classifica `dorm api` como "planned ... not yet built" (`0001-declarative-schema.md:58-63`) — a implementação actual é a prova mínima, não o produto.
- **Recomendação:** substituir a geração à mão por geração a partir da **mesma** definição proto (OpenAPI 3.1 via `gnostic`/transcoding), introduzir auth/authz, paginação por *keyset* com tecto, esquemas de pedido tipados e saneamento de erros. Ver Parte B §3 e §6.
- **Esforço:** L
- **ADR/Tijolo:** ADR 0012 (Data Plane) + ADR novo 0014 (segurança/multi-tenancy); tijolos `dorm-server`, `dorm-openapi`.

### [GW-05] Não há execução de objectos de BD remotos nem allow-list de SQL nomeado — o único SQL cru existente é local e por posição — Médio
- **Hipótese:** H11 (confirmada), nova nuance sobre H12
- **Evidência:** o compilador só emite CRUD/agregação a partir do `Plan` (`compiler.rs`, funções `compile_select`/`compile_insert`/`compile_update`/`compile_delete`); não há caminho para invocar *procedures*/funções de BD. O SQL cru existe só no caminho local (`Manager::raw`/`db::sql`, registado no discovery e na memória `dorm-production-hardening-campaign`), sempre por **posição** (`model.rs:27-29`, discovery H10). Não existe qualquer registo de *statements* nomeados nem allow-list versionada.
- **Impacto:** num Data Plane poliglota, abrir SQL cru ao cliente violaria o invariante nuclear ("No identifier comes from a caller", `ARCHITECTURE.md:56`). Sem um mecanismo de objectos declarados (procedures/funções) e de *statements* nomeados registados, ou se fecha a porta a SQL avançado, ou se abre um buraco de injecção.
- **Recomendação:** procedures/funções declaradas no esquema → RPCs tipados; SQL cru remoto **só** por *statements* nomeados registados numa allow-list versionada (nunca texto livre do cliente). Ver Parte B §4.
- **Esforço:** M
- **ADR/Tijolo:** ADR novo 0013 (objectos de BD remotos + allow-list de SQL nomeado); tijolo no `dorm-server` + registo no descritor.

### [GW-06] Sem SDKs poliglotas nem verificação de contrato (`buf lint`/`buf breaking`) — Médio
- **Hipótese:** H12 (confirmada)
- **Evidência:** os únicos "clientes" são o Rust tipado e o REPL (`ARCHITECTURE.md:43-50`), ambos dentro do mesmo binário. Não há `.proto`, logo não há geração de SDKs TS/Python/Go/Java/Kotlin/C#, nem CI de compatibilidade (`grep buf` → zero).
- **Impacto:** o objectivo "poliglota" não tem artefacto. Sem `.proto` + descritor como fonte única, qualquer cliente noutra linguagem seria escrito à mão e divergiria (o mesmo defeito do OpenAPI à mão, GW-04).
- **Recomendação:** SDKs gerados do `.proto` + descritor, com `buf lint` e `buf breaking` como portões de CI (o contrato nunca quebra em silêncio, à imagem dos *snapshots* SQL). Ver Parte B §5.
- **Esforço:** M
- **ADR/Tijolo:** ADR 0010 (IR) + tijolo `dorm-sdk-*`; CI `buf`.

### Síntese dos achados

| # | Severidade | Hipótese | Essência |
|---|---|---|---|
| GW-01 | Alto | H12 | `Plan`/`Sql` sem `Serialize`; sem Query IR |
| GW-02 | Alto | H12 | `ModelMeta` `&'static`/`inventory`; sem descritor em runtime |
| GW-03 | Alto | H11, H12 | sem `tonic`/`prost`/`buf`, sem `dorm-server`, sem streaming/tx remotas |
| GW-04 | Alto | H11 | Axum gerado: OpenAPI à mão, sem auth, sem tecto, corpo dinâmico, erros crus |
| GW-05 | Médio | H11 | sem objectos de BD remotos nem allow-list de SQL nomeado |
| GW-06 | Médio | H12 | sem SDKs poliglotas nem `buf lint/breaking` no CI |

**Veredicto:** H11 e H12 **confirmadas**, cada uma por evidência de código e por
negativos de `grep`. O `Plan` é um contrato interno excelente mas estritamente
in-process; o poliglotismo exige externalizá-lo sem perder os invariantes.

---

## Arquitectura-alvo (esboço)

O princípio director: **o `Plan` já é o IR certo; falta-lhe uma pele externa.**
Todo o desenho abaixo externaliza o `Plan` e o `ModelMeta` para a rede **sem**
mover para o cliente nenhuma das três decisões de segurança que o compilador
hoje garante por construção. Repetem-se aqui os três invariantes do
`ARCHITECTURE.md:54-60` para fixar a prova:

- **I1 — nenhum identificador vem do cliente** (`registry::resolve` → `&'static FieldMeta`; o compilador cita só *strings* estáticas).
- **I2 — nenhum valor é formatado dentro do SQL** (todo `Value` é ligado como `$n`).
- **I3 — o compilador não faz I/O** (puro; fixado por *snapshots*).

### 1. Query IR (`dorm.v1`, Protocol Buffers)

O `Plan` de `compiler.rs:50` serializado, versionado no pacote `dorm.v1` e
validado contra o descritor **antes** de tocar no compilador. A mensagem de topo
`Query` espelha campo a campo o `Plan`. A diferença essencial e deliberada:
`Plan.model` deixa de ser `&'static ModelMeta` e passa a ser uma `string` com o
**nome lógico** do modelo; o servidor resolve-a com `registry::model(name)`
(`registry.rs:19`) e todos os caminhos de filtro/ordenação/valores continuam a
passar por `registry::resolve` (`registry.rs:210`). O cliente nunca envia tabelas
nem colunas — só nomes lógicos que o servidor valida. **O `Sql` (`compiler.rs:39`)
nunca é uma mensagem de entrada**: é saída pura do compilador, produzida só no
servidor; expô-lo ao cliente seria abrir SQL livre.

- **Preserva I1:** o IR referencia modelos/campos por nome lógico; a resolução
  continua a ser o único sítio onde uma *string* vira identificador, agora no
  servidor. Um nome inexistente é um erro de validação do IR, não SQL. O cliente
  não tem como nomear uma tabela/coluna que o descritor não declare.
- **Preserva I2:** os valores viajam no IR como `Value` tipado (um `oneof`
  escalar), nunca como texto SQL; no servidor continuam a ser ligados como `$n`
  pelo mesmo `Binder` (`compiler.rs:150-172`).
- **Preserva I3:** a conversão IR→`Plan` é pura e sem I/O; o compilador permanece
  intocado, continuando a ser fixado pelos *snapshots* (`sql_snapshots.rs`), que
  ganham um irmão: *snapshots* de IR→`Plan`.

### 2. Catálogo de modelos em runtime (descritor)

`ModelMeta`/`FieldMeta`/`Relation` (`meta.rs:45-119`) publicados como um
`SchemaDescriptor` gerado de `dorm.schema` (a mesma fonte do ADR 0001). O Data
Plane carrega o descritor em runtime e constrói um registo equivalente à
*distributed slice*, para poder **servir modelos que não conhece em tempo de
compilação**. O caminho compilado do ADR 0001 (`0001-declarative-schema.md:29-36`)
**mantém-se**: quem embebe o ORM em Rust continua com `inventory` e tipos
`&'static`; o descritor é o modo adicional para o servidor multi-esquema e para
os SDKs. Os dois convivem porque ambos descrevem exactamente a mesma coisa — o
descritor é `ModelMeta` projectado em proto, e `registry::validate`
(`registry.rs:65`) corre igual sobre qualquer dos dois.

- **Preserva I1:** o descritor **é** a lista de identificadores permitidos; a
  resolução valida contra ele tal como hoje valida contra a *slice*. Um Data
  Plane sem o descritor de um modelo recusa qualquer consulta a esse modelo.
- **Preserva I2/I3:** o descritor só descreve estrutura (nomes, tipos, relações);
  não transporta valores nem gera SQL, pelo que não toca nestes invariantes.

### 3. Data Plane `dorm-server`: gRPC como caminho crítico, REST/OpenAPI derivados

Crate novo `dorm-server` sobre `tonic`. Serviço `dorm.v1.Query` com:
- **Streaming de linhas** (`stream QueryRow`) em vez de materializar tudo em
  memória — corrige a ausência de streaming notada em GW-03.
- **Deadlines → `statement_timeout`/cancelamento:** o *deadline* gRPC do pedido é
  traduzido no servidor para `statement_timeout` (já existe statement timeout no
  ADR 0004) e para cancelamento do `tokio-postgres`.
- **Transacções interactivas por sessão:** `Begin`/`Exec`/`Commit`/`Rollback`
  com identificador de sessão, **TTL** (uma sessão órfã expira e faz *rollback*)
  e **idempotency keys** por operação de escrita (reenvio seguro). Isto reaproveita
  o `db::atomic` existente e o trabalho de `dorm studio`/`:begin/:commit/:rollback`
  da memória `dorm-studio-and-interactive-tx`, agora exposto por rede.
- **OpenAPI 3.1 + REST gerados da MESMA definição** (transcoding gRPC↔JSON /
  `gnostic`), substituindo o `codegen_api.rs` à mão (GW-04). Nunca se escreve o
  OpenAPI à mão outra vez.

- **Preserva I1/I2:** cada RPC desemboca no `Plan`→compilador→`$n`; o servidor é
  apenas transporte + sessão, não um segundo sítio onde se constrói SQL.
- **Preserva I3:** o compilador continua puro; todo o I/O (pool, tx, deadlines)
  vive em `dorm-server`/`db.rs`, do lado de fora do compilador.

### 4. Execução de objectos de BD remotos

Procedures e funções **declaradas no esquema** tornam-se **RPCs tipados** no
descritor (entrada/saída tipadas, geradas, não texto). SQL cru remoto **só** por
**statements nomeados** registados numa **allow-list versionada** (declarados no
esquema, revistos como migração, com parâmetros `$n`), nunca texto livre vindo do
cliente. O cliente invoca pelo nome do *statement* + parâmetros tipados; o
servidor procura o texto na allow-list e liga os parâmetros.

- **Preserva I1:** o cliente nomeia um objecto/statement registado, não emite
  SQL; o texto vive no servidor (esquema/allow-list), não no pedido. É a extensão
  natural de "nenhum identificador vem do cliente" a procedures e SQL avançado.
- **Preserva I2:** os parâmetros de um *statement* nomeado são sempre ligados como
  `$n`; a allow-list guarda texto parametrizado, nunca interpolado.
- **Preserva I3:** a allow-list é dado estático (como os `checks` de `meta.rs:118`,
  que já entram "straight into the DDL" por virem do esquema e não do chamador); o
  compilador não é envolvido no caminho de *statement* nomeado.

### 5. SDKs poliglotas gerados

Do `.proto` (`dorm.v1`) + descritor geram-se SDKs para **TS, Python, Go,
Java/Kotlin, C#**. O construtor de consultas de cada SDK produz a mensagem
`Query` do IR (nunca SQL), com os nomes lógicos tipados a partir do descritor.
**`buf lint` e `buf breaking`** são portões de CI: o contrato `dorm.v1` não pode
regredir em silêncio — exactamente o papel que os *snapshots* SQL
(`sql_snapshots.rs`) e a paridade de esquema (`schema_parity.rs`) já desempenham
para o núcleo (`ARCHITECTURE.md:59-60`).

- **Preserva I1:** os SDKs só sabem nomes lógicos do descritor; não têm vocabulário
  para nomear uma tabela/coluna física. O que o cliente constrói é um `Plan`
  abstracto, validado de novo no servidor.
- **Preserva I2/I3:** os SDKs emitem valores tipados no IR; não constroem SQL nem
  fazem I/O de BD — toda a compilação e ligação `$n` acontece no servidor.

### 6. Segurança e multi-tenancy

- **mTLS/JWT** no `dorm-server` (transporte e identidade — ausentes hoje, GW-04).
- **Authz por modelo/operação** avaliada contra a identidade antes de o `Plan`
  compilar (p. ex. "este sujeito pode `SELECT` em `Application` mas não `DELETE`").
- **RLS por GUC transaccional:** o *tenant* corrente injectado como `SET LOCAL`
  dentro da transacção, para o PostgreSQL aplicar *Row-Level Security*; encaixa em
  `tenant_scoped`/`tenant_field` que `ModelMeta` já declara (`meta.rs:107`,
  `:134-145`).
- **Limites de custo de consulta:** tecto de linhas, profundidade máxima de joins
  (contável a partir de `Joins`/`resolve`, `registry.rs:277-333`) e tempo (o
  *deadline* de §3) — corrige o `limit` sem tecto de `codegen_api.rs:153`.
- **Quotas** por *tenant* (pedidos, sessões transaccionais abertas, linhas lidas).

- **Preserva I1:** authz e RLS **restringem** o que um identificador já validado
  pode fazer; nunca introduzem identificadores do cliente. O conjunto permitido
  continua a ser o descritor.
- **Preserva I2:** o *tenant* da RLS viaja como valor ligado (`SET LOCAL ... = $1`),
  não interpolado no SQL.
- **Preserva I3:** authz/quotas são verificações no `dorm-server`, antes e à volta
  do compilador puro; a RLS é responsabilidade do PostgreSQL. O compilador não
  passa a conhecer identidade nem rede.

### Mapeamento `Plan` → `dorm.v1` (Protocol Buffers)

Fonte: `compiler.rs:50-74` (`Plan`), `:38-44` (`Sql`), `:31-36` (`ColumnSpec`),
mais `expr.rs` (`Q`/`Predicate`/`OrderBy`/`Aggregate`/`F`/`Assignment`) e
`value.rs` (`Value`/`FieldType`).

| Campo/tipo Rust (ficheiro:linha) | Mensagem/campo `dorm.v1` | Notas de invariante |
|---|---|---|
| `Plan.model: &'static ModelMeta` (`compiler.rs:52`) | `Query.model: string` (nome lógico) | **I1**: `&'static` não serializa; vira nome resolvido por `registry::model` no servidor |
| `Plan.filters: Vec<Q>` (`:54`) | `repeated Q filters` | — |
| `Plan.excludes: Vec<Q>` (`:55`) | `repeated Q excludes` | — |
| `Plan.order: Vec<OrderBy>` (`:57`) | `repeated OrderBy order` | — |
| `Plan.unordered: bool` (`:65`) | `bool unordered` | — |
| `Plan.limit: Option<i64>` (`:66`) | `optional int64 limit` | servidor impõe tecto (§6) |
| `Plan.offset: Option<i64>` (`:67`) | `optional int64 offset` | — |
| `Plan.distinct: bool` (`:68`) | `bool distinct` | — |
| `Plan.select_related: Vec<String>` (`:70`) | `repeated string select_related` | **I1**: validado por `resolve` |
| `Plan.annotations: Vec<Aggregate>` (`:71`) | `repeated Aggregate annotations` | — |
| `Plan.values: Vec<String>` (`:73`) | `repeated string values` | **I1**: paths/aliases validados |
| `Q` enum (`expr.rs`): `Empty/Pred/And/Or/Not` | `message Q { oneof node { Empty empty; Predicate pred; And and; Or or; Q not; } }` | árvore recursiva; o servidor recusa `OR` que misture agregado e coluna (`compiler.rs:409`) |
| `Predicate { path, values }` (`expr.rs`) | `message Predicate { string path = 1; repeated Value values = 2; }` | **I1** path por `resolve`; **I2** values ligados `$n` |
| `OrderBy { path, descending }` (`expr.rs`) | `message OrderBy { string path = 1; bool descending = 2; }` | **I1** path validado |
| `Aggregate { alias, kind, path }` (`expr.rs`) | `message Aggregate { string alias = 1; AggregateKind kind = 2; string path = 3; }` | `kind`: `enum AggregateKind { COUNT; SUM; AVG; MIN; MAX; }` |
| `F` enum (`expr.rs`): `Col/Lit/Add/Sub/Mul/Div` | `message F { oneof { string col; Value lit; FBinOp add/sub/mul/div; } }` | usado só nos RPCs de `update` (SET) |
| `Assignment { field, expr: F }` (`expr.rs`) | `message Assignment { string field = 1; F expr = 2; }` | **I1** field validado |
| `Value` enum (`value.rs:85-94`) | `message Value { oneof v { google.protobuf.NullValue null; bool bool; int32 i32; int64 i64; string text; string decimal; string timestamp; string uuid; google.protobuf.Struct json; } }` | **I2**: decimal/timestamp/uuid como `string` para não perder precisão (espelha `value_to_json`, `codegen_api.rs:83-95`) |
| `FieldType` enum (`value.rs:27-35`) | `enum FieldType { I32; I64; TEXT; BOOL; DECIMAL; TIMESTAMP; UUID; JSON; }` | só no descritor/`ColumnSpec`, não em pedidos |
| `ColumnSpec { name, ty }` (`compiler.rs:31-36`) | `message ColumnSpec { string name = 1; FieldType ty = 2; }` | metadados da **resposta** (streaming) |
| `Sql { text, params, columns }` (`compiler.rs:38-44`) | **sem mensagem de entrada** | **I1**: `text` é SQL; nunca entra pela rede, só sai do compilador no servidor |
| `ModelMeta`/`FieldMeta`/`Relation` (`meta.rs:45-119`) | `SchemaDescriptor { repeated ModelDescriptor models }` (§2) | catálogo em runtime, gerado de `dorm.schema` |

Mensagens de topo do serviço: `service Query { rpc Select(Query) returns (stream QueryRow); rpc Count(Query) returns (CountReply); rpc Exists(Query) returns (ExistsReply); rpc Insert(InsertRequest) returns (stream QueryRow); rpc Update(UpdateRequest) returns (AffectedReply); rpc Delete(Query) returns (AffectedReply); rpc Begin/Exec/Commit/Rollback(...) ... }` — cada uma a desembocar nas funções `compile_*` existentes, intocadas.
