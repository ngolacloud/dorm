# ADR 0057 — Data Plane `dorm-server`: gRPC como caminho crítico, REST/OpenAPI derivados

Status: **Rejected** · see [ADR 0054](0054-orm-boundary.md)

> Renumbered from ADR 0012 — a 2026-10-31 review-branch proposal that collided
> with the accepted [ADR 0012](0012-native-cqrs.md).
> **Rejected:** DORM ships no in-repo gRPC data plane (`dorm-server`). Narrow,
> read-only, one-module tooling access to a module's data — if ever needed — is a
> consuming project's own binary, never the way services talk to each other
> (that is contracts and events). This keeps DORM one database, one transaction,
> one process.

## Context

Não existe `tonic`/`prost`/`buf` em nenhum `Cargo.toml` do workspace nem um crate
`dorm-server` (achado GW-03; `00-discovery.md:44`). O único modo de rede é a API
CRUD gerada em Axum (`crates/dorm-schema/src/codegen_api.rs`), que o próprio ADR
0001 classifica como "planned ... not yet built" (`0001-declarative-schema.md:58-63`)
e cujo achado GW-04 detalha como não sendo de produção:

- OpenAPI construído à mão por `json!` (`codegen_api.rs:280-341`, versão
  `"3.0.3"` em `:336`), separado dos *handlers* — pode divergir em silêncio.
- Sem *streaming* de linhas: `grep` por `stream|cursor|copy_in` em `db.rs` só
  devolve um comentário (`db.rs:175`). Tudo materializa em memória.
- Sem *deadlines* mapeados a `statement_timeout`, sem transacções interactivas
  remotas por sessão.

Tudo o que não seja um embedder Rust fica sem caminho de produção (H11).

## Decision

Criar o Data Plane `dorm-server`, um crate novo sobre `tonic`, como **caminho
crítico gRPC**, servindo o serviço `dorm.v1.Query` definido no ADR 0055 sobre o
catálogo em runtime do ADR 0056. O REST/OpenAPI é **derivado da mesma
definição**, não escrito à mão.

- **Streaming de linhas.** RPCs de leitura devolvem `stream QueryRow` em vez de
  materializar tudo em memória, corrigindo a ausência notada em GW-03. Os
  metadados de coluna da resposta vêm do `ColumnSpec` (`compiler.rs:31-36`).
- **Deadlines → `statement_timeout`/cancelamento.** O *deadline* gRPC do pedido é
  traduzido no servidor para o `statement_timeout` do ADR 0004
  (`0004-production-connections.md:57-69`) e para o cancelamento do
  `tokio-postgres`. É cancelamento do lado do servidor, não um *race* client-side
  que deixaria a consulta a correr e a segurar locks — a mesma razão do ADR 0004.
- **Transacções interactivas por sessão.** `Begin`/`Exec`/`Commit`/`Rollback`
  com identificador de sessão, **TTL** (uma sessão órfã expira e faz `ROLLBACK`)
  e **idempotency keys** por operação de escrita (reenvio seguro). Reaproveita o
  `db::atomic` existente e o trabalho de transacções interactivas de `dorm studio`
  (`:begin`/`:commit`/`:rollback`), agora exposto por rede.
- **OpenAPI 3.1 + REST gerados da MESMA definição** (transcoding gRPC↔JSON /
  `gnostic`), substituindo o `codegen_api.rs` à mão. Nunca mais se escreve o
  OpenAPI à mão; o contrato REST não pode divergir dos *handlers* porque é o mesmo
  `.proto`.

O `dorm-server` é **transporte + sessão**, não um segundo sítio onde se constrói
SQL: cada RPC desemboca nas funções `compile_*` existentes (`compile_select`/
`compile_insert`/`compile_update`/`compile_delete`), intocadas.

### Como preserva os invariantes do `ARCHITECTURE.md`

- **I1 / I2:** cada RPC converte a `Query` do IR (ADR 0055) num `Plan` validado
  contra o descritor (ADR 0056) e desemboca no compilador → `$n`. O servidor não
  constrói identificadores nem interpola valores; limita-se a transportar o IR e
  a gerir a sessão transaccional.
- **I3 (o compilador não faz I/O):** todo o I/O — pool, transacções, *deadlines*,
  *streaming*, TTL de sessão — vive em `dorm-server` e em `db.rs`, do lado de
  fora do compilador, que permanece puro e fixado por *snapshots*.

## Consequences

- **Prós:** existe pela primeira vez um Data Plane de grau de produção: gRPC com
  *streaming*, transacções remotas seguras e um contrato REST/OpenAPI que não
  diverge. É o ponto de entrada da segurança/multi-tenancy (ADR 0059), do
  controlo de concorrência remoto (ADR 0060) e dos objectos de BD remotos
  (ADR 0058).
- **Contras / custo:** um servidor de longa duração traz gestão de sessões,
  TTLs, *idempotency keys* e *backpressure* de *streaming* — superfície
  operacional nova. A tradução transcoding gRPC↔REST tem um custo de manutenção,
  pago uma vez contra o custo perpétuo de um OpenAPI à mão.
- **Testes necessários:** E2E por RPC (Select *streaming*, Count, Exists, Insert,
  Update, Delete) contra PostgreSQL live, provando que o SQL emitido é idêntico
  ao do caminho embebido; teste de *deadline* → cancelamento do lado do servidor
  (SQLSTATE `57014`); ciclo de vida de sessão transaccional (Begin/Exec/Commit,
  Rollback, expiração por TTL com `ROLLBACK`, reenvio idempotente); teste de que
  o OpenAPI gerado corresponde aos RPCs (nenhuma rota à mão).
- **Dependências:** depende do ADR 0055 (Query IR) e do ADR 0056 (catálogo em
  runtime); é a fundação dos ADRs 0013, 0014 e do acesso remoto do 0015.
