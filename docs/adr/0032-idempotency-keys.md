# ADR 0032 — Chaves de idempotência (L13)

Status: Accepted — implementado (Fase 7) · 2026-11-03

## Context

Um cliente que repete um `POST` (timeout, resposta perdida) com a mesma
`Idempotency-Key` não pode criar o recurso duas vezes nem receber uma resposta de
outra operação. O DORM não tinha nada (`grep idempoten` vazio no código).

## Decision

Um helper genérico no núcleo (`dorm_core::idempotency`) com uma tabela
`dorm_idempotency (key PK, request_hash, status, response, created_at,
expires_at)`.

- **`begin(key, request_hash, ttl) -> Begin`.** Reclama a chave se livre
  (`Fresh` — o chamador faz o trabalho e depois `finish`); se já concluída devolve
  `Replay(resposta)`; se ainda a correr, `InProgress`. Uma chave reusada com um
  **hash de pedido diferente** é `Error::IdempotencyMismatch` — a mesma chave tem
  de significar a mesma operação.
- **`finish(key, response)`.** Guarda a resposta e marca concluída.
- **`purge_expired()`.** Remove registos expirados (um job).

A reclamação é um `INSERT … ON CONFLICT (key) DO NOTHING RETURNING key`: uma linha
devolvida = ganhámos a chave (`Fresh`); nenhuma = inspecciona-se o estado. O TTL
liga como texto (`($3 || ' seconds')::interval`) para evitar a armadilha de
inferência de tipo (ADR 0030).

## Consequences

- **Prós:** idempotência de pedidos genérica — replay seguro, recusa de reutilização
  com corpo diferente, expiração.
- **Contras / custo:** o `request_hash` é responsabilidade do chamador (hash do
  corpo canónico); o `InProgress` deixa a política de retry ao chamador (um
  `409`/`425`).
- **Dependências:** `db` do núcleo; `Error::IdempotencyMismatch` novo.

## Proof

- **Live (`dorm-core/tests/idempotency_live.rs`):** primeira vez `Fresh` →
  `finish`; o retry devolve `Replay` da resposta guardada; a mesma chave com outro
  hash dá `IdempotencyMismatch`; um segundo `begin` antes de `finish` é
  `InProgress`; uma chave com TTL 0 é expirada e purgada, ficando livre de novo.
- **Unit:** a migração cria a tabela com `key` como PK.
