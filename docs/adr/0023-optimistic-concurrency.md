# ADR 0023 — Concorrência optimista (e `set_local` seguro)

Status: Accepted — implementado (Fase 2) · 2026-11-03

## Context

`save()` sobrescreve incondicionalmente por PK — *last-write-wins* silencioso.
`save_instance` (`crates/dorm-core/src/model.rs:60-115`) filtra só por
`pk_field = obj.pk()` (`:99-100`), monta as *assignments* de todas as colunas e
executa `compile_update` **descartando o número de linhas afectadas** (`:113`).
Não há:

- campo de versão: `FieldMeta` (`meta.rs:57-74`) tem
  `nullable/primary_key/unique/index/auto_now_add/auto_now` — **nenhuma** flag
  `version`; o parser do `dorm.schema` não reconhece `@version`;
- `update_if_version`/verificação otimista (`grep` vazio em todo o repo);
- `Error::Conflict`: o enum (`error.rs:18-90`) não o tem;
- `etag` (`grep` vazio).

Dois clientes que leiam a mesma linha, modifiquem campos diferentes e gravem —
o segundo apaga o primeiro sem aviso. Numa plataforma com reconciliadores
concorrentes e APIs com `If-Match`, isto é um bug de dados à espera de acontecer.

Além disso, vários eixos (RLS do ADR 0021, `lock_timeout` do ADR 0022) precisam de
um mecanismo seguro para `SET LOCAL <guc> = <valor>` por transacção, que hoje não
existe de forma segura (`SET LOCAL` não aparece no código — `grep` vazio).

## Decision

- **Campo de versão declarativo.** `#[field(version)]` no derive e `@version` no
  `dorm.schema` marcam uma coluna inteira (uma flag nova em `FieldMeta`, via slice
  próprio — **sem** acrescentar variante a `Value`/`FieldType`). O compilador de
  `UPDATE` de um modelo versionado acrescenta `SET version = version + 1` e
  `WHERE … AND version = $expected`.
- **`update_if_version(expected)` e `save()` verificado.** O terminal de update lê
  o número de linhas afectadas (hoje descartado em `model.rs:113`): **zero linhas**
  com a PK presente ⇒ `Error::Conflict { expected, actual }` (actual relido numa
  segunda consulta barata, ou `None` se a linha desapareceu). `save()` de um modelo
  versionado passa a usar este caminho.
- **`etag` opaco.** Um helper deriva um *etag* estável a partir de `(pk, version)`
  — para `If-Match`/`ETag` numa API — e valida um *etag* recebido de volta para a
  versão esperada. Opaco: o cliente não interpreta o conteúdo.
- **`set_local(key, value)` seguro (primitiva partilhada).** `SET LOCAL <key> = $1`
  dentro de uma transacção, com a **chave validada contra uma allow-list** de GUCs
  permitidos (`lock_timeout`, `idle_in_transaction_session_timeout`, `dorm.tenant`,
  …) — nunca um identificador livre do chamador — e o **valor ligado como `$1`**. É
  o mecanismo que o ADR 0021 (GUC de RLS) e o ADR 0022 (`lock_timeout`) consomem.

### Como preserva os invariantes do `ARCHITECTURE.md`

- **I1:** `version` é um `&'static FieldMeta` resolvido pelo registry; a chave de
  `set_local` vem de uma allow-list, não do chamador.
- **I2:** `$expected` do WHERE e o valor de `set_local` são ligados como `$n`.
- **I3:** o `SET version = version + 1 WHERE version = $n` é texto puro emitido
  pelo compilador a partir do `Plan` (entra no contrato de *snapshot*); a leitura
  do *rowcount* e o `set_local` são comportamento do `db`, fora do compilador.

## Consequences

- **Prós:** escrita concorrente segura por omissão para modelos versionados; base
  de `If-Match` para APIs; `set_local` seguro desbloqueia RLS (0021) e
  `lock_timeout` (0022) sem repetir a validação de allow-list em cada sítio.
- **Contras / custo:** a aplicação tem de tratar `Error::Conflict` (reler e
  repetir); `save()` de um modelo versionado deixa de ser *fire-and-forget*. Os
  *snapshots* de UPDATE de modelos versionados mudam.
- **Dependências:** nenhuma a montante; é dependência de 0021 e 0022.

## Implementação (Fase 2)

- **Desvio deliberado face à proposta:** a versão **não** entra em `FieldMeta`.
  `#[field(version)]` emite um `VersionMeta { model, field }` num *slice*
  `inventory` próprio (`meta.rs`), resolvido por `ModelMeta::version_field()` — a
  mesma disciplina do `dorm-rag`. Zero edições aos literais de `FieldMeta`, e o
  compilador continua ignorante de versão; o *bump*/guarda é da costura de escrita
  (`model.rs::versioned_update`), que lê o rowcount antes descartado.
- `Error::Conflict { model, expected, actual }` em `error.rs`; `etag`/
  `version_from_etag` em `model.rs` (decodável, sem assinatura — uma versão errada
  só perde a guarda, não dá acesso, por isso dispensa HMAC).
- **`set_local` seguro** em `db.rs`: `SELECT set_config($1_key_validado, $2, true)`
  (não `SET LOCAL … = $1`, que não aceita parâmetro), chave contra `SET_LOCAL_ALLOWED`.
- O derive passou a incluir o campo de versão no `Model::column_values` (não no
  `NewRow`), para a costura ler a versão corrente da instância carregada sem o
  obrigar a vir no INSERT (o default da base inicializa-o).

## Proof

- **Unit:** `model::tests` (etag ida-e-volta, etag malformado recusado),
  `db::tests` (`set_local` recusa chave fora da allow-list e fora de transacção),
  `meta::tests` (resolução de `version_field` pelo *slice*).
- **Live (provado, `dorm-models/tests/optimistic_live.rs`):** cria uma linha
  (versão 0); dois leitores na versão 0; A grava (→ 1); a gravação de B (ainda em 0)
  recebe `Error::Conflict { expected: 0, actual: Some(1) }`; B relê e repete (→ 2);
  o caminho `update_if_version` com `etag` sucede na versão esperada e é recusado
  numa versão obsoleta.
