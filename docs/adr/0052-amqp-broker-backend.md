# ADR 0052 — Backend de broker AMQP/RabbitMQ (L7, parte 2)

Status: Accepted — backend AMQP implementado e provado ao vivo (Fase 7) · 2026-11-03

## Context

O [[0051]] implementou o primeiro backend de broker (NATS) e a ponte genérica
`OutboxBus::forward<P: Publisher>`. O AMQP (RabbitMQ) ficou declarado atrás da
feature `amqp` mas por ligar. Muita da NgolaCloud e dos seus inquilinos fala AMQP
0-9-1 (RabbitMQ), por isso é o segundo backend a fechar.

Medição vs. ponto de partida: `dorm-messaging/Cargo.toml` tinha `amqp = []`
(feature vazia) e nenhum `Publisher` de AMQP.

## Decision

Reutilizar o padrão do [[0051]] — a ponte `forward` não muda; só entra um
`Publisher` novo:

- **`feature amqp = ["dep:lapin"]`** — o cliente `lapin` (2.5) só entra com a
  feature; um build sem ela não o arrasta.
- **`amqp::AmqpPublisher`** implementa o `Publisher`: liga a
  `amqp://user:pass@host:port/vhost`, abre um canal em modo de **publisher
  confirms**, e publica o envelope em **CloudEvents structured JSON** na *default
  exchange* com o `topic` como *routing key* (entrega à fila com o nome do topic).
  O `publish` **espera a confirmação do broker** antes de devolver — a ponte não
  pode avançar o cursor sobre uma publicação não confirmada (o mesmo contrato que
  o `flush` do NATS).
- A `OutboxBus::forward` é genérica sobre `Publisher`, por isso serve o
  `AmqpPublisher` sem uma linha nova: outbox transacional → RabbitMQ,
  at-least-once, cursor só avança após confirmação, dead-letter após
  `max_attempts`.

Kafka fica como o único backend por ligar, na mesma forma.

## Consequences

- **Prós:** eventos publicados transacionalmente no outbox chegam a um RabbitMQ
  real, at-least-once, com confirmação do broker; a NgolaCloud e os inquilinos que
  já falam AMQP ligam-se sem adaptador. A abstração provou-se extensível — dois
  brokers (NATS, AMQP) partilham a mesma ponte.
- **Decisões/limites (honesto):** publica na *default exchange* com o topic como
  routing key (modelo fila-por-topic); exchanges nomeadas/tópicos AMQP,
  mensagens persistentes (`delivery_mode=2`), e auth/TLS por `amqp s://` ficam para
  configuração via `publisher.channel()` ou um follow-up. A de-duplicação no
  consumidor continua a ser o inbox do [[0031]]. Kafka por implementar.

## Proof

- **Offline:** `make check` (com `--all-features`, logo `nats` **e** `amqp`
  ligados) verde — o módulo `amqp` e a ponte compilam e passam o clippy; o build
  por omissão não arrasta `lapin`.
- **Live (`dorm-messaging/tests/amqp_live.rs`, RabbitMQ 3.12.1 + PostgreSQL 16):**
  declara-se a fila, publicam-se três CloudEvents no outbox numa transação;
  `OutboxBus::forward` reenvia os três ao RabbitMQ (`forwarded == 3`, cada um
  confirmado pelo broker); um consumidor drena a fila e recebe os três envelopes
  **intactos** (round-trip CloudEvents JSON, ids a bater); um segundo `forward`
  reenvia **zero** (o cursor já os cobre).
