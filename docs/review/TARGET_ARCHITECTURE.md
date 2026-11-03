# DORM — Arquitectura-alvo poliglota

> Como externalizar o DORM para qualquer linguagem **sem** mover para o cliente
> nenhuma das três decisões de segurança que o compilador hoje garante por
> construção. Narrativa em português europeu; identificadores em inglês.
> Data: 2026-10-31. Decisões em `docs/adr/0010`–`0014`.
>
> Princípio director: **o `Plan` já é o IR certo; falta-lhe uma pele externa.**

---

## 1. Os invariantes são a especificação, não um detalhe

Toda a arquitectura abaixo é avaliada por uma única régua — preservar os três
invariantes do `ARCHITECTURE.md`, que a revisão **verificou serem verdadeiros
hoje** (`findings/security.md`, `findings/design.md`):

- **I1 — nenhum identificador vem do chamador.** Tabela/coluna/join chegam ao SQL
  só via `registry::resolve` → `&'static FieldMeta`; o compilador cita apenas
  *strings* estáticas (`compiler.rs:105` `quote` defensivo).
- **I2 — nenhum valor é formatado dentro do SQL.** Todo `Value` é ligado como
  `$n` pelo `Binder` (`compiler.rs:150-172`).
- **I3 — o compilador não faz I/O.** Puro; fixado pelos *snapshots*
  (`sql_snapshots.rs`, `schema_parity.rs`).

A regra poliglota é, portanto: **o cliente descreve uma consulta por nomes
lógicos e valores tipados; o servidor é o único sítio onde um nome vira
identificador e um valor vira `$n`.** O `Sql { text, params }` nunca atravessa a
rede como entrada — é sempre saída pura do compilador, produzida só no servidor.

---

## 2. Visão geral

```mermaid
flowchart TB
  subgraph clients["Clientes poliglotas (SDKs gerados)"]
    ts["TypeScript"]; py["Python"]; go["Go"]; jv["Java/Kotlin"]; cs["C#"]
  end

  subgraph wire["Contrato de fio — dorm.v1 (Protocol Buffers)"]
    ir["Query IR\n(Plan por nome lógico)"]
    desc["SchemaDescriptor\n(ModelMeta projectado)"]
  end

  subgraph server["dorm-server (tonic) — Data Plane"]
    authz["Auth (mTLS/JWT) + authz\npor modelo/operação + quotas"]
    reg["registry::resolve\n(valida nomes contra o descritor)"]
    comp["compiler (PURO, intocado)\nPlan → Sql{text,$n}"]
    sess["Sessões tx interactivas\n(TTL + idempotency keys)\ndeadline → statement_timeout"]
    allow["Allow-list de SQL nomeado\n+ procedures como RPC"]
  end

  subgraph core["dorm-core (embebido, ADR 0001 — inalterado)"]
    inv["inventory + tipos &'static\n(caminho compilado)"]
  end

  db[("PostgreSQL / SQLite\n(RLS por SET LOCAL)")]

  clients -->|constroem Query| ir
  ir --> authz --> reg --> comp --> sess --> db
  desc -. gera .-> clients
  allow --> sess
  inv -. mesma fonte dorm.schema .-> desc
  comp -. mesmo compilador .- inv
```

Dois modos convivem, descrevendo **a mesma coisa**:

- **Embebido (ADR 0001, inalterado):** uma aplicação Rust depende de `dorm-core`,
  os modelos entram por `inventory` como tipos `&'static`, o compilador corre
  em processo. É o caminho provado; não muda.
- **Servido (novo):** `dorm-server` carrega um **descritor** gerado da mesma
  `dorm.schema`, aceita um **Query IR** pela rede, valida-o contra o descritor,
  e corre o **mesmo compilador** para produzir o mesmo SQL. O cliente pode estar
  em qualquer linguagem.

---

## 3. As cinco peças

### 3.1 Query IR `dorm.v1` (ADR 0010)

O `Plan` (`compiler.rs:50`) serializado em Protocol Buffers, versionado no pacote
`dorm.v1`. A diferença deliberada: `Plan.model` deixa de ser `&'static ModelMeta`
e passa a `string` com o **nome lógico**, resolvido no servidor por
`registry::model(name)`. O mapeamento campo-a-campo `Plan`→proto está em
`findings/gateway.md` (secção "Mapeamento `Plan` → `dorm.v1`").

- **I1:** o IR referencia modelos/campos por nome lógico; a resolução continua a
  ser o único sítio onde uma *string* vira identificador, agora no servidor. Nome
  inexistente = erro de validação do IR, não SQL.
- **I2:** valores viajam como `Value` tipado (um `oneof` escalar), nunca texto;
  ligados como `$n` pelo mesmo `Binder`.
- **I3:** a conversão IR→`Plan` é pura; o compilador fica intocado e ganha um
  *snapshot* irmão IR→`Plan`.

### 3.2 Catálogo de modelos em runtime (ADR 0011)

`ModelMeta`/`FieldMeta`/`Relation` (`meta.rs:45-119`) publicados como
`SchemaDescriptor` gerado de `dorm.schema`. O Data Plane carrega-o em runtime e
constrói um registo equivalente à *distributed slice* — para servir modelos que
não conhece em tempo de compilação. **É o descritor que define o conjunto de
identificadores permitidos**; `registry::validate` corre igual sobre ele.

### 3.3 Data Plane `dorm-server` (ADR 0012)

Crate novo sobre `tonic`. `service dorm.v1.Query` com: `Select` → `stream
QueryRow` (streaming, não materializar tudo); *deadline* gRPC → `statement_timeout`
+ cancel do `tokio-postgres`; `Begin`/`Exec`/`Commit`/`Rollback` por sessão com
**TTL** (sessão órfã expira e faz rollback) e **idempotency keys** por escrita.
REST/OpenAPI 3.1 **gerados da mesma definição** (transcoding), substituindo o
`codegen_api.rs` à mão (GW-04). Cada RPC desemboca nas funções `compile_*`
existentes, intocadas.

### 3.4 Objectos de BD remotos e SQL nomeado (ADR 0013)

Procedures/funções **declaradas no esquema** → **RPCs tipados** (entrada/saída
geradas). SQL cru remoto **só** por *statements* **nomeados** registados numa
**allow-list versionada** (declarados no esquema, revistos como migração,
parâmetros `$n`), nunca texto livre do cliente. Preserva I1 porque o texto vive
no servidor; preserva I2 porque os parâmetros são sempre `$n`.

### 3.5 SDKs poliglotas (ADR 0010/0012)

Do `.proto` + descritor geram-se SDKs para TS, Python, Go, Java/Kotlin, C#. Cada
SDK produz a mensagem `Query` do IR (nunca SQL), com nomes lógicos tipados do
descritor. **`buf lint` + `buf breaking`** são portões de CI — o contrato
`dorm.v1` não regride em silêncio, à imagem dos *snapshots* SQL.

---

## 4. Segurança e multi-tenancy (ADR 0014)

- **mTLS/JWT** no `dorm-server` (transporte + identidade — ausentes hoje, GW-04).
- **Authz por modelo/operação** avaliada antes de o `Plan` compilar.
- **RLS por GUC transaccional:** o tenant corrente injectado como `SET LOCAL
  app.tenant_id = $1` dentro da tx (valor ligado, não interpolado — I2); encaixa
  em `tenant_scoped`/`tenant_field` que `ModelMeta` já declara (`meta.rs:107`).
- **Limites de custo:** tecto de linhas, profundidade máxima de joins (contável de
  `registry::resolve`), tempo (o *deadline* de §3.3) — corrige o `limit` sem tecto
  de `codegen_api.rs:153`.
- **Quotas** por tenant (pedidos, sessões abertas, linhas lidas).

Authz e RLS **restringem** o que um identificador já validado pode fazer; nunca
introduzem identificadores do cliente (I1). O compilador não passa a conhecer
identidade nem rede (I3).

---

## 5. O que NÃO muda

- O compilador e os seus *snapshots* — a conversão IR→`Plan` encosta-lhe por fora.
- O caminho embebido do ADR 0001 (`inventory` + `&'static`).
- Os invariantes — são a régua de aceitação de cada peça, não uma vítima dela.

## 6. Dependências entre decisões

```
0011 (descritor) ─┐
0010 (Query IR)  ─┼─→ 0012 (Data Plane) ─→ 0013 (objectos remotos)
                  └─────────────────────┬─→ 0014 (segurança/multi-tenancy)
                                         └─→ SDKs + buf (CI)
```

A ordem de construção segue o `ROADMAP_BRICKS.md`: o IR e o descritor primeiro
(são tipos + conversão pura, sem rede), o Data Plane depois, e a segurança a
acompanhar o Data Plane desde o primeiro RPC — nunca como camada posterior.
