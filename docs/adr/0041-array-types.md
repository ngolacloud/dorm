# ADR 0041 — Colunas de array (`TEXT[]`/`BIGINT[]`) e lookups de conjunto (L5-arrays)

Status: Accepted — implementado (Fase 7) · 2026-11-03

## Context

Um control plane guarda listas — tags, conjuntos de ids — e pergunta por conjunto
("que recursos têm a tag X?"). O `Value` dizia explicitamente que não tinha array
(`value.rs:88`), e os lookups de conjunto não existiam. O pedido (L5) e o ADR 0025
pediam a reversão dessa decisão **com justificação**: os operadores `@>`/`&&`/`<@`
precisam de **um único parâmetro array**, que o `IN ($1,$2,…)` não exprime.

## Decision

Dois tipos de array **concretos** — `TextArray` (`TEXT[]`) e `IntArray`
(`BIGINT[]`) — e não um `Array(elem)` genérico. Porquê concretos: `FieldType` é
`Copy` e `sql_type()` devolve `&'static str`; um elemento recursivo quebraria
ambos. `TEXT[]`/`BIGINT[]` cobrem os casos reais (tags, listas de ids) e ligam
nativamente como `Vec<String>`/`Vec<i64>` (um só parâmetro array, sem o risco de
inferência do ADR 0037, porque o tipo é o nativo).

- **`FieldType::TextArray`/`IntArray`** (`sql_type` → `TEXT[]`/`BIGINT[]`) e
  **`Value::TextArray(Vec<String>)`/`IntArray(Vec<i64>)`**, com os cinco braços,
  `ToValue`/`ColumnType` para `Vec<String>`/`Vec<i64>`, e inferência no derive
  (`Vec<String>`→`TextArray`, `Vec<i64>`→`IntArray`, `Vec<u8>`→`Bytes`).
- **Lookups de conjunto** `array_contains` (`@>`), `array_contained_by` (`<@`),
  `array_overlap` (`&&`), só numa coluna de array; o valor é o próprio array,
  ligado como um parâmetro nativo.
- **Recusa explícita** em SQLite/MySQL (leitura devolve erro; DDL → `TEXT`).

## Consequences

- **Prós:** listas nativas com consultas de conjunto, um parâmetro por array.
- **Contras / custo:** só `TEXT[]`/`BIGINT[]` (não arbitrário); arrays de outros
  tipos ou multi-dimensionais ficam para um `Array(elem)` futuro (exigiria
  `sql_type`→`String` e perder `Copy`). PostgreSQL-only.

## Proof

- **Unit (offline):** `array_contains` emite `@>` e liga o array como um parâmetro;
  `FieldType::TextArray` → `TEXT[]`.
- **Live (`dorm-models/tests/array_types_live.rs`):** round-trip de `tags TEXT[]`;
  `@>` {rust,db} acha 1; `&&` {python,orm} acha 2; `<@` acha 1; um lookup de array
  numa coluna `bigint` é recusado. `make check` verde.
