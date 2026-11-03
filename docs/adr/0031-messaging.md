# ADR 0031 — Mensageria (`dorm-messaging`)

> **Moved (ADR 0054).** The implementation moved out of DORM to `seiva-messaging`
> / `seiva-outbox-relay` (Delonix Seiva repo), with history preserved — broker
> messaging is cross-service communication, not the ORM's. `dorm-outbox` (the
> in-database outbox) stays. This ADR is kept for the record.


Status: Accepted — núcleo (envelope + bus outbox) implementado (Fase 5/L7) · 2026-11-03

## Context

Os módulos da plataforma precisam de publicar e consumir eventos de forma
interoperável. O DORM não tinha nada: `grep` por `CloudEvent`/`Publisher`/
`traceparent` era vazio. É preciso um envelope padrão (para interoperar com o
resto do ecossistema) e uma garantia de entrega clara, sem obrigar já a um broker
externo.

## Decision

Um crate **opcional** `dorm-messaging`, dependência unidireccional sobre
`dorm-core` + `dorm-outbox`.

- **Envelope CloudEvents 1.0.** `CloudEvent { id, source, ty, time, traceparent,
  data }` com serialização no formato JSON estruturado do CloudEvents
  (`specversion: "1.0"`). O `traceparent` W3C viaja ponta-a-ponta para *tracing*
  distribuído. O `(source, id)` é a chave de idempotência.
- **Traits `Publisher`.** `publish(topic, event)`. Um backend de broker não exige
  transacção; o backend por omissão exige.
- **Bus por omissão = outbox (`OutboxBus`).** `publish` escreve o evento no outbox
  **na mesma transacção** que a escrita de negócio (ADR 0026). `drain(consumer,
  topic, cfg, handler)` consome com **entrega at-least-once** e **efeito
  exactly-once**: a guarda do inbox (`record_once`) e o efeito do handler fazem
  *commit* na **mesma transacção**, por isso um evento reentregue (a posição do
  cursor ainda não avançou) é um no-op.
- **Backends de broker atrás de features.** `nats` (JetStream, prioritário),
  `kafka`, `amqp` implementam o mesmo `Publisher`. Precisam de um broker a correr,
  por isso não são compilados nem exercitados por omissão.

### Como preserva os invariantes do `ARCHITECTURE.md`

Vive num crate à parte sobre o núcleo e o outbox; não toca no compilador de
consultas. O `publish` e o `drain` usam o `dorm-outbox`, que liga valores como
`$n` e usa nomes de tabela constantes.

## Consequences

- **Prós:** envelope interoperável (CloudEvents) com `traceparent`; o bus por
  omissão dá a garantia at-least-once + exactly-once-no-efeito **sem broker**,
  reutilizando o outbox transacional; os backends de broker são opcionais.
- **Contras / custo:** o exactly-once-no-efeito exige que o handler faça o seu
  trabalho de BD na transacção que o `drain` abre (handlers com efeitos externos
  não-transaccionais ficam em at-least-once, o que a documentação diz sem
  ambiguidade). Os backends `nats`/`kafka`/`amqp` ficam por implementar/provar —
  exigem infra de broker que o ambiente de teste não tem.
- **Dependências:** `dorm-outbox` (ADR 0026); `db::atomic` do núcleo.

## Proof

- **Live (`dorm-messaging/tests/messaging_live.rs`):** dois `CloudEvent`
  publicados numa transacção; um subscritor drena-os por ordem com o `traceparent`
  intacto; re-drenar não faz nada; **republicar um evento com o mesmo `id`** é
  reentregue mas o handler é saltado (o inbox já viu `(projector, evt-a)`) — tratado
  **exactamente uma vez**.
- **Unit:** ida-e-volta do envelope CloudEvents; atributo obrigatório em falta é
  erro.
