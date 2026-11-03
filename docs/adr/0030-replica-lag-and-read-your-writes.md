# ADR 0030 — Réplicas com consciência de atraso e read-your-writes (L8)

Status: Proposed · 2026-11-03 — **primitivas RYW implementadas (Fase 6)**

## Context

O routing de réplica actual (ADR 0010) tem **uma** réplica (`Option<Pool>`), sem
medição de atraso, sem ejecção, sem balanceamento, e sem read-your-writes fora de
uma transacção. Um leitor que acabou de escrever e lê de uma réplica atrasada não
vê a sua própria escrita.

## Decision

A peça nuclear e independente de infra — o **token read-your-writes** — entra já;
o routing multi-réplica assenta nela.

- **Token = LSN de escrita do primário.** `db::write_lsn()` captura
  `pg_current_wal_lsn()` logo após a escrita. É opaco; a sua única operação é o
  `>=` que o PostgreSQL define em `pg_lsn`.
- **Catch-up da réplica.** `db::replica_caught_up(token)` corre
  `pg_last_wal_replay_lsn() >= token` na ligação da réplica. Numa consulta
  `ReadYourWrites(token)`, se a réplica alcançou o token lê-se dela; senão
  espera-se até um timeout ou cai-se para o primário (que trivialmente tem as suas
  escritas — por isso numa não-réplica o check é `false`, o sinal para ir ao
  primário).
- **Medição de atraso.** `db::replica_lag_seconds()` via
  `pg_last_xact_replay_timestamp()` — `None` num primário. É o que um router
  consciente de atraso usa para ejectar uma réplica muito atrasada.

### Armadilha sistémica confirmada (3ª vez)

Todos os três casts de parâmetro ligado a um tipo não-escalar
(`$n::numeric`→ADR 0025, `$n::interval`→ADR 0027, `$n::pg_lsn`→aqui) caíram na
mesma armadilha: o `ToSql` do `Value` escreve os bytes do tipo **nativo** da
variante mesmo quando o servidor infere outro tipo do operando, por o `accepts`
do `Value` aceitar qualquer tipo. O contorno é forçar a inferência para o tipo
nativo — `($n || '')::pg_lsn`, `($n || ' milliseconds')::interval`, ou o cast
nativo (`::bigint`). **Follow-up recomendado:** endurecer o `Value::ToSql`
(`accepts`/`to_sql`) para recusar um `ty` incompatível em vez de escrever lixo.

## Consequences

- **Prós:** o token RYW resolve o "ler a própria escrita" sem transacção, a peça
  que faltava; a medição de atraso dá o sinal de ejecção.
- **Por fazer (adiado — precisa de réplica streaming):** o refactor do handle
  `Database` para **várias** réplicas; as estratégias de balanceamento
  (`round_robin`, `least_outstanding`, `p2c_ewma`); a ejecção acima de `max_lag`
  com histerese; e o routing que consome `ReadYourWrites(token)` escolhendo réplica
  ou primário. A **prova** destes exige um par primário/réplica com replicação
  real (o acceptance pede `pg_wal_replay_pause()` numa réplica) — infra que o
  ambiente de teste de instância única não tem.
- **Dependências:** `db::fetch`/`db::sql` do núcleo.

## Proof

- **Live (`dorm-core/tests/read_your_writes_live.rs`, instância única):** o token
  `write_lsn()` **avança** de uma escrita para a seguinte (a comparação `pg_lsn`
  que uma leitura RYW faz); `replica_caught_up` reporta `false` num primário (sem
  replay LSN, o sinal para ir ao primário); `replica_lag_seconds` é `None` num
  primário.
- **Adiado:** `pg_wal_replay_pause()` numa réplica + routing multi-réplica, por
  exigirem replicação em streaming.
