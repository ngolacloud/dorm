# ADR 0026 — Outbox e inbox genéricos (`dorm-outbox`)

Status: Accepted — núcleo implementado (Fase 5) · 2026-11-03

## Context

O DORM já tem um outbox, mas é **específico do RAG**: a tabela `dorm_rag_outbox`,
a captura por trigger e o worker vivem em `dorm-rag` e servem só a indexação
semântica (ADR 0019). É mono-consumidor e de consumo destrutivo (`DELETE` ao
processar), sem cursor por consumidor, sem inbox de idempotência, e com o
"backoff" prometido no ADR 0019 ainda por implementar.

Uma plataforma precisa de um outbox **genérico**, partilhado por vários
consumidores independentes — projecções (L12), relays de mensageria (L7),
reconciliadores — cada um a avançar ao seu ritmo sem destruir eventos que outro
ainda não viu. É o padrão transactional-outbox: a escrita de negócio e a
publicação do evento acontecem na **mesma transacção**, por isso nunca há um
evento sem a escrita nem uma escrita sem o evento.

## Decision

Um crate **opcional** `dorm-outbox`, na mesma disciplina do `dorm-rag`:
dependência unidireccional sobre `dorm-core` (o núcleo nunca depende dele, logo um
projecto que não o use tem o grafo intacto).

- **`dorm_outbox` append-only.** `(id BIGSERIAL, topic, payload JSONB, created_at)`.
  Nada é apagado no consumo; a retenção (job) corta o que **todos** os consumidores
  já passaram.
- **`publish(topic, payload)` só em transacção.** Faz `INSERT … RETURNING id`;
  fora de uma transacção (`db::in_transaction()` falso) é erro — garante o padrão
  outbox (o evento vive ou morre com a escrita de negócio).
- **Cursor por consumidor.** `dorm_outbox_cursor (consumer, topic, position)` —
  cada consumidor avança a sua própria posição; dois consumidores vêem os mesmos
  eventos (fan-out).
- **Relay exactly-once (efeito na BD).** Por evento, numa transacção: tranca a
  linha do cursor (`FOR UPDATE`), reconfirma a posição, corre o handler (cujos
  efeitos de BD partilham a transacção via `CURRENT_TX`), regista o inbox e avança
  a posição — tudo atómico. Um handler que falhe faz *rollback* (a posição não
  avança) e é repetido com *backoff*; ao fim de `max_attempts` vai para
  **dead-letter** e a posição avança para não estagnar.
- **Inbox idempotente.** `dorm_inbox (consumer, message_id, …)` com
  `UNIQUE (consumer, message_id)` via `INSERT … ON CONFLICT DO NOTHING` — um
  consumidor não-idempotente usa-o para efeito exactly-once mesmo com entrega
  at-least-once.
- **Dead-letter.** `dorm_outbox_dead` guarda o evento, o consumidor, o erro e o
  instante — inspeccionável e re-injectável.

### Como preserva os invariantes do `ARCHITECTURE.md`

Vive inteiramente num crate à parte, sobre `dorm-core`. `publish` e o relay usam
`db::atomic`/`db::sql` com valores ligados (`$n`); os nomes de tabela são
constantes do crate, nunca do chamador. O compilador de consultas não é tocado.

## Consequences

- **Prós:** vários consumidores independentes com cursor próprio; publicação
  atómica com a escrita de negócio; exactly-once para handlers de BD; dead-letter
  inspeccionável. É a base do event sourcing (L12) e dos relays de mensageria (L7).
- **Contras / custo:** o relay por-evento-em-transacção troca débito por correcção
  (uma transacção por evento); um projecto de altíssimo débito afinará o lote. A
  migração do `dorm-rag` para ser um consumidor deste outbox (preservando dados)
  fica como follow-up — o RAG mantém o seu outbox próprio até lá.
- **Dependências:** `db::in_transaction`/`db::atomic`/`db::sql` do núcleo.

## Proof

- **Live (`dorm-outbox/tests/outbox_live.rs`):** `publish` fora de transacção é
  recusado; dentro de `db::atomic` grava; dois consumidores drenam os mesmos
  eventos de forma independente (fan-out), cada um uma vez e por ordem; um handler
  que falha sempre acaba em dead-letter e o cursor avança; o inbox recusa o
  segundo processamento do mesmo `message_id`.
- **Unit:** DDL das migrações fixado; `publish` fora de transacção erra sem tocar
  na BD.
