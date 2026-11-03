# ADR 0025 — Tipos de um control plane (rede, enum, array, JSONB, PK composta)

Status: Proposed · 2026-11-03 — **brick JSONB implementado (Fase 3)**

## Implementação — lookups JSONB por caminho (Fase 3)

Primeiro brick entregue, o de maior valor imediato e **sem variantes novas** de
`Value`/`FieldType`:

- `registry::resolve` ganha `json_path: Vec<String>` no `Resolved`: quando a cabeça
  do caminho é um campo `Json` e há segmentos a seguir, estes são o caminho JSON
  (o último é o *lookup* se o nomear), em vez de uma relação a atravessar.
- `compiler::compile_json_predicate` emite
  `(jsonb_extract_path_text(col, $1, $2))::<tipo> OP $n` — **cada segmento do
  caminho ligado como parâmetro** (nunca interpolado, invariante I2). `spec__cpu__gte`
  → `(jsonb_extract_path_text(spec, $1))::bigint >= $2`.
- **Armadilha resolvida (bug latente real):** o *cast* tem de ser o tipo **nativo**
  do valor (`i64`→`::bigint`, não `::numeric`). O `ToSql` do `Value` escreve os bytes
  do tipo nativo mesmo quando o servidor infere outro tipo do operando; um `::numeric`
  sobre um `i64` fazia o servidor esperar `numeric` e o cliente escrever bytes `int8`
  → valor-lixo → comparação sempre verdadeira. Provado live (sem o fix dava 3 em vez
  de 2).
- Só PostgreSQL; outro backend devolve erro de capacidade explícito.
- **Prova:** `dorm-models/tests/jsonb_live.rs` — forma offline (caminho ligado,
  cast nativo, segmento fora do texto) + live (numérico `cpu__gte`, texto `zone`,
  aninhado `limits__mem__gte`, combinado). `make check` verde.

**Por fazer (restantes bricks de L5):** rede (inet/cidr/macaddr)+lookups, arrays,
enums nativos, PK composta e `@@unique`/`@@index` do `dorm.schema` — cada um precisa
de variantes novas de `Value`/`FieldType` (+dep) ou de campos novos no `ModelMeta`,
e fica para fases próprias e testadas.

---


## Context

O conjunto de tipos serve um ORM genérico mas não um control plane de cloud.
Estado medido no `main`:

- **`FieldType`** tem 10 variantes (`crates/dorm-core/src/value.rs:27-41`:
  `I32,I64,F64,Text,Bool,Decimal,Timestamp,Uuid,Json,Bytes`) e **`Value`** 11. O
  conjunto é deliberadamente fechado (`value.rs:21-25`).
- **Sem tipos de rede.** `inet`/`cidr`/`macaddr` não existem em nenhuma camada
  (`grep` vazio) — a IPAM de um control plane depende deles e dos lookups
  `contains`/`contained_by`/`overlaps`, que o enum `Lookup` (`lookup.rs:14-30`)
  não tem (`Contains` ali é LIKE textual, `compiler.rs:416-421`).
- **Sem arrays.** Decisão explícita: "There is no `Array` variant" — `in` expande
  para `IN ($1,$2,…)` (`value.rs:88-92`), logo não há `= ANY($1)` tipado nem `@>`
  / `&&`.
- **Sem lookups em caminhos JSONB.** `JSONB` existe como tipo de coluna
  (`value.rs:58`) mas o split por `__` resolve segmentos como **relações**
  (`registry.rs:211`, `compiler.rs:341`): um `spec__cpu__gte` seria lido como a
  relação `spec` → coluna `cpu`, não um caminho JSON.
- **`enum`, PK composta e `@@unique`/`@@index` do schema estão em "Not yet".** O
  `dorm.schema` parseia `@@unique`/`@@index` mas **ignora-os** (`BlockAttr::Ignored`,
  `parser.rs:496-503`; teste `:545-549`); `docs/schema.md:76-79` lista "Composite
  primary keys, `@@unique`/`@@index` emission, … enums" como roadmap. O `CHECK` e o
  `unique_together` existem na camada Rust/DDL (`meta.rs:109-118`, `ddl.rs:73,85`)
  mas não são alcançáveis a partir do `dorm.schema`. O `Scalar` do schema
  (`lib.rs:168-177`) nem sequer tem `F64`/`Bytes`.

## Decision

Alargar o sistema de tipos por grupos, cada tipo novo com os cinco braços
(`sql_type`, `to_sql`, `read_column`, `coerce`, `describe`) e teste de ida-e-volta
em PostgreSQL + recusa explícita nos backends que o não suportam. **Esta ADR é a
justificação, com bump de versão, para as novas variantes de `FieldType`/`Value`**
(o invariante exige-a):

- **Rede:** `Inet`/`Cidr`/`Macaddr`, com os lookups de conjunto `contains` (`>>`),
  `contained_by` (`<<`) e `overlaps` como variantes novas de `Lookup`.
- **Arrays:** uma variante `Array(elem)` em `Value`/`FieldType` — revertendo a
  decisão de `value.rs:88` **com justificação**: os lookups de conjunto
  (`contains` `@>`, `overlap` `&&`) e o `= ANY($1)` tipado precisam de um único
  parâmetro array, que `IN ($1,$2,…)` não exprime. O `in` escalar mantém-se.
- **Enums:** declarados no `dorm.schema` (bloco `enum`), gerados como **enum nativo
  PostgreSQL** (com `ALTER TYPE … ADD VALUE` rotulado fora-de-transacção, ADR 0022)
  ou como `CHECK` conforme um atributo — reutilizando `ModelMeta.checks` já
  existente para o caminho `CHECK`.
- **Lookups em JSONB por caminho.** Quando o registry resolve a cabeça do caminho a
  um campo `Json`, os segmentos restantes deixam de ser tentados como relações e
  passam a extracção JSON (`#>`/`->>`) com o lookup terminal aplicado ao valor
  extraído (`spec__cpu__gte` → `(spec #>> '{cpu}')::numeric >= $1`). O registry
  continua a ser a única autoridade sobre o que a cabeça nomeia.
- **PK composta e `@@unique`/`@@index` do schema.** O parser do `dorm.schema` passa
  a **modelar** (não ignorar) `@@id([a,b])`, `@@unique([…])` e `@@index([…])`,
  emitindo-os via o `unique_together`/índices que o DDL já sabe gerar; a PK composta
  liga-se à identidade de linha (hoje `UnsupportedChange`, `autodetect.rs:570-572`).

### Como preserva os invariantes do `ARCHITECTURE.md`

- **I1/I2:** cada tipo novo é uma coluna/valor resolvido pelo registry e ligado
  como `$n`; os operadores de rede/array/JSON são lookups fechados que o compilador
  emite, não texto do chamador. O caminho JSONB usa o `#>> '{…}'` sobre segmentos
  validados contra o campo `Json`, com o valor de comparação ligado.
- **Fecho preservado:** o conjunto continua fechado — cada adição é uma variante
  enumerada com os cinco braços, nunca `ToSql` arbitrário; o autodetector continua
  a saber nomear o tipo de cada coluna.
- **I3:** o compilador permanece puro; os novos lookups/tipos entram no contrato de
  *snapshot*.

## Consequences

- **Prós:** DORM torna-se usável para schemas reais de control plane (IPAM por
  `inet`/`cidr`, `spec` em JSONB, `tags` array, estados por enum, chaves naturais
  compostas) sem cair em SQL cru que não compõe com o `QuerySet`.
- **Contras / custo:** mais variantes = mais braços a manter e um bump de versão do
  contrato de `Value`/`FieldType`; o SQLite recusa explicitamente os tipos que não
  suporta (rede, array, enum nativo) em vez de degradar em silêncio — ao contrário
  do mapeamento textual actual de Decimal/Uuid/Json (`row.rs:20`), que se mantém
  para os tipos já existentes.
- **Dependências:** `ALTER TYPE … ADD VALUE` fora-de-transacção do ADR 0022 para
  enums nativos.

## Proof esperada

- **Unit/snapshot:** ida-e-volta de `inet`/`cidr`/`macaddr`/array/enum em
  PostgreSQL; `host__contained_by($cidr)` compila `host << $1`;
  `tags__overlap($arr)` compila `tags && $1`; `spec__cpu__gte($n)` compila a
  extracção JSONB; `@@id([a,b])`/`@@unique([…])` do `dorm.schema` geram o DDL
  esperado (contra o oráculo `schema_parity.rs`).
- **Recusa:** cada tipo não suportado em SQLite devolve um erro de capacidade
  explícito, provado por teste — nunca uma coluna silenciosamente textual.
- **Live (`DELONIX_ORM_TEST_DSN`):** uma tabela com `inet`, `text[]`, um enum
  nativo e uma PK composta grava e lê de volta cada tipo; um lookup de rede
  (`contained_by`) e um de array (`overlap`) devolvem as linhas certas.
