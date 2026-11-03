# ADR 0044 — Índices compostos (`@@index([a, b])`) (L5-composite-index)

Status: Accepted — slice + DDL + emissão de schema implementados (Fase 7) · 2026-11-03

## Context

Um índice de várias colunas — `(tenant, created_at)` para "as linhas mais recentes
de um inquilino" — é o caminho de acesso mais comum numa tabela multi-inquilino.
O DORM só sabia índices de UMA coluna (`#[field(index)]` / `@index`); o
`@@index([a, b])` do `dorm.schema` era parseado e **deitado fora**
(`BlockAttr::Ignored`), listado em "Not yet" na `schema.md` ([[0043]]), porque um
índice composto não tinha representação em metadados.

Medição vs. o ponto de partida (antes desta mudança):
- `crates/dorm-migrations/src/ddl.rs` `indexes()`/`indexes_concurrent()` filtravam
  só `f.index` por coluna — nenhum índice de várias colunas.
- `crates/dorm-schema/src/parser.rs` `block_attr()` só modelava `@@unique`/`@@id`
  ([[0043]]); `@@index` caía no ramo `Ignored`.

## Decision

Um índice composto é levado por um **slice de inventory** `IndexMeta`, não por um
campo novo em `ModelMeta` — a mesma disciplina de `VersionMeta`/`SoftDeleteMeta`
([[0023]]/[[0033]]) e da `RagMeta` do `dorm-rag`:

- **`dorm-core` (`meta.rs`):** novo `pub struct IndexMeta { model, columns }` +
  `inventory::collect!(IndexMeta)`; `ModelMeta::composite_indexes()` resolve os
  grupos deste modelo pelo slice. `ModelMeta`/`FieldMeta` ficam **inalterados** —
  o compilador puro nunca vê isto (um índice só interessa ao migrador), e os
  ~30 construtores literais de `ModelMeta` espalhados pelo workspace não são
  tocados.
- **`dorm-derive`:** nova opção `#[model(index_together = "a, b")]` (repetível,
  um grupo por ocorrência, igual ao `unique_together`); cada grupo emite um
  `inventory::submit! { IndexMeta { .. } }`.
- **`dorm-migrations` (`ddl.rs`):** `index_on`/`index_on_concurrent`
  generalizam-se a várias colunas (`index_on_cols`/`index_on_cols_concurrent`),
  com um único sítio a soletrar o nome (`index_name` → `<table>_<c1>_<c2>_idx`);
  `indexes()`/`indexes_concurrent()` anexam um `CREATE INDEX` por grupo
  **depois** dos por-coluna. Assim `migrate`/`sync` constroem-no `CONCURRENTLY`
  e idempotente (`IF NOT EXISTS`), e uma tabela nova leva-o no `create_table`.
- **`dorm-schema`:** `BlockAttr::Index(Vec<String>)` + `Model::indexes`; o parser
  reutiliza o `ident_list()` de [[0043]]; o codegen emite um
  `index_together = "a, b"` por grupo (colunas em snake_case para casarem com as
  colunas geradas); a validação recusa um `@@index` que nomeie uma coluna
  inexistente.

É puramente aditivo (slice novo, sem mudança em `ModelMeta`): um modelo sem
`@@index`/`index_together` gera e compila exactamente como antes.

## Consequences

- **Prós:** um índice composto declara-se no `dorm.schema` (`@@index([tenant,
  created_at])`) ou na struct (`#[model(index_together = "tenant, created_at")]`)
  e é construído por `migrate`/`sync`, sem bloquear escritas (CONCURRENTLY no
  PostgreSQL). `ModelMeta` não cresce.
- **Por fazer (declarado, honesto):** o **autodetector versionado** é centrado-na-
  coluna — compara o `index` por `FieldMeta` e emite add/drop de índice de coluna
  única como statement próprio. Um índice composto **acrescentado a uma tabela já
  existente** é construído pela via `sync`/`create_table` (idempotente), mas NÃO é
  emitido como um passo de migração incremental versionada (nem o seu DROP), porque
  isso exigiria rastrear estado de índice ao nível da tabela no diff — um passo com
  ADR própria. Índices parciais/com expressão/`USING <method>` também ficam de
  fora (só B-tree sobre colunas, por agora).

## Proof

- **Unit (offline):**
  - `dorm-core` `meta::tests::composite_indexes_resolve_through_the_slice_and_are_empty_otherwise`
    — dois `IndexMeta` no slice resolvem para `[[tenant, created_at], [status]]`;
    um modelo sem submissão tem zero.
  - `dorm-migrations` `ddl::tests::a_composite_index_names_and_spans_all_its_columns`
    e `indexes_include_the_composite_groups_after_the_per_field_ones` — o nome é
    uma só regra partilhada com o caso de uma coluna; `indexes()` anexa o
    `CREATE INDEX` composto depois dos por-coluna.
  - `dorm-schema` `codegen::tests::composite_indexes_reach_the_derive` — `@@index`
    parseia para `Model::indexes` e gera `index_together = "tenant, created_at"`
    (snake_case); `no_block_attrs_means_no_change` garante zero regressão.
- **Live (`dorm-models/tests/composite_index_live.rs`, PostgreSQL 16):**
  `sync` cria a tabela `l5_events` e constrói o índice composto `CONCURRENTLY`;
  `pg_indexes.indexdef` mostra `(tenant, created_at)`, `pg_index.indisvalid` é
  verdadeiro (build concorrente bem-sucedido), e um segundo `sync` é no-op
  (idempotente). Confirmado também por `\d l5_events`:
  `l5_events_tenant_created_at_idx btree (tenant, created_at)`.
