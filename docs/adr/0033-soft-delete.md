# ADR 0033 — Soft delete e purga (L14)

Status: Accepted — implementado (Fase 7) · 2026-11-03

## Context

Muitas tabelas querem apagar "logicamente" — esconder a linha mas mantê-la para
auditoria/recuperação — e só remover fisicamente mais tarde. O DORM não tinha
nada (`grep soft.delete|deleted_at|with_deleted` vazio).

## Decision

`#[model(soft_delete)]` marca um modelo como soft-deletante (a sua coluna
`deleted_at TIMESTAMPTZ` nullable). Reutiliza a costura `scope` (ADR 0021) e um
slice `inventory` próprio (`SoftDeleteMeta`, como `version` no ADR 0023) — **sem
mexer em `FieldMeta`/`ModelMeta` nem no compilador**.

- **Filtro por omissão.** A costura acrescenta `deleted_at IS NULL` a todo o
  SELECT/count/exists/aggregate/update de um modelo soft-delete, escondendo as
  linhas apagadas.
- **`delete()` vira stamp.** Num modelo soft-delete, `compile_delete` emite um
  `UPDATE SET deleted_at = now()` (ligado como `Value::Timestamp`) em vez de um
  `DELETE`, sobre o plano já filtrado por `deleted_at IS NULL` (só apaga vivas).
- **`db::with_deleted(fut)`.** Opt-in explícito que desliga o filtro — "mostra-me
  também as apagadas".
- **`db::with_hard_delete(fut)`.** Faz o `delete()` ser mesmo um `DELETE` (o
  caminho da purga).
- **`scope::purge_soft_deleted(meta, retain_secs)`.** Remove fisicamente as linhas
  apagadas há mais de `retain_secs` — o job de retenção.

## Consequences

- **Prós:** apagar esconde e preserva; `with_deleted` para auditoria/recuperação;
  purga por retenção; zero mudança ao compilador/metadata.
- **Contras / custo:** o `deleted_at = now()` usa o relógio do cliente (ligado);
  um modelo marcado `soft_delete` tem de declarar a coluna `deleted_at` (senão o
  soft-delete não se aplica). Falta a sintaxe `@@softDelete` no `dorm.schema`
  (assenta neste mecanismo).
- **Dependências:** a costura `scope` (ADR 0021); o lookup `isnull` do compilador.

## Proof

- **Live (`dorm-models/tests/soft_delete_live.rs`):** `delete()` esconde a linha
  por omissão mas não a remove fisicamente; `with_deleted` revela-a com o
  `deleted_at` carimbado; `purge_soft_deleted(meta, 0)` remove-a de vez;
  `with_hard_delete` apaga mesmo, saltando o caminho suave.
