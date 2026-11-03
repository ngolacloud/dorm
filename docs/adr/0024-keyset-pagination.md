# ADR 0024 — Paginação por keyset com cursor assinado

Status: Accepted — implementado (Fase 3) · 2026-11-03

## Context

A paginação é exclusivamente `OFFSET`/`LIMIT`. O compilador emite
`LIMIT {n} OFFSET {n}` inline (`crates/dorm-core/src/compiler.rs:824-829`); a API
gerada faz `.limit(p.limit.unwrap_or(50)).offset(p.offset.unwrap_or(0))`
(`crates/dorm-schema/src/codegen_api.rs:153`); o snapshot confirma
`… ORDER BY … LIMIT 10 OFFSET 20`. Três problemas para escala e para dez anos de
dados:

- **`OFFSET N` lê e descarta N linhas** — o custo cresce com a profundidade; a
  páginas fundas numa tabela grande é O(N).
- **A ordem não é estável.** `compile_order` (`compiler.rs:736-770`) emite só os
  termos declarados/explícitos e **não acrescenta desempate pela PK** — duas
  linhas com a mesma chave de ordenação podem trocar de página entre pedidos.
- **Não há cursor.** `grep` por `keyset`/`.after(`/`cursor`/`seek` em
  `queryset.rs`/`compiler.rs`/`codegen_api.rs` é vazio; existe só `offset(n)`
  (`queryset.rs:386-388`).

## Decision

- **Ordem estável, sempre.** `compile_order` passa a **acrescentar a PK** como
  termo de desempate final (na mesma direcção do último termo) quando ela lá não
  esteja. Torna qualquer ordenação total e determinística — pré-requisito do
  keyset e reutilizado pelo backfill do ADR 0022.
- **`.page(size).after(cursor)` no `QuerySet`.** Em vez de `OFFSET`, compila um
  predicado keyset sobre os valores da última linha da página anterior
  (comparação lexicográfica por tuplo de `(ordem…, pk)`), traduzido em
  `WHERE (o1, o2, pk) > ($1, $2, $3)` com a semântica correcta por direcção de
  cada termo. Usa o índice da ordenação; nunca `OFFSET`.
- **Cursor opaco e assinado.** O cursor codifica os valores de fronteira **e** uma
  impressão digital da consulta (modelo, ordem, filtros), selada com HMAC sobre
  uma chave configurável (`DELONIX_ORM_CURSOR_KEY`). Decodificar valida a
  assinatura e a impressão digital: um cursor de **outra** consulta (ordem/filtros
  diferentes) é recusado com erro, não aplicado a linhas erradas. Os valores de
  fronteira viajam como `Value`s ligados, nunca texto no SQL.
- **API gerada.** As rotas de listagem passam a devolver um cursor `next` e a
  aceitar `?after=`, com um tecto de `size` — substituindo o `limit/offset` sem
  tecto de `codegen_api.rs:153`.

### Como preserva os invariantes do `ARCHITECTURE.md`

- **I1:** os termos de ordem e a PK de desempate são `&'static FieldMeta`
  resolvidos pelo registry; o cursor não transporta identificadores — só valores e
  uma assinatura que o servidor verifica.
- **I2:** os valores de fronteira do keyset são ligados como `$n`.
- **I3:** o predicado keyset e o desempate por PK são texto puro que o compilador
  emite do `Plan` (entram no *snapshot*); a selagem/validação HMAC do cursor é
  comportamento de fronteira, fora do compilador.

## Consequences

- **Prós:** páginas de custo constante independentemente da profundidade; ordem
  estável sob escrita concorrente; um cursor não pode ser reutilizado noutra
  consulta. O desempate por PK beneficia qualquer `ORDER BY`, não só a paginação.
- **Contras / custo:** o keyset não dá "salta para a página 500" (é navegação
  sequencial — a troca certa a esta escala); a impressão digital no cursor obriga
  a que a consulta seja idêntica entre páginas; `OFFSET` mantém-se disponível para
  casos pequenos. Os *snapshots* de ordenação mudam (ganham a PK de desempate).
- **Dependências:** nenhuma a montante; a ordem estável é consumida pelo backfill
  do ADR 0022.

## Implementação (Fase 3)

- O preditor vive em `compiler::compile_keyset` (expansão lexicográfica OR, cada
  termo `>`/`<` conforme a direcção) e integra-se no `WHERE` do `compile_select_on`;
  a ordem total é dada por `Plan::effective_order`, que acrescenta a PK.
- `Plan` ganha `keyset: Option<Vec<Value>>` e `paginate: bool`. **Desvio
  deliberado:** o desempate por PK aplica-se só quando `paginate`/`keyset` estão
  activos (`QuerySet::fetch_page` liga `paginate`), **não** em todas as queries —
  evita reescrever ~40 snapshots congelados. A página 1 já ordena pela PK (o flag
  liga-se antes do primeiro fetch), por isso nenhuma linha com chave igual se perde
  entre páginas.
- `QuerySet::fetch_page(size) -> Page<T>` e `after(cursor)`; `Page { items, next }`.
  O `next` é cunhado a partir dos valores de chave da última linha (`keyset_boundary`).
- Cursor em `cursor.rs`: valores de fronteira + *fingerprint* da query (modelo,
  ordem, filtros), selados com **HMAC-SHA256** (`DELONIX_ORM_CURSOR_KEY`,
  default de dev). **Nota de dependência:** `hmac`/`sha2`/`base64` (já na árvore do
  workspace) entram no `dorm-core` — facilmente *feature-gated* se um build mínimo
  as quiser fora.
- `QuerySet::sql()` (preview) fica no compilador puro (não pagina), como na L1.
- **Por fazer (follow-up):** a integração na API gerada (`?after=`/`next`), e o
  snapshot de `EXPLAIN` (a ausência de `OFFSET` já é provada pela forma do SQL).

## Proof

- **Unit (offline):** `cursor::tests` (ida-e-volta sob a própria *fingerprint*,
  recusa de cursor de outra query, recusa de cursor adulterado, precisão de
  timestamp ao microssegundo); `pagination_live::the_keyset_query_has_no_offset_and_a_tuple_predicate`
  (o SQL tem o tuplo OR, termina em `LIMIT`, a PK é o último termo do `ORDER BY`,
  **sem `OFFSET`**).
- **Live (provado, `dorm-models/tests/pagination_live.rs`):** 25 linhas com chave
  de ordenação repetida (`bucket`), percorridas em páginas de 10 — cada linha
  aparece **exactamente uma vez**, na ordem total `(bucket, id)`, em 3 páginas; a
  última página tem `next = None`; um cursor desta query é recusado por uma query
  com filtro diferente (`different query`).
