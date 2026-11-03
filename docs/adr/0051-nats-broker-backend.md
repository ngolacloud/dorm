# ADR 0051 — Backend de broker NATS + ponte outbox→broker (L7)

Status: Accepted — backend NATS + ponte implementados e provados ao vivo (Fase 7) · 2026-11-03

## Context

O [[0031]] deu ao `dorm-messaging` o envelope CloudEvents e o `OutboxBus` (o
transporte por omissão é o outbox transacional: publicar escreve o evento na
**mesma transação** que a mudança de negócio, consumir é at-least-once com efeito
exactly-once via inbox). Os backends de broker (NATS, Kafka, AMQP) foram
declarados atrás de features mas **não implementados nem exercitados** — "precisam
de um broker a correr". L7 era isso: um backend real, provado ao vivo.

Medição vs. ponto de partida: `dorm-messaging/Cargo.toml` tinha `nats = []`
(feature vazia) e o `lib.rs` só tinha o `trait Publisher` + `OutboxBus`; nenhuma
ligação a um broker.

## Decision

Implementar o **NATS** (o priorizado no [[0031]]) e a **ponte outbox→broker**
genérica, mantendo o padrão de outbox como a fonte de durabilidade:

- **`feature nats = ["dep:async-nats"]`** — o cliente `async-nats` (0.45) só entra
  quando a feature está ligada; um build que use só o `OutboxBus` não arrasta
  cliente de broker.
- **`nats::NatsPublisher`** (atrás da feature) implementa o `Publisher`: liga-se a
  `nats://host:port`, publica o envelope em **CloudEvents structured JSON** no
  *subject* (= topic) e faz `flush`, para que uma falha de publicação apareça ali
  (a ponte não pode avançar o cursor sobre uma publicação não confirmada).
- **`OutboxBus::forward(consumer, topic, cfg, publisher)`** — a ponte, genérica
  sobre qualquer `Publisher`: drena, via `dorm_outbox::relay`, o que o `consumer`
  ainda não viu para o `topic` e publica cada evento, avançando o cursor **só após**
  uma publicação bem-sucedida. É o padrão transactional-outbox → broker: a escrita
  de negócio confirmou o evento com a sua transação; esta relay leva-o ao broker
  com entrega **at-least-once** (uma publicação que falha deixa o cursor onde
  estava → reentrega; após `max_attempts` → dead-letter, exactamente como o
  `relay` define).

Kafka e AMQP ficam na mesma forma, atrás das suas features, por ligar — a abstração
(`Publisher` + `forward`) já os acomoda sem mudança.

## Consequences

- **Prós:** um evento publicado transacionalmente no outbox chega a um broker NATS
  real, at-least-once, sem acoplar a escrita de negócio ao broker (se o NATS estiver
  em baixo, o evento fica durável no outbox e é reenviado). A mesma ponte serve
  qualquer `Publisher` (NATS hoje; um duplo de teste; Kafka/AMQP amanhã).
- **Decisões/limites (honesto):** a publicação usa o **core NATS** (fire-and-flush)
  — a durabilidade e a reentrega vêm do **outbox**, não do JetStream; um consumidor
  que queira durabilidade do lado do broker usa `publisher.client()` para abrir um
  contexto JetStream próprio. A de-duplicação no consumidor continua a ser o inbox
  do [[0031]] (a ponte garante at-least-once, não exactly-once-no-broker). Kafka e
  AMQP não estão implementados. Autenticação/TLS do NATS ficam para configuração do
  `async_nats::ConnectOptions` num follow-up (hoje é URL simples).

## Proof

- **Offline:** `make check` (com `--all-features`, logo com `nats` ligado) verde —
  o módulo `nats` e a ponte compilam e passam o clippy; o build por omissão
  (sem a feature) não arrasta `async-nats`.
- **Live (`dorm-messaging/tests/nats_live.rs`, NATS 2.10.7 + PostgreSQL 16):**
  três CloudEvents são publicados no outbox numa transação; `OutboxBus::forward`
  reenvia os três ao NATS (`forwarded == 3`); um subscritor recebe os três
  envelopes **intactos** (round-trip CloudEvents JSON, ids a bater); um segundo
  `forward` reenvia **zero** (o cursor já os cobre — at-least-once, não
  at-least-twice num percurso saudável).
