# ADR 0040 — Tipo de rede `inet` e lookups de contenção (L5-rede)

Status: Accepted — `inet` implementado (Fase 7) · 2026-11-03

## Context

Um control plane faz IPAM: guardar endereços e perguntar contenção de rede ("que
hosts estão em 10.0.0.0/8?"). O conjunto de tipos do DORM não tinha nada de rede
(medição L5). Adicionar um tipo de coluna novo toca em ~9 `match` exaustivos
(value/row/ddl/autodetect/sqlite/studio/mcp/derive), por isso foi deixado para um
passo próprio.

## Decision

Primeiro o `inet`, que é **dep-free**: o `postgres-types` mapeia
`std::net::IpAddr` ↔ `INET` nativamente.

- **`FieldType::Inet`** (`sql_type` → `INET`) e **`Value::Inet(std::net::IpAddr)`**,
  com os cinco braços (sql_type, describe, to_sql via `to_sql_checked`,
  read_column, coerce `Text→Inet`), `ToValue`/`ColumnType` para `IpAddr`, e a
  inferência `IpAddr → Inet` no derive. Round-trip nativo em PostgreSQL.
- **Lookups de rede** `net_contains` (`>>`), `net_contained_by` (`<<`),
  `net_overlaps` (`&&`), só válidos numa coluna `Inet` (recusados noutra). O valor
  (um IP ou **CIDR** com máscara, p. ex. `10.0.0.0/8`) é ligado como **texto** e
  cast com `($n || '')::inet` — o padrão à prova da armadilha de inferência de
  tipo (ADR 0037); um `Value::Inet` nativo liga sem cast. O valor de rede **não** é
  coerçido para `IpAddr` (perderia a máscara).
- **Recusa explícita** nos backends sem `inet`: a leitura tipada e dinâmica em
  SQLite/MySQL devolve erro ("inet is a PostgreSQL-only type"); a DDL mapeia a
  coluna a `TEXT` nesses backends (armazena a forma textual, mas os operadores de
  rede são PostgreSQL-only).

## Consequences

- **Prós:** IPAM nativo — endereços guardados como `inet`, consultas de contenção
  (`<<`/`>>`/`&&`) com CIDRs; zero dependências novas; a armadilha de inferência
  fica fechada pelo ADR 0037 e pelo cast de texto.
- **Por fazer (declarado):** `cidr` (rede com máscara numa coluna, via a crate
  `cidr`), `macaddr` (via `eui48`), arrays e enums nativos, e PK composta — cada
  um exige uma dependência nova (feature) ou mais cirurgia no núcleo, e fica para
  passos próprios. Ler uma coluna `inet` **com máscara** de volta precisa também do
  tipo `cidr` (o `IpAddr` só guarda o host).

## Proof

- **Unit (`network_types_live::a_network_lookup_…`, offline):** o lookup emite
  `<<` e `($n || '')::inet`, com o CIDR ligado como texto; `FieldType::Inet` →
  `INET`.
- **Live (`dorm-models/tests/network_types_live.rs`):** quatro hosts com coluna
  `INET`; round-trip de um `IpAddr`; `net_contained_by 10.0.0.0/8` acha os 2 hosts
  `10.x`, `10.99.0.0/16` acha 1, `net_overlaps 192.168.0.0/16` acha 1; um lookup de
  rede numa coluna `bigint` é recusado antes de qualquer SQL. `make check` verde.
