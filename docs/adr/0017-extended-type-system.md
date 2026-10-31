# ADR 0017 — Sistema de tipos alargado

Status: Proposed · 2026-10-31

## Context

O conjunto de tipos é estreito (achado DBA-04, H5 confirmada): `FieldType` tem
exactamente 8 variantes — `I32, I64, Text, Bool, Decimal, Timestamp, Uuid, Json`
(`crates/dorm-core/src/value.rs:27-35`) — e `Value` espelha as mesmas 8
(`value.rs:85-94`). O tipo é deliberadamente **fechado** (`value.rs:23-25`): um
tipo novo exige variante em `FieldType` **e** em `Value`, mais um braço em
`to_sql` (`value.rs:196-209`), `read_column` (`value.rs:331-364`), `coerce`
(`value.rs:133-168`) e `sql_type` (`value.rs:40-54`).

Isto torna DORM inutilizável para esquemas reais que precisem de ponto flutuante
(`REAL`/`DOUBLE PRECISION`), datas/horas puras (`DATE`/`TIME`/`INTERVAL`), binário
(`BYTEA`), inteiros pequenos (`SMALLINT`/`i16`), rede (`INET`/`CIDR`), `enum`s
nativos, arrays, `NUMERIC(p,s)` com precisão explícita (hoje `NUMERIC` nu,
`value.rs:46-49`), e extensões (PostGIS, pgvector). O escape por SQL cru (ADR
0005) devolve linhas mas `read_column` não tem braço → `UnexpectedColumnType`. O
ADR 0004 já nomeia "the wider `Value` set (`f64`, `bytea`, `date`/`time`, arrays)"
como follow-up (`0004-production-connections.md:82`).

## Decision

Alargar `FieldType` e `Value` com os tipos de produção, por grupos de prioridade
crescente de complexidade de descodificação:

1. **Escalares sem ambiguidade:** `F32`/`F64` (`REAL`/`DOUBLE PRECISION`),
   `I16` (`SMALLINT`), `Bytes` (`BYTEA`).
2. **Data/hora:** `Date`/`Time`/`Interval` (chrono `NaiveDate`/`NaiveTime`/
   `Duration`).
3. **Precisão explícita:** `NUMERIC(p, s)` via atributo de precisão/escala
   (reutilizando `max_length`, que já existe).
4. **Compostos e rede:** arrays (ligados ao `= ANY($1)` tipado do eixo
   performance), `INET`/`CIDR`.
5. **`enum`s nativos** (coordenado com o tijolo de tipos/enums do eixo migrações).
6. **Ponto de extensão** para PostGIS (`geometry`) e pgvector (`vector`).

Cada tipo novo é uma variante em `FieldType`+`Value` com os cinco braços
(`to_sql`, `read_column`, `coerce`, `sql_type`), **mapeado por dialecto**:
PostgreSQL usa o tipo nativo (`value.rs:40-54`); SQLite colapsa no seu conjunto
restrito (`crates/dorm-migrations/src/ddl.rs:249-256`).

**Honestidade de capacidade (a regra do roadmap, igual ao `verify-ca` do ADR
0004):** um backend sem a capacidade devolve um **erro explícito**, nunca um
resultado errado nem um silêncio. Um `INET` ou um array num SQLite que não o
suporte é um erro nomeado na migração/descodificação, não uma coerção aproximada.

### Como preserva os invariantes do `ARCHITECTURE.md`

- **I1 (nenhum identificador vem do cliente):** o sistema de tipos descreve
  valores e colunas, não introduz identificadores; o tipo de uma coluna continua
  a vir do `FieldMeta` que `registry::resolve` devolve.
- **I2 (nenhum valor é formatado no SQL):** cada tipo novo é ligado como `$n`
  pela sua implementação de `ToSql` (`value.rs`); nenhum valor — incluindo
  `BYTEA`, datas ou arrays — é interpolado no texto. É o alargamento directo do
  invariante I2 a mais tipos.
- **I3 (o compilador não faz I/O):** o `sql_type` por dialecto é texto puro; a
  descodificação (`read_column`) é CPU pura sobre a linha devolvida. O compilador
  permanece sem I/O e os *snapshots* ganham os tipos novos.

## Consequences

- **Prós:** DORM passa a cobrir os tipos que um DBA de produção usa quase de
  imediato; o erro explícito por backend sem capacidade mantém a regra de "nunca
  um resultado errado"; o ponto de extensão abre PostGIS/pgvector sem reescrever o
  núcleo.
- **Contras / custo:** cada tipo toca cinco sítios (`FieldType`, `Value`,
  `to_sql`, `read_column`, `coerce`, `sql_type`) mais o `dorm-derive` (atribuir um
  `FieldType` a um campo Rust) e o mapeamento por dialecto — superfície larga,
  por isso o faseamento por grupos; arrays/enums/extensões são tijolos maiores
  subsequentes.
- **Testes necessários:** *round-trip* de cada tipo novo (escrever e ler de volta
  o mesmo valor) em PostgreSQL; mapeamento de `sql_type` fixado por `schema_parity`
  por dialecto; `NUMERIC(p,s)` preserva precisão; um tipo não suportado por um
  backend devolve o erro nomeado (não coerção silenciosa); *snapshot* dos novos
  braços de `to_sql`/`read_column`.
- **Dependências:** os arrays coordenam-se com o `= ANY($1)` tipado do eixo
  performance e com o lookup `in`; os `enum`s nativos com o eixo migrações;
  independente do eixo gateway (ADRs 0010-0014).
