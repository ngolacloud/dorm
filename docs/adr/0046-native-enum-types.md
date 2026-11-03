# ADR 0046 — Tipos enum nativos (L5-enum)

Status: Accepted — FieldType + DDL + binding/leitura implementados (Fase 7) · 2026-11-03

## Context

Uma coluna de estado — `order_status` ∈ {pending, paid, shipped} — era, no DORM,
um `String` sem qualquer imposição: um valor fora do conjunto entrava na base de
dados. O PostgreSQL tem enums nativos (`CREATE TYPE … AS ENUM (…)`) que impõem o
conjunto de rótulos no próprio servidor; o DORM não os sabia declarar.

Medição vs. o ponto de partida:
- `FieldType` (`crates/dorm-core/src/value.rs`) não tinha variante de enum.
- `crates/dorm-migrations/src/ddl.rs` nunca emitia `CREATE TYPE`.
- Um `String` ligado a uma coluna enum falhava mesmo que a coluna existisse — o
  `ToSql`/`FromSql` do `String` do `tokio-postgres` **recusa** um tipo enum
  personalizado (não é um dos OIDs embutidos que conhece).

## Decision

Um enum é uma coluna **de valor-texto** com um **tipo nativo** só na camada de
DDL — `Value` não ganha variante nenhuma:

- **`FieldType::Enum(&'static str)`** — a variante leva apenas o **nome** do tipo
  SQL (um `&'static str`, mantendo `FieldType: Copy`); `sql_type()` devolve esse
  nome. As variantes vivem num **slice de inventory `EnumMeta { type_name,
  variants }`** (a disciplina de `IndexMeta`/`VersionMeta` — [[0044]]/[[0023]]),
  resolúveis por `meta::enum_variants(name)`.
- **Derive:** `#[field(enum_type = "order_status", enum_variants = "pending, paid,
  shipped")]` num campo `String` fixa `ty = Enum("order_status")` e emite um
  `EnumMeta`. Exige ambas as metades (nome + variantes); o nome é validado como
  identificador `lower_snake_case`.
- **DDL:** `create_enum_types(models, backend)` emite, por tipo distinto, um
  `DO $$ … IF NOT EXISTS (SELECT 1 FROM pg_type …) THEN CREATE TYPE … $$`
  idempotente (o PostgreSQL não tem `CREATE TYPE IF NOT EXISTS`); `schema_for_on`
  e o `sync` emitem os tipos **antes** das tabelas. SQLite/MySQL guardam a coluna
  como `TEXT` (sem tipo nativo portável).
- **Binding/leitura:** como o `String` do driver recusa o tipo enum, o DORM
  escreve o rótulo como bytes UTF-8 quando o parâmetro é `Kind::Enum` (o formato
  de wire de um enum é exactamente isso) e lê-o de volta por um pequeno wrapper
  `PgEnumLabel: FromSql` que aceita `Kind::Enum`. O `Decode` do `String` tenta o
  caminho normal e cai neste em falha, pelo que uma coluna enum tipada como
  `String` decodifica sem o modelo saber que é um enum. A base de dados continua
  a validar o rótulo contra as variantes.

É aditivo: um modelo sem campos enum compila e gera exactamente como antes.

## Consequences

- **Prós:** o conjunto de rótulos é imposto pelo servidor; um valor fora do
  conjunto é um erro (`22P02`, "invalid input value for enum …"), não um dado
  silenciosamente aceite. O valor continua a ser um `String` em Rust — sem tipo
  gerado, sem variante nova em `Value`, sem toque no `ModelMeta`.
- **Por fazer (declarado, honesto):**
  - **Bloco `enum` de topo no `dorm.schema`** (estilo Prisma `enum OrderStatus {
    … }` + campo `status OrderStatus`) — é uma feature de parser à parte
    (declaração de topo + resolução de tipo do campo); por agora os enums
    alcançam-se pelo atributo do derive. Follow-up.
  - **ALTER TYPE … ADD VALUE** (acrescentar um rótulo a um enum existente) e a
    emissão de `CREATE TYPE` nos ficheiros de migração versionada (autodetector) —
    o `sync` cria o tipo, o `migrate` versionado ainda não. Mesma família do
    adiamento de índice composto incremental ([[0044]]).
  - **MySQL `ENUM(...)` nativo** — guardamos `TEXT` em MySQL/SQLite por agora.

## Proof

- **Unit (offline, `enum_types_live.rs::the_enum_type_resolves_and_the_ddl_creates_and_uses_it`):**
  o campo é `FieldType::Enum("order_status")`, `sql_type()` é `"order_status"`,
  `meta::enum_variants` devolve `["pending","paid","shipped"]`,
  `create_enum_types` produz o `CREATE TYPE order_status AS ENUM ('pending',
  'paid', 'shipped')` e `create_table` usa a coluna `status order_status`.
- **Live (`enum_types_live.rs::the_database_enforces_the_enum_labels`, PostgreSQL
  16):** `sync` cria o tipo (antes da tabela) e a tabela; `'paid'` insere e relê
  como `String`; `'bogus'` é recusado com um erro da base que nomeia
  `order_status` ("invalid input value"); um segundo `sync` nada muda
  (idempotente). Confirmado por `psql`:
  `enum_range(NULL::order_status) = {pending,paid,shipped}`.
