# ADR 0035 — Read models e Event Sourcing (L12)

Status: Proposed · 2026-11-03 — **read models por projeção implementados (Fase 7)**

## Context

O lado de leitura do CQRS (ADR 0012) precisa de read models materializados, e o
event sourcing de um event store. A regra do pedido: construir **sobre o
`dorm-outbox` (L6)**, sem um segundo mecanismo de captura de mudanças.

## Decision (parte implementada)

Um **read model é uma projeção do log do outbox**: um consumidor drena os eventos
(`dorm_outbox::relay`) e mantém uma tabela denormalizada. A propriedade que define
um read model — ser **função pura do log** — é demonstrada pela
**reconstrutibilidade**: truncar o read model, repor o cursor do projetor e
reproduzir o log reconstrói o estado idêntico. Projeções 1:1 (upsert por chave)
são idempotentes, logo seguras de reproduzir.

## Consequences

- **Prós:** read models sem segundo CDC — reutilizam o outbox transacional (um
  evento existe sse e só se a escrita de negócio existiu); reconstrutíveis.
- **Por fazer (adiado — subsistema maior):** o **event store** propriamente dito
  (ordenação por `xid8`, snapshots de agregado, upcasting de versões de evento)
  assenta no mesmo log mas é um subsistema à parte, para fase própria. Projeções
  não-idempotentes precisam do avanço do cursor na mesma transacção do efeito
  (exactly-once) — refinamento do relay (hoje at-least-once + inbox).

## Proof

- **Live (`dorm-outbox/tests/projection_live.rs`):** quatro eventos de encomenda
  (três colocadas, uma muda de estado) são projetados num read model `rm_order`
  por upsert idempotente; depois o read model é truncado, o cursor reposto, e o
  log reproduzido — o read model volta idêntico (ordem 1 em `shipped`).
