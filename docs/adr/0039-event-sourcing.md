# ADR 0039 — Event store e Event Sourcing (`dorm-eventstore`, L12)

Status: Accepted — núcleo implementado e provado ao vivo · 2026-11-03

## Context

O ADR 0035 fechou o lado de **leitura** do CQRS (read models por projeção sobre o
`dorm-outbox`) mas **adiou explicitamente** o event store propriamente dito:

> **Por fazer (adiado — subsistema maior):** o event store propriamente dito
> (ordenação por `xid8`, snapshots de agregado, upcasting de versões de evento)
> assenta no mesmo log mas é um subsistema à parte, para fase própria.

Este ADR é essa fase. Event sourcing significa que a **fonte de verdade** deixa de
ser o estado atual e passa a ser um **log append-only de eventos imutáveis**, um
fluxo (*stream*) por agregado; o estado é um *fold* (reconstrução) sobre o fluxo.
A diferença face ao `dorm-outbox` (L6) é a intenção: o outbox é um mecanismo de
publicação de mudanças (um evento existe sse a escrita de negócio existiu); o
event store **é** o modelo de escrita — não há tabela de estado por trás, o fluxo
é o registo. São complementares, não concorrentes, e por isso vivem em crates
distintos.

As três peças que faltavam são precisamente: controlo de concorrência otimista no
`append`, snapshots de agregado para reconstrução rápida, e upcasting de versões
de evento ao carregar.

## Decision

Um crate **opcional** novo, `dorm-eventstore`, com o padrão dos irmãos
(`dorm-outbox`/`dorm-jobs`): dependência **unidirecional** de `dorm-core`
(o núcleo nunca depende dele), DDL com `IF NOT EXISTS`, nomes de tabela em `pub
const`, e todo o SQL através de `dorm_core::db`.

### Esquema

- **`dorm_events`** — o log append-only:
  `global_seq BIGSERIAL PRIMARY KEY`, `aggregate_type`, `aggregate_id`,
  `version INT`, `event_type`, `event_version INT`, `data JSONB`,
  `metadata JSONB`, `recorded_at`. A chave da correção é a **restrição
  `UNIQUE (aggregate_type, aggregate_id, version)`**: não há dois eventos na mesma
  versão de um agregado.
- **`dorm_snapshots`** — `PRIMARY KEY (aggregate_type, aggregate_id)`, com
  `version INT` e `state JSONB`: um snapshot (o mais recente) por agregado.

### Ordenação total: `BIGSERIAL`, não `xid8` — e porquê

O pedido original (e o ADR 0035) apontava `xid8`/`pg_current_xact_id` para a ordem
total. **Escolhemos `BIGSERIAL`**, por três razões:

1. **A ordem que importa de verdade é por agregado**, e essa é a coluna `version`,
   estritamente 1-based e sem lacunas, imposta pela restrição `UNIQUE`. É sobre
   essa ordem que o `load` reconstrói e sobre ela que assenta toda a consistência;
   `global_seq` serve apenas a leitura do fluxo global (todos os agregados).
2. **A armadilha `Value::ToSql`** (ADR 0037): `Value::ToSql` recusa ligar um valor
   a um tipo incompatível, e `xid8` não tem representação nativa em `Value`. Para o
   usar seria preciso ligar tudo como `Value::Text` e reconverter em SQL
   (`($n || '')::xid8`) — fricção real em todas as comparações. `BIGSERIAL` é
   `bigint` nativo: liga-se como `Value::I64` sem *cast* nenhum, longe da
   armadilha.
3. **Honestidade sobre o que `BIGSERIAL` dá e não dá** (ver *Proof*/Consequences):
   dá uma ordem total monotónica por **ordem de inserção** (o valor da sequência é
   atribuído no `INSERT`). **Não** é estritamente a ordem de **visibilidade de
   commit** sob concorrência — uma transação com `global_seq` menor pode fazer
   commit (tornar-se visível) depois de outra com `global_seq` maior, deixando uma
   lacuna transitória. Isto é a conhecida anomalia de *gaps* das sequências e
   afeta igualmente qualquer leitor do fluxo **global**. `xid8` com
   `pg_snapshot_xmin` resolveria isto para um *subscriber* global com garantias de
   não-perda; fica registado como limitação conhecida, não escondida. Para o
   percurso deste ADR (append/load **por agregado**) é irrelevante, porque aí a
   ordem é `version`, serializada pela restrição `UNIQUE`.

### API

- **`append(aggregate_type, aggregate_id, expected_version, &[NewEvent]) -> Result<i32>`**
  dentro de `db::atomic`. Lê o `MAX(version)` atual; se não bater em
  `expected_version` devolve **`Error::Conflict`** (reutilizado do `dorm-core`:
  `model` = `aggregate_type`, `expected`, `actual` = versão real) e não escreve
  nada. Insere nas versões seguintes. A restrição `UNIQUE` é o **backstop** para o
  caso de corrida que o `MAX` não vê (um *appender* concorrente ainda não
  commitado): o `23505` é remapeado para o mesmo `Error::Conflict`.
- **`load(aggregate_type, aggregate_id) -> Vec<StoredEvent>`** por ordem de
  `version`.
- **`save_snapshot(...)`** com *upsert* que recusa recuar (`WHERE version <=
  EXCLUDED.version`); **`load_from_snapshot(...) -> (Option<Snapshot>,
  Vec<StoredEvent>)`** devolve o snapshot e só os eventos **desde** a sua versão —
  a reconstrução rápida de um agregado longevo.
- **`Upcasters`** — registo de `fn(JSON) -> JSON` por `(event_type,
  from_version)`. `upcast` encadeia (v1→v2→v3…) até não haver transformação para a
  versão atual; `Upcasters::load` carrega e aplica a cada evento. Os eventos em
  disco **nunca** são reescritos: o upcasting acontece ao carregar, para que o
  *fold* só veja o formato atual.

## Consequences

- **Prós:** modelo de escrita por fluxo append-only com concorrência otimista real
  (dupla barreira: `MAX(version)` + restrição `UNIQUE`); snapshots cortam o custo
  de reconstrução de agregados longevos; upcasting desacopla o formato em disco do
  formato que o domínio vê, sem migração destrutiva de eventos. Crate opcional,
  grafo de `dorm-core` inalterado para quem não o usa.
- **Limitação conhecida (honesta):** `global_seq` é ordem total por **inserção**,
  não por **visibilidade de commit** — há lacunas transitórias sob concorrência.
  Só afeta um leitor do fluxo **global**; o append/load por agregado é imune
  (ordem = `version`). Um *subscriber* global com garantia de não-perda precisaria
  de `xid8`/`pg_snapshot` (fase futura, fora deste ADR).
- **Por fazer (fora de escopo):** projeção automática do event store para read
  models (hoje liga-se pela mão via `dorm-outbox`); compactação/arquivo de fluxos
  antigos; *subscribe* ao fluxo global com cursor (o L6 já o faz para o outbox).

## Proof

- **Offline (`cargo test -p dorm-eventstore`, parte do `make check`):** a DDL cria
  ambas as tabelas com a unicidade de fluxo e o `global_seq BIGSERIAL`; o
  encadeamento de upcasters v1→v2→v3 e a paragem no formato atual; o remapeamento
  de `23505` para `Error::Conflict` (e a passagem incólume de outro erro de BD).
- **Live (`crates/dorm-eventstore/tests/eventstore_live.rs`, `#[ignore]`):**
  contra um PostgreSQL real, com `aggregate_id` único por *run* e `TRUNCATE` no
  início (re-executável):
  1. *append* de três eventos a um agregado novo → versão 3;
  2. um *append* com `expected_version` desatualizado (1) é recusado com
     `Error::Conflict { expected: 1, actual: Some(3) }` e não escreve nada;
  3. `load` reconstrói o fluxo estritamente por `version` (1,2,3) e `global_seq` é
     monotónico;
  4. `save_snapshot` na versão 4 + `load_from_snapshot` devolve o snapshot e **só**
     o evento na versão 5 (reconstrução rápida: snapshot ⊕ cauda);
  5. um evento gravado em `event_version = 1` é carregado já **upcasted** para v2
     (o `data` v1 `{"name":"Ada Lovelace"}` ganha `first`/`last`), com o registo em
     disco a continuar em v1.

  Comando:

  ```text
  DELONIX_ORM_TEST_DSN='host=/var/run/postgresql port=5432 user=walter dbname=ormtest' \
    cargo test -p dorm-eventstore --test eventstore_live -- --ignored --nocapture
  ```
