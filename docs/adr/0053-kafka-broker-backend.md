# ADR 0053 — Backend de broker Kafka (L7, parte 3)

Status: Accepted — backend Kafka implementado e provado ao vivo (Fase 7) · 2026-11-03

## Context

O [[0051]] (NATS) e o [[0052]] (AMQP) fecharam dois dos três brokers do [[0031]],
sobre a ponte genérica `OutboxBus::forward<P: Publisher>`. Faltava o **Kafka** —
o terceiro e último backend declarado.

Medição vs. ponto de partida: `dorm-messaging/Cargo.toml` tinha `kafka = []`
(feature vazia) e nenhum `Publisher` de Kafka.

## Decision

Reutilizar o padrão — a ponte não muda, só entra um `Publisher` novo —
escolhendo um cliente **puro-Rust** para não arrastar C:

- **`feature kafka = ["dep:rskafka"]`** — `rskafka` (0.5) é um cliente Kafka
  puro-Rust (sem `librdkafka`/cmake/C), pulled só com a feature.
- **`kafka::KafkaPublisher`** implementa o `Publisher`: liga a uma lista de
  bootstrap brokers (`host:port,…`), e no `publish` produz o envelope em
  **CloudEvents structured JSON** para o `topic` (partição 0), com o `id` do
  evento como **chave do registo** (particionamento/compactação), **esperando o
  ack do broker** antes de devolver — a ponte não avança o cursor sobre um produce
  não confirmado. `ensure_topic` cria o tópico idempotentemente (um producer
  precisa do tópico, salvo auto-create no broker).
- **`OutboxBus::forward`** (genérica sobre `Publisher`) conduz o `KafkaPublisher`
  sem uma linha nova: outbox transacional → Kafka, at-least-once.

Com isto, os **três** brokers (NATS, AMQP, Kafka) estão fechados sobre a mesma
ponte — a abstração provou-se completa para o [[0031]].

## Consequences

- **Prós:** eventos publicados transacionalmente no outbox chegam a um Kafka real,
  at-least-once, com ack do broker; cliente puro-Rust mantém o build sem toolchain
  de C. A escolha de `Publisher` + `forward` acomodou três brokers heterogéneos
  (subject/queue/topic) sem mudar a ponte.
- **Decisões/limites (honesto):** sem compressão (`NoCompression`); a
  de-duplicação no consumidor continua a ser o inbox do [[0031]]. O `rskafka` é
  deliberadamente de baixo nível (sem consumer-groups geridos) — o DORM só
  **produz**; consumir é do serviço a jusante.

## Follow-up landed

- **Partitioning por chave:** o `publish` deixou de produzir sempre na partição 0;
  agora lê a contagem de partições do tópico (cacheada, via `list_topics`) e
  escolhe a partição por um hash estável da chave (FNV-1a do `id` do evento, o
  partitioner próprio do DORM, não o murmur2 do Kafka). Eventos da mesma chave
  mantêm a ordem numa partição; o fluxo espalha-se pelas restantes. Prova live
  (`kafka_live.rs::distinct_keys_spread_across_partitions`): 30 chaves distintas
  num tópico de 3 partições espalham-se por 3/3.
- **Auth/TLS/SASL:** disponíveis pelo `ClientBuilder` do `rskafka`
  (`tls_config`/`sasl_config`); `KafkaPublisher::from_client` recebe um cliente
  pré-construído para esse efeito.

## Proof

- **Offline:** `make check` (com `--all-features`, logo `nats`, `amqp` **e**
  `kafka`) verde — o módulo `kafka` e a ponte compilam e passam o clippy; o build
  por omissão não arrasta `rskafka`.
- **Live (`dorm-messaging/tests/kafka_live.rs`, Apache Kafka 4.2.2 em modo KRaft
  single-node + PostgreSQL 16):** garante-se o tópico, captura-se o offset do fim
  do log, publicam-se três CloudEvents no outbox numa transação;
  `OutboxBus::forward` reenvia os três ao Kafka (`forwarded == 3`, cada um
  acked); um `fetch_records` a partir do offset capturado lê os três registos de
  volta, **intactos** (round-trip CloudEvents JSON, ids a bater); um segundo
  `forward` reenvia **zero** (o cursor já os cobre).
