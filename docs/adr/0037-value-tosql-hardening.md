# ADR 0037 — Endurecer `Value::ToSql` contra tipos incompatíveis

Status: Accepted — implementado · 2026-11-03

## Context

Quatro vezes nesta campanha (ADR 0025 `::numeric`, 0027 `::interval`, 0030
`::pg_lsn`, e o `generate_series`) um parâmetro ligado a um tipo que o servidor
inferia diferente do tipo nativo da variante foi escrito **à letra** — bytes de
`i64` num campo `numeric`, texto num `interval` — dando valor-lixo ou `08P01` em
vez de um erro claro. A causa: `impl ToSql for Value` chamava o `to_sql` **cru** de
cada tipo interno, que escreve os bytes sem consultar o `accepts` do tipo; o
`accepts` do próprio `Value` devolvia `true` para tudo (tem de o fazer — corre
antes de a variante ser conhecida). O gate de compatibilidade era, assim, saltado.

## Decision

Cada braço de `Value::to_sql` passa a delegar no **`to_sql_checked`** do tipo
interno (não no `to_sql` cru). O `to_sql_checked` consulta o `accepts` desse tipo
e devolve `Err` se o `ty` do servidor for incompatível — um erro de tipo limpo em
vez de corromper. O `accepts` do `Value` mantém-se abrangente; a verificação real
por-variante fica onde a variante é conhecida.

## Consequences

- **Prós:** fecha a classe de bug de uma vez — um `i64` ligado a um parâmetro
  `numeric`/`interval`/`pg_lsn` falha com um erro de tipo, não com lixo silencioso.
  Os contornos legítimos (`::bigint` nativo, `($n || '…')::interval`/`::pg_lsn`
  ligados como texto) continuam a funcionar, porque aí o `ty` inferido **é** texto
  ou o tipo nativo.
- **Contras / custo:** código que (por acaso) dependesse da escrita permissiva
  passa a falhar — correctamente. Verificado: `make check` verde e toda a suite
  live (dorm-models 24/24, outbox/jobs/messaging/mcp) passa sem alteração; nada
  dependia do comportamento permissivo.

## Proof

- **Unit (`value::tests`):** `Value::I64(5).to_sql_checked(&Type::NUMERIC, …)` é
  `Err`; texto para `INT8` é `Err`; os casos coincidentes (`i64`→`INT8`,
  texto→`TEXT`) são `Ok`.
- **Regressão:** a suite live completa passa pós-endurecimento (sem
  `cannot convert`/`08P01` em lado nenhum).
