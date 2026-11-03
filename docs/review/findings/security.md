# Revisão — eixo SEGURANÇA

> Fase 1. Apenas leitura; nenhuma alteração de código. Hipóteses do eixo:
> H10 (descodificação posicional), H11 (API gerada não-produção), H15
> (`READ_ONLY` global por processo). Evidência por `ficheiro:linha`.
> Português europeu pré-1990; identificadores em inglês.

## Veredicto do alicerce — o invariante "nenhum identificador vem do chamador"

**Mantém-se, com uma nuance documentada.** Todo o identificador (tabela, coluna,
join) que chega ao SQL vem de `ModelMeta`/`FieldMeta` `&'static`, nunca de input
externo. Os dois pontos onde uma *string* externa toca o caminho do identificador
são ambos defendidos:

1. **Chaves de corpo JSON na API gerada** (`codegen_api.rs:169`, `:210`) e chaves
   do `filter_str` dinâmico são **validadas contra a metadata** antes de qualquer
   SQL: `compiler::compile_insert_on` faz `meta.field(name)` e devolve
   `Error::UnknownField` para um nome que o modelo não declara
   (`compiler.rs:1030-1034`); o identificador emitido é sempre `quote(f.column)`
   (`compiler.rs:1060`), a *string* estática, **nunca** a chave do chamador. O
   mesmo em `compile_update_on` (`compiler.rs:890-899`). A chave externa é uma
   *chave de pesquisa*, nunca texto SQL.
2. **Aliases de anotação** (`annotate(#{ n: Count(...) })`) são escolhidos pelo
   chamador e passam por `quote(&agg.alias)` (`compiler.rs:574,604,639,803`).
   Aqui a defesa **não** é o registry mas sim `quote()` (`compiler.rs:105-112`),
   que rejeita `"`, `\0` e vazio e envolve em aspas duplas. Como `"` é o único
   escape de um identificador citado em SQL e é rejeitado, não há *breakout*.

Ou seja: o enunciado estrito "nenhum identificador vem do chamador" é **verdadeiro
para tabelas/colunas/joins** (sempre `ModelMeta`); para **aliases de anotação** a
*string* é do chamador mas neutralizada por `quote()`. Nenhum caminho leva um
identificador externo não-validado ao SQL. **Não há furo crítico.**

`limit`/`offset` são escritos em linha (`compiler.rs:704-708`) — mas são `i64`
tipados, não texto do chamador, logo não são vector de injecção (ver [SEC-05]).
Nomes de savepoint são gerados (`db.rs:1368`, teste `db.rs:1556`).

---

## [SEC-01] A API gerada contorna o `guard_write` no POST (create) — Alto
- **Hipótese:** H11 + H15 (nova, confirmada)
- **Evidência:** `codegen_api.rs:173-176` — o handler `create_*` compila e corre
  o INSERT por um caminho directo que **não** passa pelo guarda de escrita:
  ```
  let sql = compiler::compile_insert(... ).map_err(err)?;
  let rows = dorm_core::model::run_query(&sql).await.map_err(err)?;
  ```
  `run_query` é `db::fetch` cru (`model.rs:158-160`) e **nem `fetch`/`query` nem
  `execute` chamam `guard_write`** (`db.rs:642-705`, `:768-799`). O `guard_write`
  só é invocado pelos terminais tipados do `QuerySet` (`queryset.rs:133,150,489,
  496`) e pelo `sql_execute` (`db.rs:824`). O INSERT da API usa `RETURNING`, logo
  viaja por `fetch` (caminho de leitura) e escapa ao guarda por completo.
- **Impacto:** Uma implantação em modo só-leitura (`set_read_only(true)`) continua
  a **aceitar escritas via `POST /<table>`**. O `update`/`delete` gerados vão pelo
  `QuerySet` e são barrados; o `create` não. "Só-leitura" deixa de ser uma
  propriedade — é "só-leitura excepto POST".
- **Reprodução:** gerar a API, `db::set_read_only(true)`, `POST /tenants {...}` →
  linha inserida (deveria dar 403/`Error::ReadOnly`). Teste: comparar com
  `db.rs:1609` (`raw_execute_is_refused_in_a_read_only_session`), que prova que o
  caminho tipado barra e este não.
- **Recomendação:** o `create_*` gerado deve usar o terminal guardado
  (`Model::objects().create(...)`/`queryset::insert`) em vez de
  `compile_insert`+`run_query`; ou acrescentar `db::guard_write("insert")?` no
  topo do handler.
- **Esforço:** S
- **ADR/Tijolo:** ADR 0005 (o guarda tem de cobrir todas as escritas) + o futuro
  tijolo de API de produção.

## [SEC-02] API gerada: sem autenticação/autorização, mass-assignment e sem âmbito de tenant — Alto
- **Hipótese:** H11 (confirmada)
- **Evidência:**
  - **Sem auth.** O router (`codegen_api.rs:259-274`) regista `/health`,
    `/openapi.json` e o CRUD por modelo — **nenhuma** camada `tower`/middleware de
    autenticação ou autorização. Qualquer cliente que alcance o socket tem CRUD
    total. Por omissão escuta `127.0.0.1:8080`, mas `DORM_API_ADDR` permite
    `0.0.0.0` (`codegen_api.rs:267`).
  - **Mass-assignment.** O corpo de `create`/`update` é um `serde_json::Map`
    dinâmico cujas chaves são aceites para **qualquer** coluna que o modelo
    declare (`codegen_api.rs:169`, `:210`). Não há *allow-list*: o cliente pode
    escrever a PK, a FK (`tenant_id`), colunas de auditoria ou de privilégio
    (p.ex. `is_superuser` num modelo de contas) desde que existam no modelo.
  - **Sem âmbito de tenant.** `list_*` faz `objects().limit(...).offset(...)`
    sem qualquer filtro (`codegen_api.rs:151-154`); `get/update/delete` filtram
    **só** pela PK (`codegen_api.rs:195,214,233`). Não existe predicado de tenant
    em lado nenhum.
- **Impacto:** (a) exposição de dados entre tenants — `GET /<table>` devolve as
  linhas de todos os tenants; (b) escalada de privilégio / sequestro de tenant por
  mass-assignment — `POST`/`PATCH` definem `tenant_id` ou flags de privilégio
  arbitrárias; (c) sem auth, tudo isto é não-autenticado.
- **Reprodução:** gerar a API para um esquema com `tenant_id`; `POST /things
  {"tenant_id": <outro>}`; `GET /things` lista linhas de todos os tenants.
- **Recomendação:** marcar a API gerada como **não-produção** na documentação
  (já o diz o cabeçalho `codegen_api.rs:1-9`, reforçar); no tijolo de produção:
  middleware de auth obrigatório, *allow-list* de colonas graváveis por modelo
  (projecção de escrita), e um gancho de âmbito (tenant) aplicado a todo o
  `QuerySet` antes do terminal.
- **Esforço:** L
- **ADR/Tijolo:** novo ADR "API de produção" (auth/authz + projecção de escrita +
  tenant scope); liga-se a H1/H15.

## [SEC-03] `READ_ONLY` é estado global do processo, não por sessão/pedido — Alto
- **Hipótese:** H15 (confirmada)
- **Evidência:** `db.rs:75` `static READ_ONLY: AtomicBool`; `set_read_only`
  (`db.rs:238-240`) e `is_read_only`/`guard_write` (`db.rs:243-253`) operam sobre
  esse único átomo de processo. `BEGIN READ ONLY` ao servidor só acontece **dentro
  de** `atomic`/transacção interactiva (`db.rs:1326-1331`, `:1421-1425`) — uma
  instrução avulsa (o caminho da API e a maioria dos pedidos) não abre transacção,
  pelo que a única barreira para ela é o `AtomicBool`.
- **Impacto:** num processo concorrente (um servidor, a gateway poliglota futura),
  um pedido que faça `set_read_only(true)` **desliga escritas em todas as tarefas**
  e um que faça `set_read_only(false)` reabre-as para todas — fuga de estado entre
  pedidos/tenants e corrida de correcção. O próprio ramo do dono
  `fix/typed-e2e-readonly-flag-race` corrige um sintoma real desta flag a vazar
  entre testes e2e paralelos (discovery §4.1).
- **Reprodução:** duas tarefas tokio; A faz `set_read_only(true)` e aguarda; B
  tenta `create` → é recusado apesar de B nunca ter pedido só-leitura.
- **Recomendação:** o modo só-leitura tem de ser por sessão/pedido — um valor
  transportado no `CURRENT_TX`/task-local (como a transacção já é), não um
  `static`. Enquanto global, documentar que é configuração de processo fixada no
  arranque, nunca um botão por pedido.
- **Esforço:** M
- **ADR/Tijolo:** ADR 0004/0005 (semântica de só-leitura); pré-requisito da
  gateway poliglota.

## [SEC-04] Sem isolamento multi-tenant: nem RLS, nem GUC por tenant, pool global único — Médio
- **Hipótese:** H1/H15 (confirmada)
- **Evidência:** `db.rs:74` `static POOL: OnceLock<Pool>`, DSN só do ambiente
  (`db.rs:86-97`) — um único pool, um único papel de BD para todo o processo. Não
  há emissão de `SET LOCAL app.tenant_id = …` nem política RLS em lado nenhum
  (procura: nenhum `SET LOCAL`/`set_config`/`current_setting`/`ROW LEVEL` no
  núcleo). O migrador também não gera RLS (discovery H6).
- **Impacto:** qualquer consumidor do pool global vê todos os tenants; o
  isolamento é inteiramente a nível de aplicação (o chamador *tem* de escrever
  `.filter(tenant=...)`). A API gerada não aplica nenhum (ver [SEC-02]). Sem RLS,
  um `raw()` ou um filtro em falta expõe tudo; não há rede de segurança na BD.
- **Reprodução:** conceptual — não existe caminho que ancore o tenant na sessão
  nem na política.
- **Recomendação:** no eixo gateway/produção, oferecer (a) um `SET LOCAL` de
  tenant por transacção ligado ao âmbito de pedido, e/ou (b) suporte a RLS no
  migrador, de modo que a BD recuse o que a aplicação esquecer.
- **Esforço:** L
- **ADR/Tijolo:** novo ADR "multi-tenancy" (RLS + GUC por tenant); liga H1, H15,
  SEC-02, SEC-03.

## [SEC-05] `limit` sem tecto e paginação por `offset` — Médio
- **Hipótese:** H11 (confirmada)
- **Evidência:** `codegen_api.rs:153` `.limit(p.limit.unwrap_or(50))` — sem
  máximo; `.offset(p.offset.unwrap_or(0))`. O `limit`/`offset` são escritos em
  linha no SQL (`compiler.rs:704-708`) como `i64`.
- **Impacto:** DoS por página gigante (`?limit=100000000` materializa tudo em
  memória e serializa para JSON); `offset` grande tem custo O(n) no servidor
  (varre e descarta). `?limit=-1`/`offset=-1` produzem SQL que o PostgreSQL
  rejeita (erro, não injecção).
- **Reprodução:** `GET /<table>?limit=100000000` contra uma tabela grande.
- **Recomendação:** tecto no `limit` (p.ex. `min(limit, 1000)`), rejeitar
  negativos, e preferir paginação por *keyset* no tijolo de produção.
- **Esforço:** S
- **ADR/Tijolo:** tijolo de API de produção (ver SEC-02).

## [SEC-06] Descodificação posicional só é um risco no escape hatch `raw()` — Baixo
- **Hipótese:** H10 (confirmada, limitada) / H16 (refutada para o compilador)
- **Evidência:** o compilador **nunca** emite `SELECT *`/`RETURNING *`: o
  `SELECT` nomeia sempre as colunas (`compiler.rs:530-540`, comentário
  `compiler.rs:531`) e o `INSERT … RETURNING` lista colunas explícitas por
  `meta.columns()` (`compiler.rs:1065-1069`), que itera `self.fields` **na mesma
  ordem** dos `ColumnSpec` (`meta.rs:153-155`). Logo o par
  "lista RETURNING ↔ descodificação posicional" é auto-consistente por construção,
  mesmo após uma migração — a premissa de H16 (`RETURNING *`) **não corresponde ao
  código**. O risco posicional subsiste apenas em `Manager::raw`/`db::sql`
  (`queryset.rs:125-127`, `row.rs:46` `try_get::<usize>(idx)`), onde o `SELECT` é
  escrito à mão pelo chamador: se a ordem/colunas divergirem da ordem dos campos
  do modelo, os valores mapeiam para o campo errado sem erro de tipo quando os
  tipos coincidem.
- **Impacto:** num `raw()` mal escrito, um campo pode ser lido de outra coluna do
  mesmo tipo (p.ex. devolver `password_hash` onde se esperava `email`), silencioso
  enquanto os tipos baterem certo. É um defeito de correcção que *pode* virar
  exposição de dados.
- **Reprodução:** `Manager::<T>::raw("SELECT col_b, col_a FROM t", &[])` com `T`
  cujos campos são `(a, b)` → troca silenciosa se ambos forem do mesmo tipo.
- **Recomendação:** é por contrato (ADR 0005, "o chamador é dono do SELECT");
  manter documentado e, como melhoria, oferecer um `raw` que descodifique **por
  nome de coluna** em vez de por posição.
- **Esforço:** M
- **ADR/Tijolo:** ADR 0005 (contrato do escape hatch).

## [SEC-07] TLS por omissão em claro; `sslmode=require` não verifica o certificado — Baixo
- **Hipótese:** nova (confirmada, documentada)
- **Evidência:** um DSN sem `sslmode` resolve para `Disable` → `NoTls`
  (`db.rs:336-338`, `:426-428`); `require` instala um verificador que aceita
  qualquer certificado (`db.rs:486-489`, `NoVerify` `:526-536`; OpenSSL
  `:589,:597`). Só `verify-full` valida cadeia e hostname (`db.rs:483-485`).
- **Impacto:** por omissão a ligação é em claro; sob `require` há cifra mas MITM é
  possível (sem verificação de certificado). É o comportamento documentado de
  libpq (ADR 0004) e opt-in, não um downgrade silencioso — mas num ambiente de
  alta criticidade o default-plaintext merece aviso.
- **Reprodução:** ligar sem `sslmode` → tráfego em claro; com `require` a um MITM
  com certificado próprio → aceite.
- **Recomendação:** documentar `verify-full` como o modo de produção; considerar
  avisar (um `warn`) quando um DSN remoto corre em claro. Segredos: o DSN **nunca**
  é registado — `observe` recebe só o texto SQL e nunca `params` (`db.rs:116-121,
  :156`), e `summarize_sql` só encurta SQL (`db.rs:137-147`). Verificado: nenhum
  `Display`/log do DSN. (Nota menor: `db.rs:280` devolve o erro de *parse* do DSN
  no texto de `Error::Config`; o erro do `tokio_postgres` não ecoa a password, mas
  convém não propagar o DSN cru em mensagens futuras.)
- **Esforço:** S
- **ADR/Tijolo:** ADR 0004 (TLS).

---

## Contagem por severidade

| Severidade | Nº | IDs |
|---|---:|---|
| Crítico | 0 | — |
| Alto | 3 | SEC-01, SEC-02, SEC-03 |
| Médio | 2 | SEC-04, SEC-05 |
| Baixo | 2 | SEC-06, SEC-07 |

## Mapa das hipóteses

- **H10** — confirmada, mas o risco posicional vive **só** no escape hatch `raw()`
  (SEC-06); o caminho do compilador nomeia sempre colunas.
- **H11** — confirmada e desdobrada: sem auth/authz + mass-assignment + sem tenant
  scope (SEC-02), `limit` sem tecto + offset (SEC-05), e — achado novo — o POST
  gerado contorna o `guard_write` (SEC-01).
- **H15** — confirmada: `READ_ONLY` é `static AtomicBool` de processo (SEC-03),
  agravada por SEC-01 (o POST nem sequer consulta a flag) e por SEC-04 (pool
  global, sem isolamento de tenant).
- **H16** — **refutada** no caminho do compilador (não há `RETURNING *`; é lista
  explícita por ordem de campos, auto-consistente).
</content>
</invoke>
