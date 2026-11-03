# ADR 0021 — Âmbito de inquilino imposto no núcleo (fail-closed)

Status: Accepted — núcleo implementado (Fase 2) · 2026-11-03

## Context

`ModelMeta` já **declara** a tenancy mas **nada a impõe**. `tenant_scoped: bool`
e `tenant_field()` existem (`crates/dorm-core/src/meta.rs:108`, `:134-145`), e o
derive aceita `#[model(tenant_scoped)]`, mas o compilador e o `QuerySet` são
agnósticos ao inquilino: `compiler.rs` nunca lê `tenant_scoped` (a única menção é
um comentário, `compiler.rs:995`) e `queryset.rs` idem. O isolamento é, hoje,
**convenção aplicada nas bordas**: o MCP empurra o predicado para dentro do `Plan`
à mão (`crates/dorm-mcp/src/tools.rs:412,449,576`) e a shell inclui o filtro nas
queries que constrói (`crates/dorm-shell/src/main.rs:336`). Até o caminho tipado
exige o filtro explícito — `search_live.rs:138` só isola porque escreve
`.filter(ticket::tenant.eq("acme"))`.

As consequências, confirmadas por código:

- **Uma query tipada que esqueça o filtro cruza inquilinos e nada falha.** Não há
  contexto de inquilino por tarefa análogo a `with_read_only` (`db.rs:356`); os
  únicos `task_local!` (`db.rs:120,146,…`) são o `READ_ONLY_SCOPE` e timeouts.
- **Não há fail-closed.** O enum `Error` (`error.rs:18-90`) não tem variante de
  inquilino; uma query sem âmbito corre sem predicado em vez de recusar.
- **`:raw` e `db execute` atravessam todos os inquilinos** (ADR 0005, válvula de
  SQL cru) e não há RLS que os trave: `grep` por `ROW LEVEL SECURITY`,
  `current_setting`, `CREATE POLICY`, `SET LOCAL` em `dorm-migrations`/`db.rs`
  devolve zero.

Para uma plataforma multi-inquilino de dez anos, o isolamento não pode ser uma
disciplina que cada chamador tem de lembrar. Tem de ser uma propriedade de
construção, como já é a segurança de identificadores.

## Decision

Trazer o âmbito de inquilino para o único ponto por onde **ambas** as frentes já
convergem, mantendo o compilador puro.

- **Contexto de inquilino por tarefa.** `db::with_tenant(tenant: Value, fut)` — um
  `task_local!` no mesmo molde de `with_read_only` (`db.rs:356`), herdável por
  sub-tarefas da mesma forma. O valor é um `Value` ligado, nunca texto.
- **Injecção `Plan → Plan`, fora do compilador.** Uma transformação
  `Plan::scoped_for(tenant)` aplica, para modelos `tenant_scoped`:
  - em `SELECT`/`UPDATE`/`DELETE`, um predicado `tenant_field = $tenant` em `AND`
    com os filtros do `Plan`, sobre o `&'static FieldMeta` que `tenant_field()`
    devolve (nunca um identificador do chamador);
  - em `INSERT`, o valor da coluna de inquilino, sobrepondo-se a qualquer valor
    fornecido.
  A transformação corre **antes** de o `Plan` entrar no compilador, num único
  *choke point* (`compile_scoped`) por onde passam os terminais tipados do
  `QuerySet` **e** o `fetch_value_rows` model-erased da REPL/MCP/Studio. O
  compilador continua a receber um `Plan` e a devolver `Sql` — sem conhecer
  inquilino, sem ler estado ambiente, sem I/O.
- **Fail-closed.** Compilar um `Plan` de um modelo `tenant_scoped` sem contexto de
  inquilino activo devolve `Error::TenantScopeMissing { model }`. A única excepção
  é um âmbito de sistema explícito e auditado, `db::with_system_scope(fut)`, que
  emite um evento `tracing` por entrada (nunca silencioso).
- **Defesa em profundidade por RLS (opcional).** O gerador de migrações emite, por
  modelo `tenant_scoped`, `ALTER TABLE … ENABLE ROW LEVEL SECURITY` e uma
  `CREATE POLICY … USING (tenant = current_setting('dorm.tenant', true)::<tipo>)`;
  o `db` faz `SET LOCAL dorm.tenant = $1` em cada transacção com inquilino activo
  (o `set_local` seguro, com chave validada contra allow-list e valor ligado, é o
  do ADR 0023). Com RLS activo, nem o `:raw` nem o `db execute` lêem linhas de
  outro inquilino — o PostgreSQL recusa-as mesmo quando o `Plan` não existe.

### Como preserva os invariantes do `ARCHITECTURE.md`

- **I1 (nenhum identificador vem do chamador):** o predicado e a coluna de INSERT
  são construídos sobre o `&'static FieldMeta` de `tenant_field()`; a chave do GUC
  de RLS é validada contra uma allow-list. Nada novo vem do chamador.
- **I2 (nenhum valor é formatado no SQL):** o inquilino viaja sempre como `$n`
  ligado — no predicado, no INSERT e no `SET LOCAL dorm.tenant = $1`.
- **I3 (o compilador não faz I/O e é puro):** a leitura do `task_local` e a
  transformação `Plan → Plan` vivem *fora* do compilador; este recebe um `Plan`
  já com o predicado. Os novos predicados entram no contrato de *snapshot*.

## Consequences

- **Prós:** o isolamento passa a ser fail-closed e uniforme em todas as
  superfícies (tipada, dinâmica, `:raw` sob RLS, REPL, MCP, Studio) sem cada
  chamador o lembrar. A RLS fecha também o caminho de SQL cru.
- **Contras / custo:** todo o modelo `tenant_scoped` exige um contexto activo —
  caminhos administrativos legítimos têm de usar `with_system_scope`
  explicitamente; a RLS exige que as *policies* existam na base (eixo migrações),
  senão o `SET LOCAL` não tem efeito. Os *snapshots* de SQL de modelos
  `tenant_scoped` mudam (ganham o predicado) — uma reescrita única e revista.
- **Dependências:** usa o `set_local` seguro do ADR 0023 para o GUC da RLS.
  Independente dos restantes.

## Implementação (Fase 2)

- O *seam* vive em `crates/dorm-core/src/scope.rs` (`compile_select/count/exists/
  aggregate/update/delete` + `compile_insert`), uma transformação `Plan → Plan`
  (`scoped_plan`) e `rows → rows` (`scoped_rows`) **fora** do compilador puro.
- Os task-locals `TENANT`/`SYSTEM_SCOPE` e `with_tenant`/`with_system_scope`/
  `current_tenant`/`is_system_scope` estão em `db.rs`, no molde do `with_read_only`.
- Todos os terminais de execução do `queryset.rs` e as operações de instância do
  `model.rs` passaram a compilar por `scope::*`. **Desvio deliberado:**
  `QuerySet::sql()` (preview/`--sql`) fica no compilador **puro** — não devolve
  linhas e tem de ser determinístico para os *snapshots*, por isso não fail-close.
- Superfícies que compilam directamente: o **MCP** passou a correr cada tool sob
  `with_tenant` (`tools.rs::execute`); o **worker RAG** lê a linha-fonte sob
  `with_system_scope` (`worker.rs`), por ser um indexador inter-inquilino.
- **Por fazer (follow-up):** a geração de DDL de RLS + `SET LOCAL dorm.tenant` por
  transacção (defesa em profundidade para `:raw`), e migrar o **Studio** para o
  *seam*. O `set_local` seguro (ADR 0023) já existe para quando a RLS entrar.

## Proof

- **Unit:** `scope::tests` — injecção do predicado sob `with_tenant`, bypass sob
  `with_system_scope`, `TenantScopeMissing` sem contexto, e tenant forçado no
  INSERT mesmo quando a linha nomeia outro.
- **Live (provado, `dorm-models/tests/tenant_scope_live.rs`):** dois inquilinos
  numa base — a superfície **tipada** (sem `.filter`) e a **dinâmica**
  (`fetch_value_rows`) só vêem o seu inquilino; sem contexto é `TenantScopeMissing`;
  `with_system_scope` vê ambos; um INSERT sob A é forçado a A mesmo nomeando B.
  O `typed_e2e` e o `prefetch_live` migraram para o novo contexto e passam.
