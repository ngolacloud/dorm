---
session: cffbfb22-49a0-4f66-8f52-47aac91dfac3
opened: 2026-10-31
branch: fix/typed-e2e-readonly-flag-race
status: closed
closed: 2026-10-31T22:12:40+01:00
---

# Objective
Estabilizar o teste flaky `an_f_expression_increments_atomically` (dorm-models),
que falhava de forma intermitente no job `integration` do CI, de modo a que a
suite live passe de forma determinística e o CI deixe de precisar de rerun.

# Done when
- [x] A suite completa passa de forma determinística sob execução repetida contra
  PostgreSQL vivo. (Provado: suite `typed_e2e` 40/40 verde; antes falhava ~1 em 10.
  Comando: loop 40× de `DELONIX_ORM_TEST_DSN="host=/var/run/postgresql user=walter
  dbname=dorm_flaky" cargo test -p dorm-models --test typed_e2e -- --ignored`.)
- [x] A sequência exacta do CI (`migrate_live` → `sync_live` → `typed_e2e`) contra
  uma única base passa repetidamente. (Provado: 20/20 verde.)
- [x] Critério literal do pedido: F-expr isolado 20×. (Provado: 20/20.)
- [x] Gates do fast-lane (`check`) limpos: `cargo fmt --all --check` rc=0;
  `cargo clippy -p dorm-models --tests --all-features -- -D warnings` sem avisos;
  sem DSN os 12 testes continuam a `ignore` corretamente.
- [ ] "3 PRs seguidos verdes à primeira" — não observável nesta sessão (depende de
  push/merge do dono e de corridas reais de CI). Carregado para o dono.

# Out of scope
- Push e abertura/merge de PR (é do dono).
- Alterar a semântica process-wide de `db::set_read_only` (o contrato está correto;
  o cliente real é a sessão `--read-only` do shell). A correção ficou toda no arnês
  de teste.

# Handoff
Delivered: Corrida de estado global partilhado corrigida em
[typed_e2e.rs](crates/dorm-models/tests/typed_e2e.rs) (commit aa175af, branch
`fix/typed-e2e-readonly-flag-race`). Causa-raiz: o teste
`a_read_only_session_refuses_a_write_before_it_reaches_sql` chama
`db::set_read_only(true)`, um `AtomicBool` global do processo; enquanto a flag
está ligada, qualquer `create`/`update`/`delete` de um teste paralelo (o `update`
do F-expr, o `delete` do `seed()`, o `create` do `bulk_create`) é recusado com
`Error::ReadOnly(...)`. Fix: um `write_gate()` `RwLock` global — testes `e2e!`
tomam guarda partilhada (leitura, continuam em paralelo), o teste read-only foi
tirado do macro e toma guarda exclusiva (escrita, corre sozinho) + repõe a flag
via drop guard. Provado vivo: suite 40/40, sequência CI 20/20, F-expr 20/20; fmt
e clippy limpos.
Not delivered: A observação "3 PRs verdes à primeira" não é verificável localmente
— exige push e corridas de CI reais, que são do dono.
Learned: A flakiness NÃO estava no teste F-expr; afetava toda a suite. O `setup()`
usa `db::execute` (caminho não-guardado), logo o DDL não é afetado pela flag — só
operações QuerySet/model (`guard_write`) o são, e todas vivem dentro de corpos de
teste cobertos pelo gate. Reprodução fiável: correr a suite COMPLETA em paralelo
(não o teste isolado, que passa sempre) contra base descartável, ~1 falha em 10.
Next session: none.
