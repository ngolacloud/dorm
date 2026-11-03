# ADR 0029 — Partições por intervalo e retenção (L10)

Status: Accepted — núcleo implementado (Fase 6) · 2026-11-03

## Context

As tabelas só-de-acréscimo — auditoria, consumo, eventos, o outbox/inbox — crescem
sem limite. Ao fim de dez anos serão as maiores da plataforma. Sem partições,
cada índice é gigante, a retenção é um `DELETE` de milhões de linhas (lento e com
*bloat*), e o planeador não consegue podar por tempo. O DORM não tinha nada:
`grep` por `PARTITION BY`/`DETACH`/`retain` era vazio.

## Decision

Particionamento por **intervalo mensal** com criação antecipada e retenção,
como funções utilitárias no `dorm-migrations` (PostgreSQL). Opt-in: uma tabela
torna-se particionada quem a declara assim.

- **`create_parent_ddl(table, columns, range_column)`** — emite
  `CREATE TABLE … PARTITION BY RANGE (<col>)`. (O PostgreSQL exige a coluna de
  partição em toda a chave única, por isso uma tabela só-de-acréscimo
  particionada chaveia em `(id, <col>)`.)
- **`ensure_month_partitions(table, months_ahead)`** — cria as partições do mês
  corrente e dos próximos `months_ahead`, se faltarem. Corre num *schedule* (um
  job do `dorm-jobs`) para que a partição exista **antes** do mês que a precisa;
  as fronteiras são datas que o próprio PostgreSQL calcula, nunca strings que
  montámos.
- **`drop_partitions_older_than(table, retain_months)`** — a retenção: destaca e
  larga as partições mensais além da janela, com `DETACH … CONCURRENTLY` (não
  tranca o pai) seguido de `DROP`. Devolve os nomes largados. A fronteira é o
  sufixo `YYYYMM` de corte; como é de largura fixa, uma comparação lexicográfica
  é uma comparação de data.

### Como preserva os invariantes do `ARCHITECTURE.md`

Tudo vive no `dorm-migrations` (DDL/manutenção), fora do compilador de consultas,
que permanece puro. Os nomes de tabela/coluna/schema são validados como
identificadores antes de entrarem no DDL (configuração, não parâmetros ligados);
as fronteiras de data vêm do servidor. As partições são transparentes à ORM — uma
consulta à tabela-pai é roteada pelo PostgreSQL para a partição certa.

## Consequences

- **Prós:** índices pequenos por mês; poda por tempo no planeador; retenção é um
  `DETACH`/`DROP` barato de um mês inteiro em vez de um `DELETE` massivo; a criação
  antecipada evita o erro "no partition for row". Aplicável a auditoria, consumo,
  eventos, outbox e inbox.
- **Contras / custo:** a tabela tem de nascer particionada (ou ser migrada para
  tal — fora deste brick); a chave única passa a incluir a coluna de intervalo;
  `DETACH CONCURRENTLY` não corre dentro de uma transacção (estas funções correm
  em autocommit, que é o correcto). Falta ergonomia a seguir: a sintaxe
  `@@partition(by: range(createdAt), interval: month)` + `retain` no `dorm.schema`,
  que gerará estas chamadas.
- **Dependências:** `db::sql`/`db::execute` do núcleo; a criação antecipada e a
  retenção correm bem como jobs do `dorm-jobs` (ADR 0027).

## Proof

- **Live (`dorm-models/tests/partitions_live.rs`):** uma tabela particionada por
  `at`; `ensure_month_partitions` cria o mês corrente e o seguinte; uma linha a
  `now()` é roteada para a partição corrente e lê-se de volta; uma partição de
  2019 com uma linha antiga é **destacada e largada** por
  `drop_partitions_older_than(…, 24)`, enquanto a partição corrente fica intacta.
- **Unit:** o DDL do pai particiona por `RANGE (col)`; nomes com injecção são
  recusados.
