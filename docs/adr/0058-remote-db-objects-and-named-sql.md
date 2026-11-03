# ADR 0058 — Objectos de BD remotos e allow-list de SQL nomeado

Status: **Rejected** · see [ADR 0054](0054-orm-boundary.md)

> Renumbered from ADR 0013 — a 2026-10-31 review-branch proposal that collided
> with the accepted [ADR 0013](0013-prefetch-related.md).
> **Rejected:** a remote named-SQL allow-list only exists to serve non-Rust
> clients across the wire — a data-plane concern outside the ORM (ADR 0054).
> Local advanced/raw SQL stays available to embedders via [ADR 0005](0005-raw-sql.md).

## Context

O compilador só emite CRUD/agregação a partir do `Plan`
(`crates/dorm-core/src/compiler.rs`, funções `compile_select`/`compile_insert`/
`compile_update`/`compile_delete`); não há caminho para invocar *procedures* ou
funções de base de dados (achado GW-05). O único SQL cru existe no caminho
**local** (`Manager::raw`/`db::sql`, ADR 0005), sempre por **posição**
(`crates/dorm-core/src/model.rs:27-29`). Não existe qualquer registo de
*statements* nomeados nem allow-list versionada.

Num Data Plane poliglota (ADR 0057), abrir SQL cru ao cliente violaria o
invariante nuclear — "No identifier comes from a caller" (`ARCHITECTURE.md:56`).
Sem um mecanismo de objectos declarados e de *statements* nomeados registados, ou
se fecha a porta a SQL avançado, ou se abre um buraco de injecção.

## Decision

Dar acesso remoto a SQL avançado **sem** aceitar texto livre do cliente, por duas
vias, ambas declaradas no servidor e versionadas como migração:

1. **Procedures e funções declaradas no esquema → RPCs tipados.** Uma *procedure*
   ou função declarada em `dorm.schema` entra no `SchemaDescriptor` (ADR 0056)
   com entrada e saída tipadas e torna-se um RPC gerado no serviço do ADR 0057. O
   cliente invoca pelo nome lógico do objecto com parâmetros tipados; não escreve
   `CALL`/`SELECT fn(...)` nenhum.
2. **SQL cru remoto só por *statements* nomeados registados numa allow-list
   versionada.** O texto parametrizado vive no servidor (declarado no esquema,
   revisto como migração, com placeholders `$n`), nunca no pedido. O cliente
   invoca pelo **nome** do *statement* mais os parâmetros tipados; o servidor
   procura o texto na allow-list e liga os parâmetros. Texto livre do cliente é
   recusado — não há RPC que o aceite.

Esta é a extensão natural do invariante I1 a *procedures* e SQL avançado: o
cliente nomeia um objecto/statement registado; o texto é dado do servidor, não do
pedido.

### Como preserva os invariantes do `ARCHITECTURE.md`

- **I1 (nenhum identificador vem do cliente):** o cliente nomeia um
  objecto/statement que o descritor/allow-list declara, não emite SQL. O texto e
  os identificadores vivem no servidor (esquema/allow-list), revistos como
  migração. Um nome não registado é recusado.
- **I2 (nenhum valor é formatado no SQL):** os parâmetros de um *statement*
  nomeado ou de um RPC de *procedure* são sempre ligados como `$n`; a allow-list
  guarda texto parametrizado, nunca interpolado.
- **I3 (o compilador não faz I/O):** a allow-list é dado estático — a mesma
  classe dos `checks` de `meta.rs:118`, que já entram "straight into the DDL" por
  virem do esquema e não do chamador. O compilador não é envolvido no caminho de
  *statement* nomeado; o `dorm-server` liga os parâmetros e executa pelo `db`.

## Consequences

- **Prós:** SQL avançado (procedures, funções, consultas que o `Plan` não
  exprime) passa a estar disponível por rede sem abrir injecção; o conjunto de
  *statements* é revisto como código e versionado, com o mesmo rigor das
  migrações; fecha a tentação de expor `Manager::raw` à rede.
- **Contras / custo:** registar e versionar *statements* nomeados é mais cerimónia
  do que escrever SQL ad-hoc; um *statement* que precise de mudar exige uma
  migração; o servidor passa a guardar e a resolver uma allow-list.
- **Testes necessários:** invocação de uma *procedure* declarada por RPC tipado
  com parâmetros ligados; invocação de um *statement* nomeado por nome +
  parâmetros; recusa de um nome não registado; prova de que não existe RPC que
  aceite texto SQL livre do cliente; *round-trip* de versão (editar um *statement*
  é uma migração, não uma edição silenciosa).
- **Dependências:** assenta no ADR 0057 (Data Plane) e estende o descritor do
  ADR 0056; o SQL cru local (ADR 0005) mantém-se inalterado para embedders.
