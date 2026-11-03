# ADR 0059 — Segurança e multi-tenancy do Data Plane

Status: **Rejected** · see [ADR 0054](0054-orm-boundary.md)

> Renumbered from ADR 0014 — a 2026-10-31 review-branch proposal that collided
> with the accepted [ADR 0014](0014-reversible-migrations.md).
> **Rejected:** authentication, identity, per-model authorization, RLS and quotas
> are guarantees that come from who-calls and from policy, not from a database
> transaction — a consuming framework's concern (ADR 0054). The tenant-scope
> *mechanism* this ADR assumed landed, accepted, as
> [ADR 0021](0021-tenant-scope-enforcement.md).

## Context

A API gerada em Axum não tem segurança nenhuma: `grep` por
`auth|jwt|bearer|token|tls|mtls` em `codegen_api.rs` devolve zero (achado GW-04);
o `main_fn` (`crates/dorm-schema/src/codegen_api.rs:242-277`) regista `/health`,
`/openapi.json` e as rotas CRUD sem *middleware* nenhum. A paginação não tem tecto
(`limit.unwrap_or(50)` sem máximo, `codegen_api.rs:153`), e os erros devolvem o
`Display` interno cru ao cliente (`codegen_api.rs:77-79`), com fuga de detalhe.

O `ModelMeta` já declara `tenant_scoped`/`tenant_field` (`meta.rs:107`,
`:134-145`), mas nada o aplica: não há identidade, não há autorização por
modelo/operação, não há Row-Level Security, não há quotas. Para o Data Plane do
ADR 0057 ser multi-tenant e de produção, falta toda esta camada (achados
SEC-02/03/04/05).

## Decision

O `dorm-server` (ADR 0057) ganha a camada de segurança, em redor do compilador
puro:

- **mTLS/JWT.** Transporte e identidade no Data Plane — ausentes hoje. A
  identidade do sujeito é estabelecida antes de qualquer `Plan` compilar.
- **Autorização por modelo/operação.** Avaliada contra a identidade antes de o
  `Plan` compilar (p. ex. "este sujeito pode `SELECT` em `Application` mas não
  `DELETE`"). A decisão **restringe** o conjunto já validado pelo descritor; nunca
  acrescenta identificadores.
- **RLS por GUC transaccional.** O *tenant* corrente é injectado como
  `SET LOCAL app.tenant_id = $1` dentro da transacção, para o PostgreSQL aplicar
  *Row-Level Security*. Liga-se directamente a `tenant_scoped`/`tenant_field`
  (`meta.rs:107`). O `set_local` seguro é o mecanismo do ADR 0060 (chave validada,
  valor ligado).
- **Limites de custo de consulta.** Tecto de linhas, profundidade máxima de joins
  (contável a partir de `Joins`/`resolve`, `registry.rs:277-333`) e tempo (o
  *deadline* → `statement_timeout` do ADR 0057) — corrige o `limit` sem tecto de
  `codegen_api.rs:153`.
- **Quotas por *tenant*.** Pedidos, sessões transaccionais abertas, linhas lidas.
- **Saneamento de erros.** A superfície remota devolve um erro tipado e estável,
  nunca o `Display` interno cru (fecha a fuga de `codegen_api.rs:77-79`).

### Como preserva os invariantes do `ARCHITECTURE.md`

- **I1 (nenhum identificador vem do cliente):** autorização e RLS **restringem** o
  que um identificador já validado pelo descritor (ADR 0056) pode fazer; nunca
  introduzem identificadores do cliente. O conjunto permitido continua a ser o
  descritor.
- **I2 (nenhum valor é formatado no SQL):** o *tenant* da RLS viaja como valor
  ligado (`SET LOCAL app.tenant_id = $1`), não interpolado no texto. A chave do
  GUC é validada contra uma allow-list (ADR 0060), não vem livre do cliente.
- **I3 (o compilador não faz I/O):** autorização, quotas e saneamento de erros
  são verificações no `dorm-server`, antes e à volta do compilador puro; a RLS é
  responsabilidade do PostgreSQL. O compilador não passa a conhecer identidade
  nem rede.

## Consequences

- **Prós:** o Data Plane passa a ser utilizável em produção multi-tenant: a RLS
  liga a declaração que o `ModelMeta` já carregava; os limites de custo matam a
  paginação profunda sem travão; o saneamento de erros fecha a fuga de detalhe
  interno.
- **Contras / custo:** mTLS/JWT e quotas trazem gestão de identidade,
  certificados e contabilização por *tenant* — operação nova. A RLS exige que as
  *policies* existam na base de dados (eixo migrações), senão o `SET LOCAL` não
  tem efeito.
- **Testes necessários:** pedido sem identidade válida é recusado (mTLS/JWT);
  autorização nega uma operação proibida antes de compilar; RLS isola linhas por
  *tenant* com `SET LOCAL app.tenant_id` (dois *tenants*, cada um só vê o seu);
  tecto de linhas/joins/tempo aplicado; quota excedida recusada; um erro interno
  não vaza o `Display` cru para o cliente.
- **Dependências:** assenta no ADR 0057 (Data Plane) e no ADR 0056 (o conjunto
  permitido é o descritor); o `set_local` seguro para o GUC da RLS é o do ADR 0060.
