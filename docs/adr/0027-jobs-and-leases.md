# ADR 0027 — Fila de trabalho e leases (`dorm-jobs`)

Status: Accepted — núcleo implementado (Fase 5) · 2026-11-03

## Context

Os reconciliadores de um control plane precisam de uma fila durável e de garantir
que um único reconciliador activo trata cada partição. O DORM não tem nenhum dos
dois: o único mecanismo de fila é o `dorm_rag_outbox`, específico do RAG, sem
agendamento futuro, prioridades, nem chave de unicidade; os advisory locks só
existem para serializar migrações. Não há eleição de líder.

Uma fila de trabalho genérica tem de: reclamar trabalho sem dois workers
pegarem no mesmo (`FOR UPDATE SKIP LOCKED`), agendar para o futuro, ordenar por
prioridade, repetir com *backoff* e mandar para *dead-letter*, e impedir trabalho
duplicado por recurso (uma única tarefa pendente por chave). E tem de permitir
correr **um** reconciliador activo por partição.

## Decision

Um crate **opcional** `dorm-jobs`, na disciplina do `dorm-rag`/`dorm-outbox`:
dependência unidireccional sobre `dorm-core`.

- **`dorm_jobs` durável.** `(id, queue, kind, payload, priority, run_at, state,
  attempts, max_attempts, last_error, unique_key, locked_at, locked_by, …)`.
  Estados: `pending`→`running`→`done`/`dead`.
- **`enqueue(queue, kind, payload, opts)`** — `priority`, `delay` (agendamento
  futuro via `run_at = now() + $delay`), `max_attempts`, `unique_key`. Uma
  **chave de unicidade** com índice único parcial em `(queue, unique_key) WHERE
  state IN ('pending','running')` garante **uma só tarefa activa por recurso**;
  um `enqueue` duplicado é absorvido (`ON CONFLICT DO NOTHING`) e devolve `None`.
- **`dequeue(queue, worker)`** — reclama a tarefa de maior prioridade já vencida
  (`run_at <= now()`), com `FOR UPDATE SKIP LOCKED` no sub-select, para que dois
  workers nunca peguem na mesma; marca `running`, incrementa `attempts`.
- **`complete`/`fail`.** `fail` reagenda com `run_at = now() + backoff` enquanto
  `attempts < max_attempts`; ao atingir o tecto marca `dead` (dead-letter).
- **`run_once(queue, worker, handler)`** — reclama uma, corre o handler, conclui
  ou falha; a forma que um worker chama em laço.
- **Leases com expiração (`dorm_leases`).** `acquire_lease(name, holder, ttl)` via
  `INSERT … ON CONFLICT (name) DO UPDATE … WHERE expires_at < now() OR holder =
  …` — o primeiro a pegar detém-no até expirar; outro só o rouba depois de
  expirar, e o próprio detentor renova. É a eleição de líder "um reconciliador
  activo por partição", sem os problemas de um advisory lock de sessão sobre um
  pool (o lock ficaria preso à ligação errada).
- **Métricas.** `depth(queue)` (pendentes) e `oldest_age_secs(queue)` (idade do
  mais antigo pendente) — profundidade e idade da fila.

### Como preserva os invariantes do `ARCHITECTURE.md`

Vive num crate à parte sobre `dorm-core`; todo o SQL liga valores como `$n`, os
nomes de tabela são constantes do crate, e o compilador de consultas não é tocado.

## Consequences

- **Prós:** fila durável com reclamação concorrente segura, agendamento,
  prioridades, retries/backoff/dead-letter e unicidade por recurso; eleição de
  líder tolerante a pool. É o que os reconciliadores precisam.
- **Contras / custo:** a reclamação é uma linha por `dequeue` (um `UPDATE … SKIP
  LOCKED`); uma fila de altíssimo débito afinaria o lote ou partionaria a tabela
  (eixo partições, L10). O lease é cooperativo: um detentor que exceda o TTL sem
  renovar é substituído — o trabalho tem de respeitar o seu próprio lease.
- **Dependências:** `db::sql`/`db::run`/`db::atomic` do núcleo.

## Proof

- **Live (`dorm-jobs/tests/jobs_live.rs`):** enqueue→dequeue→complete; `unique_key`
  deduplica (um só job activo por recurso); a prioridade mais alta sai primeiro;
  um job agendado para o futuro não é reclamado antes da hora; duas reclamações
  concorrentes pegam em **jobs diferentes** (SKIP LOCKED); um handler que falha
  repete e acaba em `dead`; um lease é detido por um holder e só é roubado após
  expirar; `depth` reflecte os pendentes.
- **Unit:** DDL fixado; `EnqueueOpts` por omissão.
