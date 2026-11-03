# ADR 0048 — Tipos `CIDR` e `MACADDR` (L5-rede, parte 2)

Status: Accepted — FieldType + binding/leitura + lookups implementados (Fase 7) · 2026-11-03

## Context

O [[0040]] deu o `INET` (host), dep-free, porque o `std::net::IpAddr` já o mapeia
no `postgres-types`. Faltava o par IPAM: a **sub-rede** (`CIDR`, `10.0.0.0/24`) e
o **endereço físico** (`MACADDR`, `08:00:2b:01:02:03`). Ao contrário do `INET`,
estes não têm tipo Rust na biblioteca-padrão — precisam das crates `cidr` e
`eui48`, que o `postgres-types` sabe ligar ao fio através das features
`with-cidr-0_3`/`with-eui48-1`. Por isso tinham ficado adiados.

## Decision

Variantes nativas, o mesmo padrão do `INET` (nada de texto-com-cast na escrita):

- **Deps:** `cidr = "0.3"`, `eui48 = "1"`; `tokio-postgres` ganha as features
  `with-cidr-0_3` e `with-eui48-1` (reencaminhadas para `postgres-types`).
- **`FieldType::Cidr`/`FieldType::MacAddr`** (unitárias → `FieldType` continua
  `Copy`); `sql_type()` = `"CIDR"`/`"MACADDR"`.
- **`Value::Cidr(cidr::IpCidr)`/`Value::MacAddr(eui48::MacAddress)`**: ligam e lêem
  nativo (binário), via `to_sql_checked`/`FromSql` das crates. O derive infere
  `Cidr`/`MacAddr` de um campo `IpCidr`/`MacAddress` (e aceita
  `#[field(column_type = "cidr" | "macaddr")]`).
- **Inferência cidr↔inet:** um parâmetro nu ao lado de uma coluna `cidr`
  (`cidr_col = $1`) é inferido pelo PostgreSQL como `inet` (o `cidr` é
  implicitamente coercível para `inet`) — verificado com `pg_prepared_statements`.
  Como `cidr` e `inet` partilham formato de fio, o `ToSql` do `Value::Cidr`, quando
  o tipo inferido é `INET`, serializa a mesma rede como `IpInet`; a comparação
  continua igual. Assim `.eq()` numa coluna `cidr` funciona sem o chamador ter de
  converter.
- **Lookups de rede:** o guarda de tipo de `net_contains`/`net_contained_by`/
  `net_overlaps` passa a aceitar colunas `inet` **e** `cidr` (os operadores
  `>>`/`<<`/`&&` abrangem ambas); o valor continua ligado como texto e convertido
  com `($n || '')::inet` ([[0040]]).
- **SQLite/MySQL:** sem `CIDR`/`MACADDR` nativos — a coluna é `TEXT` e uma leitura
  tipada é recusada com mensagem clara (como o `INET`).

Adicionar as duas variantes tocou os ~9 matches exaustivos de sempre
(value/row×3/ddl/autodetect/sqlite/studio×2/mcp×2/derive), todos guiados pelo
compilador.

## Consequences

- **Prós:** sub-redes e endereços MAC são colunas nativas, impostas e indexáveis
  pelo PostgreSQL; a família IPAM (`inet`+`cidr`) partilha os mesmos lookups de
  containment. `MACADDR` é igualdade/ordenação nativas.
- **Contras/decisões:** duas deps novas (`cidr`, `eui48`) — aceitáveis porque são
  pequenas, sem `build.rs`, e só compiladas por quem usa estes tipos; o `INET`
  continua dep-free. `MACADDR8` (EUI-64) e `macaddr` com lookups específicos ficam
  de fora (sem operadores de containment em MAC).
- **Honesto:** a igualdade `cidr` passa pela serialização-como-inet acima; se um
  dia o PostgreSQL inferir o parâmetro como `cidr` (não acontece hoje), o caminho
  `*ty == INET` simplesmente não dispara e usa-se o `cidr` nativo.

## Proof

- **Unit (offline, `cidr_macaddr_live.rs::the_sql_types_are_cidr_and_macaddr_and_the_ddl_uses_them`):**
  `sql_type()` é `"CIDR"`/`"MACADDR"` e o DDL gera `block CIDR` / `mac MACADDR`.
- **Live (`cidr_macaddr_live.rs`, PostgreSQL 16):** a tabela cria-se com colunas
  `cidr`/`macaddr` (confirmado por `information_schema`); três linhas inserem;
  `.eq()` numa coluna `cidr` e numa `macaddr` devolvem a linha certa (round-trip
  dos valores nativos); `net_contains` numa coluna `cidr` encontra só o bloco que
  contém `10.0.0.5`. `pg_prepared_statements` confirmou a inferência
  `cidr_col = $1 → inet`, `macaddr_col = $1 → macaddr`.
