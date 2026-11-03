# ADR 0050 — Routing de leitura multi-réplica: balanceamento + ejeção (L8)

Status: Accepted — balanceamento + ejeção reactiva + fallback implementados (Fase 7) · 2026-11-03

## Context

O [[0010]] (B-13) deu ao `Database` um pool **primário** + **uma** réplica
opcional, e o [[0030]] deu as primitivas de read-your-writes
(`write_lsn`/`replica_caught_up`/`replica_lag_seconds`). O que ficou adiado
(declarado no [[0030]]): **várias** réplicas, **balanceamento** entre elas e
**ejeção** de uma que adoeça. Sem isto, uma frota de leitura é um único ponto de
carga e de falha.

Medição vs. ponto de partida: `Database` tinha `replica: Option<Pool>` e
`pool_for(route)` devolvia `self.replica.as_ref().unwrap_or(&self.primary)` — uma
só réplica, sem balanceamento, e uma réplica em baixo fazia a leitura falhar (o
pool errava em vez de tentar outra).

## Decision

`Database` passa a ter `replicas: Vec<Replica>` (em vez de `Option<Pool>`), com um
cursor round-robin (`AtomicUsize`) e, por réplica, um relógio de ejeção
(`ejected_until: AtomicU64`, segundos Unix; `0` = saudável). `pool_for` é
substituído por `Database::acquire(route)`:

- **Balanceamento:** uma `Route::ReadReplica` (fora de transação) escolhe réplicas
  em round-robin a partir do cursor, saltando as que estão em cooldown.
- **Ejeção reactiva:** se o `pool.get()` de uma réplica erra, ela é ejectada por um
  cooldown (`DELONIX_ORM_REPLICA_EJECT_COOLDOWN_SECS`, por omissão 10 s) e a
  leitura tenta a seguinte — tudo na mesma chamada, sem erro para o chamador. A
  réplica **reentra sozinha** quando o cooldown passa (sem tarefa de fundo).
- **Fallback:** se todas as réplicas estão ejectadas ou a falhar, a leitura vai ao
  **primário** em vez de errar.
- **Invariantes preservadas:** dentro de uma transação, ou para a rota primária, a
  ligação continua a ser a do primário (uma transação não se parte entre primário
  e réplica, e tem de ler as suas próprias escritas) — como antes.

Configuração: `DELONIX_ORM_REPLICA_DSN` aceita agora **várias** DSNs separadas por
vírgula; `Database::connect_replicas(primary, &[..], read_only)` é o equivalente
tipado. `connect(primary, Option<&str>, ..)` mantém-se (uma DSN, ou várias por
vírgula). `replica_count()` expõe o número configurado.

## Consequences

- **Prós:** a leitura escala horizontalmente (round-robin por uma frota) e
  tolera a perda de uma réplica sem intervenção — ejeção automática + fallback ao
  primário. Zero configuração para o caso comum; um knob para o cooldown.
- **Por fazer (declarado, honesto):** a ejeção é **reactiva** (falha de ligação).
  A ejeção **proactiva por lag** — tirar de serviço uma réplica demasiado atrasada
  — fica como política do chamador sobre a primitiva `replica_lag_seconds` do
  [[0030]] (medida ao vivo: um standby ocioso reportou ~274 s), ou como sondagem
  de fundo numa ADR própria. Pesos por réplica e `least-connections` idem (o
  round-robin é o primeiro corte). A escolha da réplica não consulta o lag no
  caminho quente (acrescentaria uma ida ao servidor por leitura).

## Follow-up landed

**Ejeção proactiva por lag:** `Database::eject_lagging_replicas(max_lag_secs)` —
sonda, no pool de cada réplica, `pg_last_wal_receive_lsn() = pg_last_wal_replay_lsn()`
(caught-up → lag 0, mesmo num primário ocioso, evitando o falso-positivo do
`pg_last_xact_replay_timestamp` isolado) e, com folga, o delta temporal; ejecta as
réplicas acima do limite (e as inalcançáveis) pelo mesmo cooldown, pelo que
reentram sozinhas ao apanhar. Conduzido-pelo-chamador (a app corre-o num temporizador),
fora do caminho quente de leitura. Prova live
(`replica_lag_ejection_live.rs`): uma réplica caught-up não é ejectada; com o
replay pausado e uma escrita no primário (received ≠ replayed), passado o limite
é ejectada e a leitura cai no primário; ao retomar, recupera.

## Proof

- **Offline:** `make check` verde — `Database` recompila com a frota; o caminho de
  uma-só-réplica/sem-réplica é o mesmo comportamento de antes.
- **Live (`multi_replica_routing_live.rs`, PostgreSQL 16, 1 primário :5434 + 2
  standbys streaming :5435/:5436):** cada backend responde a `inet_server_port()`
  com a sua porta, que testemunha onde a leitura caiu:
  1. **Balanceamento** — 20 leituras sobre 2 réplicas saudáveis atingem **ambas**
     as portas (nunca a do primário).
  2. **Ejeção** — com uma réplica saudável e uma DSN morta (porta sem ninguém a
     ouvir), **todas** as leituras passam e vão todas à réplica saudável (a morta é
     ejectada, não erra a leitura).
  3. **Fallback** — com as duas réplicas mortas, a leitura cai no **primário**.
  4. **Escrita/transação** — a rota primária ignora as réplicas e atinge sempre o
     primário.
  Replicação streaming confirmada por `pg_stat_replication` (2 walreceivers) e por
  uma escrita no primário visível em ambos os standbys.
