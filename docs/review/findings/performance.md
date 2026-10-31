# Eixo Desempenho e Escala — achados (Fase 1)

> Revisor: eixo *performance*. Hipóteses de entrada: **H2** (cache de statements
> sem limite), **H8** (lacunas que forçam N+1 e planos maus), **H9** (`bulk_create`
> sem *chunking*). Método: análise estática com evidência `ficheiro:linha` e cálculo;
> as medições vivas estão descritas mas **não** foram corridas nesta fase (ver §
> Protocolo no fim). Nenhuma alteração de código. Português europeu; identificadores
> em inglês.

Ordenados por severidade.

---

## [PERF-01] Cache de *prepared statements* sem limite nem política de evicção — Alto

- **Hipótese:** H2 (confirmada) + interage com H8/H9.
- **Evidência:** `db.rs:634-671` e `db.rs:693-705`. Tanto `query_inner` como
  `execute_inner` chamam `conn.prepare_cached(sql)` incondicionalmente, no ramo de
  transacção e no ramo normal:
  ```
  let stmt = conn.prepare_cached(sql).await.map_err(from_pg)?;
  ```
  A cache é a `StatementCache` do `deadpool-postgres` (ADR 0007), **por conexão**,
  **sem tecto** e **sem LRU**: a chave é o texto SQL exacto. Não há nenhuma
  constante de limite nem chamada de limpeza em todo o `db.rs` (nem
  `POOL_MAX_ENV`-equivalente para statements). Cada texto SQL distinto que passe
  pela conexão fica preparado no servidor (`PREPARE`) **para sempre**, até a
  conexão ser reciclada/fechada.
- **Impacto:** o caminho de acesso do ORM gera texto SQL *variável* em três
  situações comuns, e cada variante é uma entrada nova de cache **e** um
  prepared statement do lado do servidor:
  1. `IN (…)` expandido — um texto por cada cardinalidade de lista (ver PERF-03);
  2. `bulk_create` — um texto por cada número de linhas N (ver PERF-02);
  3. `UPDATE/DELETE … WHERE pk IN (subquery)` cujo *shape* varia com o número de
     filtros/joins.
  Com `max_size` de pool a 8 (`POOL_MAX_DEFAULT`, `db.rs:65`), 8 conexões ×
  milhares de variantes = crescimento de memória no cliente **e** no
  `pg_prepared_statements` do servidor, que consome `work_mem`/catálogo e degrada
  o *planner*. Em modo PgBouncer *transaction* o `prepare_cached` é simplesmente
  incorrecto: a conexão lógica muda entre transacções e o statement nomeado
  desaparece (erro `prepared statement "s0" does not exist`). É o risco mais
  estrutural do eixo porque junta fuga de memória a incompatibilidade de
  *pooling* externo.
- **Reprodução:**
  - *Memória/catálogo*: correr um laço de 10 000 `filter_str("id__in", lista)`
    com listas de cardinalidade crescente (1..5000) contra o PG 17 descartável
    (`127.0.0.1:55432`) e observar `SELECT count(*) FROM pg_prepared_statements`
    a subir sem limite por conexão, e o RSS do processo cliente.
  - *PgBouncer*: subir PgBouncer em `pool_mode=transaction` à frente do PG e
    correr qualquer `query` repetida; esperar o erro de statement inexistente.
- **Recomendação (mínima):** tornar a cache limitada e observável. Duas peças:
  (a) um tecto LRU por conexão (`DELONIX_ORM_STMT_CACHE_MAX`, com default p.ex.
  256) — o `deadpool` expõe `StatementCache`, mas a política precisa de ser
  explícita; (b) um modo "sem cache" (`prepare` simples ou *no-prepare*) activável
  para ambientes PgBouncer *transaction*. Reduzir a variabilidade do texto (PERF-02,
  PERF-03) ataca a causa; o tecto trata do resto.
- **Esforço:** M
- **ADR/Tijolo:** estende ADR 0007; novo ADR "limite e política da *statement
  cache* + compatibilidade PgBouncer".

---

## [PERF-02] `bulk_create` num só `INSERT` sem *chunking* — estoura 65 535 parâmetros e polui a cache — Alto

- **Hipótese:** H9 (confirmada).
- **Evidência:** `queryset.rs:149-158`:
  ```
  let values: Vec<Vec<(String, Value)>> = rows.iter().map(NewRow::column_values).collect();
  let sql = compiler::compile_insert_on(T::meta(), &values, db::dialect())?;
  ```
  e `compiler.rs:1019-1087` (`compile_insert_on`) emite um único
  `INSERT INTO … VALUES (…),(…),… RETURNING …` com **um placeholder por célula de
  cada linha** (`binder.bind(...)` dentro do duplo laço `compiler.rs:1039-1056`).
  Não há partição em lotes em lado nenhum.
- **Impacto:** o protocolo *extended query* do PostgreSQL limita um statement a
  **65 535** parâmetros (`int16`). Como o número de parâmetros é
  `N_linhas × colunas_por_linha`, o tecto de linhas por chamada é
  `floor(65535 / colunas_por_linha)`:

  | colunas inseridas/linha | máx. linhas antes do erro |
  |---|---:|
  | 2 (p.ex. `Tenant`: slug, name) | 32 767 |
  | 3 | 21 845 |
  | 4 (p.ex. `Deployment`: app_id, replicas, status, config) | 16 383 |
  | 8 | 8 191 |
  | 16 | 4 095 |

  Acima disto, `bulk_create` falha com erro de protocolo do driver — não com um
  erro do ORM que explique o limite. Pior: **cada N distinto gera um texto SQL
  distinto** (número de tuplos `VALUES` diferente), por isso alimenta
  directamente PERF-01 — um `bulk_create` chamado com tamanhos variados polui a
  cache com uma entrada por tamanho. E um único `INSERT` gigante também monopoliza
  uma conexão e uma transacção implícita longa (contenção de locks, WAL).
- **Reprodução:** inserir 70 000 linhas de `Deployment` (4 col/linha) de uma vez
  contra o PG descartável → erro de protocolo (`number of parameters must be
  between 0 and 65535`). Depois medir *throughput* (linhas/s) de 1k e 10k linhas
  com e sem *chunking*.
- **Recomendação (mínima):** particionar em lotes de tamanho **fixo** calculado a
  partir de `colunas_por_linha` (p.ex. alvo de ≤ 8 000 parâmetros/lote,
  `chunk = 8000 / cols`), emitidos dentro de um `atomic()` para manter a
  atomicidade que o método promete. O tamanho fixo tem um benefício duplo: os
  lotes "cheios" repetem o **mesmo** texto SQL → uma só entrada de cache (alivia
  PERF-01). A médio prazo, avaliar `UNNEST($1::int[], $2::text[], …)` (texto único
  independente de N) ou `COPY` para cargas grandes.
- **Esforço:** S (chunking) / M (UNNEST/COPY).
- **ADR/Tijolo:** novo ADR "bulk_create por lotes + caminho de carga em massa
  (UNNEST/COPY)".

---

## [PERF-03] `IN` expandido para `$1,$2,…,$n` em vez de `= ANY($1)` — explosão combinatória de textos e statements — Médio

- **Hipótese:** H8 (confirmada) + causa-raiz de PERF-01.
- **Evidência:** `compiler.rs:332-343` (`Lookup::In`):
  ```
  let placeholders = values.iter().cloned().map(|v| binder.bind(v))
      .collect::<Vec<_>>().join(", ");
  format!("{operand} IN ({placeholders})")
  ```
  A decisão é deliberada e está documentada em `value.rs:78-83` ("There is no
  `Array` variant: the `in` lookup expands to `IN ($1, $2, $3)` rather than
  `= ANY($1)`") — justificada pela ausência de um `Value::Array` e pela
  dificuldade de inferir o tipo do array para listas montadas pelo REPL.
- **Impacto:** o texto SQL passa a depender da **cardinalidade** da lista. Uma
  lista de 10 elementos e uma de 11 são dois statements distintos; um filtro
  `pk__in` usado com listas de tamanho arbitrário (o resultado natural de um
  `m2m.keys()` → `filter("pk__in", keys)`, ver PERF-04) gera uma entrada de cache
  por tamanho observado. Isto é o motor principal da fuga de PERF-01. `= ANY($1)`
  seria **um** texto para qualquer cardinalidade, prepara uma vez, e o PostgreSQL
  planeia-o igualmente bem (ou melhor, evita re-planeamento). Secundariamente,
  listas muito grandes em `IN (…)` aproximam-se do mesmo tecto de 65 535
  parâmetros de PERF-02.
- **Reprodução:** compilar `filter_str("id__in", v)` para `v` de tamanho 1..1000 e
  contar textos SQL distintos (1000) vs. 1 com `= ANY`. Em runtime, medir
  `pg_prepared_statements` após o laço.
- **Recomendação (mínima):** no `Dialect::Postgres`, compilar `IN` como
  `{operand} = ANY({placeholder})` com um único parâmetro de array, introduzindo
  um `Value::Array(Vec<Value>)` homogéneo (o tipo do elemento vem do `FieldType`
  da coluna resolvida, que o compilador **já** conhece em `compile_predicate`,
  `compiler.rs:251` `let ty = field.ty`). SQLite mantém a expansão (não tem
  `ANY`), o que é compatível com o *seam* de dialectos já existente. Preserva a
  invariante de parâmetro ligado. Protegido por *snapshot* para não derivar.
- **Esforço:** M (toca `value.rs`, `compiler.rs`, `ToSql` e snapshots).
- **ADR/Tijolo:** novo ADR "lookup `in` como `= ANY` + `Value::Array` tipado por
  coluna"; coordena com o eixo DBA (tipos).

---

## [PERF-04] N+1 estrutural: sem `prefetch_related`, `select_related` só FK directa por string, M2M com round trip duplo — Médio

- **Hipótese:** H8 (confirmada).
- **Evidência:**
  - **Sem `prefetch_related`:** `grep -rn prefetch crates/` → zero ocorrências.
    `select_related` (`queryset.rs:343-346`, `compiler.rs:541-570`) só segue uma
    *forward* FK por `LEFT JOIN`, nomeada por string, e não há caminho para
    relações *reversas* a um-para-muitos sem repetir uma query por pai.
  - **M2M `add` faz SELECT-depois-INSERT:** `m2m.rs:79-96` — `add` chama
    `self.exists(...).await?` (um `SELECT … LIMIT 1`) e só então o `INSERT`: dois
    *round trips* por associação, e nenhuma via de *batch* para associar muitos.
  - **M2M por `format!` fora do compilador:** `m2m.rs:83-93,101-111,131-141,…`
    constroem SQL com `format!` e `ph(n)`, e `ph` (`m2m.rs:245-252`) chama
    `db::dsn()`+`backend_of` **em cada placeholder** (uma leitura de ambiente +
    alocação de `String` por `$n`).
  - **`keys()` → `pk__in`:** o padrão documentado (`m2m.rs:164-166`) devolve as
    chaves e manda o chamador fazer `filter_str("pk__in", …)`, que cai em PERF-03.
- **Impacto:** carregar N pais e os filhos de cada um custa N+1 queries (ou 2N no
  caso M2M por causa do `exists` antes do `add`). Para uma página de 50 `Tenant`
  com `apps`, são 51 *round trips* — a latência de rede domina. É o padrão de
  desempenho que mais penaliza uma aplicação web típica.
- **Reprodução:** instrumentar (ADR 0008, `tracing` no *target* `dorm::db`) um
  carregamento de 50 pais + filhos e contar eventos `statement finished`; comparar
  com o alvo de 2 queries (`prefetch`).
- **Recomendação (mínima):** (a) `prefetch_related(rel)` que carrega os filhos num
  segundo `SELECT … WHERE fk = ANY($1)` (um só, graças a PERF-03) e os agrupa em
  memória pelos pais — o modelo do Django; (b) `m2m.add` idempotente via
  `INSERT … ON CONFLICT DO NOTHING` (um round trip em vez de dois) e um `add_many`;
  (c) cachear o `Backend` resolvido uma vez (como `SLOW_QUERY`/pool fazem) em vez
  de reler o DSN por placeholder em `ph`.
- **Esforço:** M (prefetch) / S (ON CONFLICT + cache de backend).
- **ADR/Tijolo:** novo ADR "prefetch_related + upsert M2M"; (b)/(c) são tijolos
  pequenos independentes.

---

## [PERF-05] SQL recompilado em cada terminal + alocações por chamada e por linha — Médio

- **Hipótese:** nova (H-perf), adjacente a H2.
- **Evidência:**
  - **Recompilação por chamada:** cada terminal recompila o `Plan` do zero —
    `queryset.rs:372` (`all`), `:454` (`count`), `:468` (`exists`), `:482`
    (`rows`), `:508` (`aggregate`). Reutilizar um `QuerySet` num laço reconstrói a
    `String` de SQL, o `Vec<Value>` e o `Vec<ColumnSpec>` a cada *await*. O
    compilador é CPU puro, mas aloca generosamente (`format!` por predicado, por
    join, por coluna; `Joins` é um `BTreeMap`).
  - **`Vec` de parâmetros emprestados por chamada:** `db.rs:771-772` e `:790-791`
    alocam um `Vec<&(dyn ToSql + Sync)>` novo em cada `fetch`/`run`.
  - **Chave da cache = re-hash do texto SQL inteiro** em cada `prepare_cached`
    (`db.rs:665,669,699,703`): o custo é proporcional ao comprimento do SQL, por
    chamada, mesmo em *cache hit*.
  - **Caminho dinâmico aloca por célula e por linha:** `queryset.rs:578-588`
    (`decode`) cria um `BTreeMap` (`ValueRow`) por linha e faz `spec.name.clone()`
    (uma `String` nova) por coluna e por linha; `value::read_column`
    (`value.rs:311-365`) produz um `Value` (e `String` para texto) por célula. O
    caminho tipado (`T::from_row`) é mais leve, mas o dinâmico (REPL, `rows()`,
    `values()`) paga isto a cada linha.
- **Impacto:** em cargas repetitivas (o padrão do ORM), a recompilação e as
  alocações somam-se ao *overhead* que PERF-06 vai querer provar ≤ 5 %. Não é um
  bug de correcção, mas é o que separa "mesmo desempenho que SQL cru" de "quase".
- **Reprodução:** `criterion` com contagem de alocações (feature `dhat` ou
  `alloc_counter`) sobre `all()` reutilizando o mesmo `QuerySet` 10 000 vezes vs.
  um statement `tokio-postgres` preparado uma vez.
- **Recomendação (mínima):** (a) opção de compilar o `Plan` uma vez e reexecutar
  (`QuerySet::compile()` devolvendo um `Sql` reutilizável), evitando recompilar no
  laço quente; (b) usar `SmallVec`/buffer reutilizável para os parâmetros
  emprestados; (c) no `decode`, reaproveitar os nomes de coluna por referência
  (`Arc<str>`/`&'static`) em vez de `clone()` por célula. Todas incrementais e sem
  tocar na pureza do compilador.
- **Esforço:** M
- **ADR/Tijolo:** tijolo "SQL compilado reutilizável"; afina ADR 0007.

---

## [PERF-06] A afirmação "SQL cru e abstracção têm o mesmo desempenho" (§3.3) está sub-evidenciada — Baixo

- **Hipótese:** nova (H-perf).
- **Evidência:** o único benchmark é `benches/prepared_cache.rs`. Mede dois grupos
  (`select`, `insert`) com três braços cada (`raw-adhoc`, `raw-cached`,
  `dorm-typed`) sobre **uma** SELECT trivial por PK indexada e **um** INSERT de uma
  linha (`prepared_cache.rs:61-64,117-224`). Não há: p50/p99 (o `criterion` dá
  média/mediana, mas o critério de aceitação não está escrito), contagem de
  alocações, `EXPLAIN` comparado, filtro com 2 joins, `bulk_create`, `count`
  agrupado, nem comparação com `sqlx`/`diesel-async`/`sea-orm`. Nota favorável: o
  caminho cru sancionado (`Manager::raw`→`db::sql`→`db::fetch`→`db::query`,
  `queryset.rs:125-128` e `db.rs:814-816`) partilha **exactamente** o mesmo
  executor que o tipado, por isso o *delta* esperado é só compilação (CPU, PERF-05)
  + descodificação — mas isto está por **medir**, não provado.
- **Impacto:** a reivindicação de paridade é plausível por leitura mas não está
  sustentada por medição. Sem critérios de aceitação escritos, "mesmo desempenho"
  é uma opinião.
- **Reprodução:** o protocolo abaixo.
- **Recomendação (mínima):** adoptar o conjunto mínimo de benchmarks e os
  critérios de aceitação da secção seguinte, e re-escrever §3.3 como um resultado
  medido (com números) em vez de uma afirmação.
- **Esforço:** M
- **ADR/Tijolo:** estende ADR 0007 (a "prova de desempenho").

---

## [PERF-07] Paginação só por OFFSET, sem keyset, streaming por cursor nem COPY — Baixo

- **Hipótese:** H8 (confirmada).
- **Evidência:** `compiler.rs:704-709` escreve `LIMIT n`/`OFFSET n` inline; não há
  nenhum construtor de *keyset* (`WHERE (col, pk) > ($1, $2)`) no compilador, e
  `H11` (discovery) já nota `limit.unwrap_or(50)` na API gerada e paginação por
  offset. `all()` faz `Vec::with_capacity(rows.len())` (`queryset.rs:374`) — materializa
  o resultado inteiro; não há terminal que devolva um *stream*/cursor. Sem `COPY`
  (ver PERF-02).
- **Impacto:** `OFFSET` grande é O(offset) no PostgreSQL (lê e descarta as linhas
  saltadas) — a paginação profunda degrada-se linearmente. A materialização total
  impede varrer tabelas grandes sem carregar tudo na memória.
- **Reprodução:** medir latência de `offset(1_000_000).limit(50)` vs. keyset
  equivalente no PG descartável.
- **Recomendação (mínima):** adicionar paginação *keyset* (cursor opaco sobre a
  ordenação + pk) e um terminal de *streaming* (`for_each`/`try_stream`) que use o
  cursor do `tokio-postgres`. Ambos assentam no compilador puro existente.
- **Esforço:** L
- **ADR/Tijolo:** novo ADR "paginação keyset + streaming por cursor".

---

## Protocolo de medição proposto

**Três caminhos, mesma query lógica:** (1) `tokio-postgres` cru com `Statement`
preparado uma vez (Bind/Execute) = **linha de base**; (2) `Manager::raw`/`db::sql`
(SQL à mão pelo executor do ORM); (3) abstracção tipada (`QuerySet`). As três
contra o PG 17 em `127.0.0.1:55432` (e PG 16 local para 2.ª leitura), tabelas
semeadas de forma determinística, `pg_stat_statements` e `EXPLAIN (ANALYZE,
BUFFERS)` activos. Benchmarks mínimos novos a juntar a `prepared_cache.rs`:
**(a)** SELECT por PK; **(b)** filtro com 2 joins (`Application.filter(tenant__slug, plan__code)`);
**(c)** `bulk_create` 1k e 10k (com e sem chunking); **(d)** `count` agrupado
(`annotate(Count)`); e um laço de `pk__in`/`m2m` para expor PERF-01/03.
**Critérios de aceitação:** plano `EXPLAIN` **idêntico** entre os três caminhos
(mesma árvore, mesmos índices); *overhead* do caminho (3) sobre (1) ≤ **5 % em
p50** e ≤ **10 % em p99**; e **alocações por linha** reportadas (dhat/alloc_counter)
e não-crescentes com o tamanho do resultado além do inerente. Comparação externa
informativa (não gate): `sqlx`, `diesel-async`, `sea-orm` na query (a) e (b),
para situar a linha de base. `pg_prepared_statements` tem de permanecer **limitado**
ao longo dos laços (prova de PERF-01).
</content>
</invoke>
