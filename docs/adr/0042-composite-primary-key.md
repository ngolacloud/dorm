# ADR 0042 — Chave primária composta (L5-composite-PK)

Status: Accepted — DDL + consulta implementados (Fase 7) · 2026-11-03

## Context

Tabelas de chave natural precisam de uma PK de várias colunas — `(region, code)`.
O DORM só sabia PK de uma coluna; o derive recusava `>1 primary_key` e mudar a PK
era `UnsupportedChange` (`schema.md` "Not yet").

## Decision

Vários `#[field(primary_key)]` declaram uma PK composta, emitida como um
`PRIMARY KEY (a, b)` **ao nível da tabela** (as colunas-chave saem sem o
`PRIMARY KEY` inline). 

- As colunas de uma PK composta são **fornecidas pelo chamador** (nunca o atalho
  serial): por isso têm de ser não-serial (p. ex. `String`); um `i32`/`i64` nu é
  recusado pelo derive, porque seria lido como auto-incremento e cairia do insert.
- `ModelMeta::primary_keys()` (todas) e `is_composite_pk()` juntam-se ao
  `primary_key()` (a primeira, para as APIs de chave única).
- **Create e consulta funcionam** (o insert usa `column_values`, a consulta usa
  filtros — nenhum depende de uma pk única). A unicidade é imposta pela PK.

## Consequences

- **Prós:** chaves naturais compostas criam, inserem, consultam e impõem
  unicidade.
- **Por fazer (declarado, honesto):** `save`/`delete` **por instância** numa
  tabela de PK composta são **recusados** por agora — o `Model::pk()` devolve um só
  valor, e uma operação por instância precisa de todas as colunas da chave; usa-se
  um insert/update/delete por `QuerySet` entretanto. A emissão de `@@id([a,b])` a
  partir do `dorm.schema` assenta neste mecanismo (parser à parte).

## Proof

- **Unit (offline):** o DDL de um modelo com dois `primary_key` contém
  `PRIMARY KEY (region, code)` e **não** chaves inline; `is_composite_pk()` e
  `primary_keys().count() == 2`.
- **Live (`dorm-models/tests/composite_pk_live.rs`):** a tabela cria-se a partir do
  DDL gerado; três linhas inserem; a consulta pela chave completa devolve uma, por
  meia-chave devolve duas; reinserir `(eu, lis)` dá violação de unicidade (`23505`);
  `save()` por instância é recusado com uma mensagem que nomeia a limitação.
