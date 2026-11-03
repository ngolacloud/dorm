# ADR 0060 — Controlo de concorrência

Status: **Partially superseded** · see [ADR 0054](0054-orm-boundary.md)

> Renumbered from ADR 0015 — a 2026-10-31 review-branch proposal that collided
> with the accepted [ADR 0015](0015-connection-resilience.md).
> **Partially superseded by [ADR 0023](0023-optimistic-concurrency.md)** (optimistic
> concurrency + safe `set_local`). The pessimistic-locking primitives — `SELECT …
> FOR UPDATE`/`SKIP LOCKED`/`NOWAIT`, per-transaction `lock_timeout`, isolation
> levels and serialization-failure classification — remain a pending ORM roadmap
> item (transactional primitives any embedder can drive).

## Context

DORM não tem controlo de concorrência de grau DBA (achado DBA-02, H4 confirmada
por varrimento). As ausências estão provadas por código:

- A transacção abre sempre com `BEGIN` literal (ou `BEGIN READ ONLY`); **nunca**
  `SET TRANSACTION ISOLATION LEVEL` (`crates/dorm-core/src/db.rs:1326-1331`). Não
  há parâmetro de nível de isolamento em `atomic`.
- `classify_db` só distingue `57014` (timeout); `40001` (serialization_failure) e
  `40P01` (deadlock_detected) caem em `Error::Db` genérico
  (`crates/dorm-core/src/error.rs:135-143`). Não há `is_serialization_failure`
  análogo a `is_unique_violation` (`error.rs:154`), logo **nenhum retry possível
  sem *string-matching***.
- `compile_select*` não tem `FOR UPDATE`/`FOR SHARE`/`NOWAIT`/`SKIP LOCKED` (os
  lookups terminam em `compiler.rs:349`).
- `statement_timeout` é só de processo via DSN (`db.rs:59`, `:283-288`); não há
  `SET LOCAL lock_timeout`/`idle_in_transaction_session_timeout` por transacção,
  nem GUC de sessão para RLS (`SET LOCAL app.tenant_id`).
- Sem coluna `version`/optimistic locking; sem `DEFERRABLE`/`INITIALLY DEFERRED`
  (`crates/dorm-migrations/src/ddl.rs:141-150`).

O ADR 0004 já nomeia "retry/reconnect with backoff" como follow-up
(`0004-production-connections.md:80`). A recomendação do DBA-02 é a base desta
decisão.

## Decision

Trazer o controlo de concorrência para a abstracção tipada, sem o deixar só no
SQL cru:

- **Nível de isolamento por transacção.** `atomic_with(IsolationLevel, read_only)`
  que emite `BEGIN ISOLATION LEVEL ...` (o nível tem de ser o primeiro statement
  após `BEGIN`). Permite `SERIALIZABLE`/`REPEATABLE READ` para invariantes.
- **Classificação e retry de `40001`/`40P01`.** `is_serialization_failure` e
  `is_deadlock` em `error.rs`, com `classify_db` a nomear `40001`/`40P01`, e um
  helper `retry_on_serialization(n, backoff, f)` com *backoff*. Habilita o padrão
  canónico "serializável + repetir".
- **Row-locks no `QuerySet`.** `.lock_for_update()`/`.lock_for_share()` com
  `.nowait()`/`.skip_locked()`, que o `compile_select` traduz em
  `FOR UPDATE`/`FOR SHARE` + `NOWAIT`/`SKIP LOCKED` — a base de filas de trabalho
  e reserva pessimista.
- **`set_local(key, value)` seguro.** Para `lock_timeout`,
  `idle_in_transaction_session_timeout` e GUCs de RLS (p. ex. `app.tenant_id`, o
  mecanismo da RLS do ADR 0059). A **chave** é validada contra uma allow-list de
  GUCs permitidos (nunca vem livre do cliente); o **valor** é ligado como `$1`.
- **Optimistic locking por coluna `version`.** Incremento + `WHERE version =
  $old`, com erro se zero linhas forem afectadas.
- **`DEFERRABLE`/`INITIALLY DEFERRED`** nas constraints geradas (eixo migrações),
  para adiar a verificação de FK até ao `COMMIT`.

### Como preserva os invariantes do `ARCHITECTURE.md`

- **I1 (nenhum identificador vem do cliente):** os row-locks e o `version` são
  cláusulas que o compilador acrescenta sobre campos já resolvidos por
  `registry::resolve`; a chave de `set_local` é validada contra uma allow-list,
  não é um identificador livre do cliente. O nível de isolamento é um *enum*
  fechado, não texto.
- **I2 (nenhum valor é formatado no SQL):** o valor de `set_local` é ligado como
  `$1`, não interpolado; o `$old` do optimistic locking é um `Value` ligado.
- **I3 (o compilador não faz I/O):** as cláusulas de bloqueio são texto puro que
  o compilador emite a partir do `Plan`; o isolamento, o retry/backoff e o
  `set_local` são comportamento do `db`/transacção, do lado de fora do compilador,
  que permanece fixado por *snapshots* (os novos `FOR UPDATE`/`SKIP LOCKED`
  entram no contrato de *snapshot*).

## Consequences

- **Prós:** DORM passa a suportar cargas concorrentes de grau DBA pela abstracção
  — isolamento configurável, retry de serialização, locks pessimistas,
  optimistic locking e GUCs transaccionais (que também servem a RLS do ADR 0059)
  — em vez de obrigar a cair em SQL cru que não compõe com o `QuerySet`.
- **Contras / custo:** o retry re-executa o *closure* da transacção, pelo que o
  *closure* tem de ser idempotente/sem efeitos externos; `SERIALIZABLE` traz
  abortos `40001` que a aplicação tem de estar preparada para repetir; mais
  superfície de API.
- **Testes necessários:** duas tarefas em `SERIALIZABLE` sobre a mesma linha →
  uma recebe `40001`, classificada e repetida com sucesso pelo helper; *deadlock*
  `40P01` classificado; `FOR UPDATE`/`SKIP LOCKED`/`NOWAIT` fixados por *snapshot*
  e provados live (uma fila de trabalho com `SKIP LOCKED` não bloqueia);
  `set_local` recusa uma chave fora da allow-list e liga o valor; optimistic
  locking falha quando a versão mudou sob os pés.
- **Dependências:** o `set_local` seguro é o mecanismo do GUC de RLS do ADR 0059;
  independente dos ADRs 0010-0013, mas o retry/isolamento mapeiam-se às sessões
  transaccionais remotas do ADR 0057 quando expostos por rede.
