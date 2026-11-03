# ADR 0028 — Vários schemas por processo (handle `Database` com `search_path`)

Status: Accepted — núcleo implementado (Fase 6) · 2026-11-03

## Context

A NgolaCloud dá a cada módulo o seu schema PostgreSQL (compute, billing, …) na
mesma base. O DORM suporta várias **bases** por processo (um `Database` por DSN),
mas não vários **schemas**: o handle `Database` não tinha `search_path`, e os
modelos compilam a tabela sem qualificação de schema. Um módulo não tinha como
dizer "as minhas tabelas vivem em `compute`" sem colidir com outro módulo que use
o mesmo nome de tabela.

## Decision

Ligar o schema ao **handle**, não à compilação. `Database::connect_schema(dsn,
replica, read_only, schema)` injecta `search_path = <schema>,public` como opção
libpq (`-c search_path=…`) em cada ligação do pool — exactamente o mecanismo que o
`statement_timeout` já usa. A consequência elegante: **o compilador e o
`ModelMeta` não mudam**. O mesmo modelo, cuja tabela compila para `"l11_widget"`,
resolve para `compute.l11_widget` ou `billing.l11_widget` conforme o
`search_path` do handle activo. Um processo tem um handle por módulo e escolhe-o
por unidade de trabalho com `with_session`.

- **Isolamento.** Um handle ligado a `compute` não alcança o schema de outro
  módulo pela ORM: as suas consultas resolvem só em `compute` (e `public` para
  extensões/tipos partilhados).
- **Segurança do nome.** O schema é validado como identificador simples (letras,
  dígitos, underscore) antes de entrar na string de opções — é configuração, não
  um parâmetro de consulta, e as opções libpq não aceitam valores ligados.

### Como preserva os invariantes do `ARCHITECTURE.md`

O compilador continua puro e ignorante de schema; a tabela compila na mesma. A
resolução de schema é uma propriedade da **ligação** (o `search_path`), fora do
compilador. Nenhum identificador novo vem do chamador: o nome de tabela continua a
vir do `ModelMeta`; o nome de schema é validado e é configuração do handle.

## Consequences

- **Prós:** multi-schema sem tocar no compilador nem no `ModelMeta`; isolamento
  por handle provado; `public` no fim mantém extensões/tipos partilhados
  acessíveis.
- **Contras / custo:** o reciclar de pool `clean` (`DISCARD ALL`) repõe o
  `search_path` — um deployment que use `clean` tem de o reconfigurar (o default
  `Verified` não o toca). Falta ergonomia a seguir: declarar o schema no modelo
  (`#[model(schema = …)]`) como *hint* e `dorm migrate deploy --schema <x>` para
  correr migrações por schema — ambos assentam neste mecanismo.
- **Dependências:** o `merge` de opções libpq do ADR 0004 (`statement_timeout`).

## Proof

- **Live (`dorm-models/tests/schema_isolation_live.rs`):** dois schemas `mod_a`/
  `mod_b` com uma tabela homónima semeada diferente; dois handles
  `connect_schema`; o **mesmo** modelo sob cada handle vê só as linhas do seu
  schema; uma consulta em `mod_a` pela PK que só existe em `mod_b` vem vazia; um
  nome de schema inválido é recusado antes de ligar.
- **Unit:** `merge_search_path` acrescenta a cláusula e valida o identificador.
