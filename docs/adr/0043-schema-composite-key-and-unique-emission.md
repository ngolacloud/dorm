# ADR 0043 — Emissão de `@@id`/`@@unique` a partir do `dorm.schema` (L5-schema-blocks)

Status: Accepted — parser + codegen + validação implementados (Fase 7) · 2026-11-03

## Context

O `dorm.schema` é a fonte-de-verdade declarativa que o `dorm generate` compila
para os `struct`s com `#[derive(Model)]`. Depois de o derive passar a aceitar PK
composta ([[0042]]) e `unique_together` (grupos únicos compostos), o schema ainda
não sabia declará-los: o `schema.md` listava `@@id`/`@@unique`/`@@index`
compostos em "Not yet". O parser consumia qualquer `@@`-atributo desconhecido e
**deitava-o fora** (`BlockAttr::Ignored`), por isso `@@id([region, code])` não
tinha efeito nenhum no Rust gerado — uma tabela de chave natural tinha de ser
escrita à mão com `#[field(primary_key)]` duas vezes.

## Decision

O AST e o codegen passam a modelar dois atributos de bloco; `@@index` composto
fica **deliberadamente de fora** (não tem ainda campo em `ModelMeta`).

- **Parser** (`dorm-schema/src/parser.rs`): `BlockAttr` ganha `Unique(Vec<String>)`
  e `Id(Vec<String>)`; `block_attr()` parseia a lista de identificadores
  `[a, b, …]` (novo helper `ident_list()`) quando o nome é `unique` ou `id`.
  Tudo o resto — incluindo `@@index` — continua `Ignored` (parseado e descartado,
  como antes), por isso um schema que os use não quebra.
- **AST** (`dorm-schema/src/lib.rs`): `Model` ganha
  `unique_together: Vec<Vec<String>>` (um grupo por `@@unique`) e
  `composite_id: Vec<String>` (as colunas do `@@id`, vazio quando ausente).
- **Codegen** (`dorm-schema/src/codegen.rs`): cada grupo `@@unique` vira um
  `unique_together = "a, b"` dentro do `#[model(..)]` (o derive aceita o atributo
  repetido, um grupo por ocorrência); cada coluna de `@@id` é **promovida** a
  `#[field(primary_key)]` mesmo sem `@id` inline — reutilizando exactamente o
  mecanismo de PK composta de [[0042]].
- **Validação** (`dorm-schema/src/validate.rs`): toda a coluna nomeada por
  `@@id`/`@@unique` tem de ser uma coluna real do modelo; um erro de escrita
  (`@@id([region, cdoe])`) é nomeado em vez de promover silenciosamente nada a
  chave.

Nenhuma destas mudanças toca no `dorm-core`, no `ModelMeta`, no `FieldMeta` nem
no compilador: a emissão assenta inteiramente nos atributos que o derive já
entende. Um schema sem `@@id`/`@@unique` gera byte-a-byte o que gerava antes
(opt-in).

## Consequences

- **Prós:** uma tabela de chave natural ou com restrição única composta
  declara-se no `dorm.schema` — `@@id([region, code])`,
  `@@unique([tenant, slug])` — e o `dorm generate` produz o Rust correcto; o
  mesmo DDL e a mesma imposição de [[0042]] e do `unique_together` do derive.
- **Por fazer (declarado, honesto):** `@@index` composto continua fora — precisa
  de um campo novo em `ModelMeta` (e portanto de uma ADR + bump próprios);
  `save`/`delete` por instância numa PK composta continuam recusados ([[0042]]).

## Proof

- **Unit (offline, `dorm-schema`):**
  - `codegen::tests::composite_unique_and_id_reach_the_derive` — um schema com
    `@@id([region, code])` + `@@unique([region, label])` parseia para
    `composite_id == [region, code]` e `unique_together == [[region, label]]`, e
    o Rust gerado contém `#[model(table = "region_codes", unique_together =
    "region, label")]` e `#[field(primary_key, max_length = 20)]` em ambas as
    colunas da chave, mantendo a coluna não-chave simples.
  - `codegen::tests::no_block_attrs_means_no_change` — um schema sem os atributos
    não emite `unique_together` (opt-in, sem regressão).
  - `validate::tests::a_composite_key_or_unique_naming_a_missing_column_is_rejected`
    e `a_well_formed_composite_key_validates_clean` — colunas inexistentes são
    nomeadas; um schema bem-formado valida limpo.
- **Live:** não aplicável — esta é uma mudança puramente de geração de código; o
  comportamento em PostgreSQL é o de [[0042]] (`composite_pk_live.rs`), que esta
  emissão apenas passa a produzir a partir do `dorm.schema`.
