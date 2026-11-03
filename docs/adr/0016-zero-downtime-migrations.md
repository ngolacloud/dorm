# ADR 0016 — Migrações zero-downtime

Status: Proposed · 2026-10-31

## Context

Toda a migração corre num único bloco transaccional (achado MIG-01, H7
confirmada): `migrator.rs:77` envolve **todas** as statements de **todas** as
migrações pendentes num `db::atomic(|_tx| ...)`
(`crates/dorm-migrations/src/migrator.rs:77`), que abre `BEGIN` numa ligação
dedicada (`db.rs:1258`, `atomic_pg` `db.rs:1274`). Daí decorre:

1. `CREATE INDEX CONCURRENTLY`, `ALTER TYPE ... ADD VALUE` e
   `REFRESH MATERIALIZED VIEW CONCURRENTLY` são **proibidos dentro de uma
   transacção** pelo PostgreSQL (`25001`) — impossíveis de emitir por este
   migrador. A palavra `CONCURRENTLY` não existe no crate (`grep` vazio).
2. Um `CREATE INDEX` normal pega `SHARE` lock e mantém-no até ao `COMMIT` final
   da migração toda, não só durante o índice.
3. Não há `lock_timeout` posto para a migração; uma espera por lock pode ser
   indefinida, empilhando ligações.

Além disso, a renomeação de coluna/tabela é tratada como **drop+add** com perda
de dados silenciosa (achado MIG-02): o autodetect compara colunas só por nome
(`autodetect.rs:250-253`, `:290-321`), logo uma coluna renomeada entra em
`col_drops` (`DROP COLUMN`) e a nova em `adds` (`ADD COLUMN`) — os dados da antiga
são destruídos. E não há backfill em lotes: um `ADD COLUMN ... NOT NULL` sem
default é rejeitado em tabela populada (`ddl.rs:155-160`) sem caminho para
preencher em *batches*.

## Decision

Tornar o migrador capaz de operar sem *stop-the-world*:

- **Statements marcadas "fora-de-transacção".** `CREATE INDEX CONCURRENTLY`,
  `REFRESH MATERIALIZED VIEW CONCURRENTLY`, `ALTER TYPE ... ADD VALUE` e `VACUUM`
  são rotuladas e, no *applier*, executadas **cada uma na sua própria ligação,
  fora do `db::atomic`** de `migrator.rs:77`, com o *ledger* gravado
  separadamente (o padrão `atomic = False`/`AddIndexConcurrently` do Django).
- **`lock_timeout` por migração.** `SET LOCAL lock_timeout` no início da
  transacção de migração, para que uma espera por lock falhe em vez de empilhar
  ligações indefinidamente (o `set_local` seguro é o do ADR 0015).
- **Detecção de renomeação.** Heurística (mesmo tipo + posição, uma só coluna
  sumiu e uma só apareceu) com **confirmação do operador**, ou um atributo
  explícito `@@renamed_from`/operação `RenameColumn` na história, emitindo
  `ALTER TABLE ... RENAME COLUMN`. Enquanto a confirmação não existir, a
  combinação drop+add ambígua é **recusada** em vez de executada — corrige a perda
  de dados silenciosa de MIG-02.
- **Expand/contract.** Statements rotuladas por fase (expand → migrate →
  contract), para que uma mudança de forma seja compatível com a versão antiga e
  a nova da aplicação durante a transição.
- **Backfill em lotes.** Um caminho de preenchimento em *batches* de tamanho fixo
  (em vez de um `UPDATE` único), para popular uma coluna nova antes de a tornar
  `NOT NULL`.

### Como preserva os invariantes do `ARCHITECTURE.md`

- **I1 / I2:** as migrações lêem o mesmo `ModelMeta` que as consultas lêem e
  geram DDL revisível (`ARCHITECTURE.md:39-41`); os identificadores vêm do esquema
  (não do chamador) e os `checks`/defaults são dados estáticos do esquema. A
  detecção de renomeação opera sobre o `Snapshot` do esquema, não sobre entrada de
  um chamador. O backfill liga valores como `$n`.
- **I3 (o compilador não faz I/O):** esta decisão vive inteiramente no
  `dorm-migrations` (*applier*, *ledger*, autodetect), fora do compilador de
  consultas, que permanece puro. O contrato de paridade de DDL
  (`schema_parity.rs`) mantém-se o oráculo.

## Consequences

- **Prós:** índices concorrentes e *refresh* concorrente passam a ser possíveis;
  um `CREATE INDEX` normal já não segura um lock pela migração toda; o
  `lock_timeout` impede esperas indefinidas; a renomeação deixa de apagar dados em
  silêncio; expand/contract e backfill tornam viável evoluir uma tabela grande sem
  paragem.
- **Contras / custo:** uma statement fora-de-transacção que falhe a meio deixa a
  migração parcialmente aplicada — o *ledger* tem de registar o progresso por
  statement e o *applier* tem de ser retomável; a detecção de renomeação com
  confirmação do operador acrescenta um passo interactivo; expand/contract exige
  disciplina de mais de uma migração por mudança.
- **Testes necessários:** um `CREATE INDEX CONCURRENTLY` corre fora do `atomic` e
  não dá `25001`; um `CREATE INDEX` normal já não segura escritas para lá do seu
  próprio tempo; `lock_timeout` faz uma migração bloqueada falhar em vez de
  pendurar; uma renomeação é detectada/confirmada e emite `RENAME COLUMN`
  (preservando os dados) — e, sem confirmação, é recusada em vez de destrutiva;
  backfill em lotes popula a coluna antes do `SET NOT NULL`.
- **Dependências:** usa o `set_local` seguro do ADR 0015 (`lock_timeout`);
  independente do eixo gateway (ADRs 0010-0014).
