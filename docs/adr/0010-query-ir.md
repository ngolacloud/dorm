# ADR 0010 — Query IR (`dorm.v1`), serializável e versionado

Status: Proposed · 2026-10-31

## Context

O `Plan` é o centro da arquitectura — "a plan in, parameterised `Sql` out, no
I/O" (`ARCHITECTURE.md:13`, `:30-33`) — mas vive só na memória do processo Rust.
Não deriva `Serialize`: `#[derive(Debug, Clone)] pub struct Plan`
(`crates/dorm-core/src/compiler.rs:50`), tal como `Sql` (`compiler.rs:39`) e o
próprio `Value` (`crates/dorm-core/src/value.rs`). O achado GW-01 confirma-o por
`grep` negativo em todo o `dorm-core`. Pior ainda para a rede: `Plan.model` é um
`&'static ModelMeta` (`compiler.rs:52`) — um ponteiro para dados do binário,
impossível de reconstruir a partir de um pedido remoto.

Sem uma representação externa, versionada e validável de uma consulta, nenhum
cliente fora do binário Rust pode construir uma consulta segura. O poliglotismo
(H12) está bloqueado na raiz. O princípio director da arquitectura-alvo
(`gateway.md` Parte B) é que **o `Plan` já é o IR certo; falta-lhe uma pele
externa** que o transporte sem mover para o cliente nenhuma das três decisões de
segurança que o compilador hoje garante por construção.

## Decision

Definir um **Query IR** em Protocol Buffers, no pacote versionado `dorm.v1`, que
é o `Plan` serializado. A mensagem de topo `Query` espelha o `Plan` campo a
campo (mapeamento completo em `gateway.md`, secção "Mapeamento `Plan` →
`dorm.v1`"). A conversão `Query` (IR) → `Plan` (interno) vive num crate novo
`dorm-ir`, é pura e sem I/O.

A diferença essencial e deliberada face ao `Plan` interno:

- **`Plan.model` deixa de ser `&'static ModelMeta` e passa a ser uma `string`
  com o nome lógico do modelo.** O servidor resolve-a com `registry::model(name)`
  (`crates/dorm-core/src/registry.rs:19`) e recusa um nome que o catálogo não
  declare. O cliente nunca envia tabelas nem colunas físicas — só nomes lógicos
  que o servidor valida (o catálogo em runtime é o ADR 0011).
- **`Sql` (`compiler.rs:39`) nunca é uma mensagem de entrada.** É saída pura do
  compilador, produzida só no servidor; expô-lo como entrada seria abrir SQL
  livre e violar o invariante nuclear. O IR não tem mensagem que transporte
  `Sql.text` do cliente para o servidor.

Invariante aplicado ao IR, afirmado explicitamente no `.proto` e nos testes:
**nenhum identificador vem do cliente**. Um `path` de filtro/ordenação, um nome
de relação em `select_related`, um `alias`/`path` de agregação — tudo viaja como
`string` lógica e tem de passar por `registry::resolve` (`registry.rs:210`) no
servidor; uma `string` que não resolva é um erro de validação do IR, não SQL.

### Como preserva os invariantes do `ARCHITECTURE.md`

- **I1 (nenhum identificador vem do cliente):** o IR referencia modelos/campos
  por nome lógico; a resolução continua a ser o único sítio onde uma `string`
  vira identificador, agora no servidor via `registry::resolve`. O cliente não
  tem vocabulário para nomear uma tabela/coluna física.
- **I2 (nenhum valor é formatado no SQL):** os valores viajam no IR como `Value`
  tipado — um `oneof` escalar — nunca como texto SQL. No servidor continuam a
  ser ligados como `$n` pelo mesmo `Binder` (`compiler.rs:150-172`).
- **I3 (o compilador não faz I/O):** a conversão IR→`Plan` é pura; o compilador
  permanece intocado. O contrato de ouro ganha um irmão — além de
  `sql_snapshots.rs`, passam a existir *snapshots* de IR→`Plan`→`Sql`, que fixam
  que uma mensagem `Query` conhecida compila sempre no mesmo texto e na mesma
  ordem de parâmetros.

## Consequences

- **Prós:** existe pela primeira vez um contrato de consulta que atravessa um
  processo, versionado e verificável. É o alicerce do Data Plane gRPC (ADR 0012)
  e dos SDKs poliglotas (gerados do mesmo `.proto`). O `.proto` torna-se a fonte
  única: `buf lint`/`buf breaking` podem ser portões de CI, à imagem do que os
  *snapshots* SQL já fazem para o núcleo.
- **Contras / custo:** acrescenta `prost`/`tonic`/`buf` à árvore (hoje ausentes,
  discovery `00-discovery.md:44`) e um crate `dorm-ir` com a sua conversão. O IR
  tem de acompanhar a evolução do `Plan`; um campo novo no `Plan` exige um campo
  novo no `.proto` sob as regras de compatibilidade do `buf`.
- **Testes necessários:** *snapshots* IR→`Plan` (uma `Query` conhecida vira o
  `Plan` esperado); *round-trip* `Plan`→IR→`Plan` idempotente para os `Plan` já
  cobertos por `sql_snapshots.rs`; testes negativos de validação (nome lógico
  inexistente → erro, não SQL); confirmação por teste de que não existe caminho
  de entrada para `Sql.text`.
- **Dependências:** base dos ADRs 0012 (Data Plane), 0013 (objectos remotos) e
  do catálogo do ADR 0011 (o IR nomeia o que o descritor declara).
