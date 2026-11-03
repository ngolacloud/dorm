# ADR 0045 — `save`/`delete` por instância em chave primária composta

Status: Accepted — implementado · 2026-11-03

## Context

A ADR 0042 trouxe a PK composta (`(region, code)`): create, consulta e imposição de
unicidade funcionam, mas `save`/`delete` **por instância** ficaram explicitamente
recusados. O motivo era o `Model::pk()` devolver **um só** `Value` (a primeira
coluna-chave), e uma operação por instância precisar de **todas** as colunas da
chave para localizar a linha certa.

Ao implementar, apareceu um defeito latente mais fundo: `compile_update_on` e
`compile_delete_on` localizavam as linhas com `WHERE <pk> IN (SELECT <pk> …)`
usando **só a primeira** coluna-chave. Numa tabela de PK composta, um
`UPDATE … WHERE region IN (SELECT region WHERE region='eu' AND code='mad')`
reduz-se a `region IN ('eu')` e atinge **todas** as linhas que partilham a
primeira coluna — corrupção silenciosa de linhas irmãs, não só uma limitação de
API.

## Decision

- **Compilador ciente de chave composta.** `compile_update_on`/`compile_delete_on`
  passam a projetar **todas** as colunas-chave na subconsulta e a comparar com uma
  **tupla row-value**: `WHERE (region, code) IN (SELECT region, code …)`. Para uma
  PK simples o lado esquerdo continua a ser a coluna nua — output byte-idêntico ao
  anterior, logo nenhum snapshot muda. Dois helpers puros isolam isto: `key_fields`
  (todas as colunas-chave, erro se nenhuma) e `key_in_lhs` (coluna nua vs. tupla).
- **`save` por instância em PK composta** (`model.rs::composite_update`): um UPDATE
  cujo `WHERE` junta **todas** as colunas-chave, lidas do `obj.column_values()`
  (`Model::pk` só carrega a primeira). O `SET` são as colunas **não-chave**,
  **não-`auto_now_add`** — exatamente o filtro do caminho de chave simples, com as
  colunas-chave retiradas do `SET` porque já fixam a linha no `WHERE`.
- **`delete` por instância em PK composta** (`model.rs::delete_instance`): um
  `Plan` + um `Q::new(coluna, valor)` por cada coluna de `meta.primary_keys()`,
  lido do `column_values()`, depois `scope::compile_delete` + `run_execute`.
- **Pelo seam de scope, sempre.** Ambos compilam por `crate::scope::compile_update`/
  `compile_delete`, logo o filtro de tenant e o de soft-delete continuam a
  aplicar-se; todos os valores ligam como `$n`; o compilador mantém-se puro (sem
  estado ambiente).
- **`save` composto é sempre UPDATE** (last-write-wins). O create numa PK composta
  continua pelo `QuerySet`, que já fornece a chave inteira — não se tenta
  insert-vs-update por instância, porque numa chave fornecida pelo chamador o
  `obj.pk().is_null()` não decide isso de forma fiável.

## Consequences

- **Prós:** chaves naturais compostas suportam agora `save()` (update) e `delete()`
  por instância, pela API tipada normal; e o `UPDATE`/`DELETE` por `QuerySet` numa
  tabela de PK composta deixou de poder apanhar linhas irmãs por engano.
- **De fora (declarado, honesto):**
  - **Create por instância** numa PK composta — continua pelo `QuerySet`
    (`objects().create(New…)`), que fornece a chave inteira.
  - **Concorrência otimista por instância** (`#[field(version)]`) numa PK composta —
    fica para outra ronda. Um modelo que declare **ao mesmo tempo** PK composta e
    `version` é **recusado** em `save` por instância (`Error::Config` que nomeia a
    limitação e aponta para o `QuerySet`), em vez de ignorar o guarda de versão em
    silêncio. O essencial — save/delete por instância na PK composta comum (sem
    version) — funciona.

## Proof

- **Unit (offline):** o DDL de um modelo com dois `primary_key` contém
  `PRIMARY KEY (region, code)`; `is_composite_pk()` e `primary_keys().count() == 2`.
  `make check` verde (fmt + clippy `-D warnings` + testes offline, incl. snapshots
  SQL inalterados — a PK simples compila igual).
- **Live (`dorm-models/tests/composite_pk_live.rs`):** contra um PostgreSQL real,
  cria a tabela do DDL gerado, insere `(eu,lis)`, `(eu,mad)`, `(af,lis)`; reinserir
  `(eu,lis)` dá violação de unicidade (`23505`); **`save()` por instância** sobre
  `(eu,mad)` muda o `label` e a releitura confirma a mudança; **`delete()` por
  instância** remove **exatamente** a linha `(eu,mad)` (afetadas = 1), a releitura
  pela chave completa não encontra nada e a contagem total fica em 2 — provando que
  a operação não tocou nas linhas irmãs que partilham `region='eu'`.

  ```text
  DELONIX_ORM_TEST_DSN='host=/var/run/postgresql port=5432 user=walter dbname=ormtest' \
    cargo test -p dorm-models --test composite_pk_live -- --ignored --nocapture
  ```
