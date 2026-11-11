---
session: dorm-e2e-review-and-correctness
branch: feat/dorm-correctness
status: closed
opened: 2026-10-31
closed: 2026-11-01T12:47:31+01:00
---

# Scope

Revisão de ponta a ponta do DORM (prompt mestre) e arranque do roteiro de
correcção. A sessão não abriu `/escopo` formal; o contrato é retroactivo. O
registo durável vive na auto-memória `dorm-e2e-review-polyglot`.

Objectivo: rever o DORM (design, escala, desempenho, segurança, migrações,
adopção, poliglota) com evidência verificável, e implementar os defeitos de
correcção prioritários sem desestabilizar o caminho PostgreSQL provado.

Done-criteria:
- [x] Fases 0-3 (discovery → revisão por eixo → verificação → síntese) entregues
      com evidência; deliverables D1-D8 + ADRs 0010-0017 propostos.
- [x] Vaga 1 (correcção) implementada e provada ao vivo.
- [x] Os 3 Críticos de correcção/migração + o read-only por sessão fechados.

Out of scope: B-13 (handle Database explícito / routing); Vagas 4-8 (tipos,
consulta, objectos de BD, zero-downtime restante, poliglota).

# Handoff
Delivered:
- **Revisão (ramo `review/dorm-e2e`, commit db9b683, só docs):** `docs/review/`
  (00-discovery, 7× findings/, VERIFICATION, REPORT, CAPABILITY_MATRIX,
  TARGET_ARCHITECTURE, ROADMAP_BRICKS) + `docs/adr/0010-0017` propostos. H1-H14
  confirmadas, H16 refutada; 15 confirmados / 1 refutado na verificação
  independente; invariantes I1/I2/I3 verificados intactos.
- **Correcção (ramo `feat/dorm-correctness`, 9 commits, `make check` VERDE,
  typed_e2e live 15/15):**
  - B-01 7d0e80a — `get_or_create`/`update_or_create` já não envenenam a tx
    (SAVEPOINT aninhado); live `25P02`→ok.
  - B-02 e6e3902 — `bulk_create` em chunks `floor(65535/cols)`; live 65 536
    falha→70k ok.
  - B-04 524250d — `DELONIX_ORM_NO_PREPARE` (PgBouncer txn mode); live `26000`→ok
    (container com `MAX_PREPARED_STATEMENTS=0`).
  - B-05 d207094 — create da API gerada respeita `guard_write`; teste do gerador.
  - B-06 54a5b24 + 470147f — binário do REPL `dorm`→`dorm-shell`; sem colisão.
  - B-22 b7479f2 — `sync` constrói índices `CONCURRENTLY` fora da tx; live índice
    válido + `CONCURRENTLY`-em-tx=`25001`.
  - B-24 993f131 — detecção de renomeação (`ColumnRename`); SQLite e2e prova que
    os dados sobrevivem; hint inválido=`Error::InvalidRename`.
  - B-12 p1 0c0773c — read-only por sessão (`db::with_read_only`, task-local);
    live 2 sessões concorrentes independentes.

Not delivered:
- **B-13 (resto do DESIGN-01):** handle `Database` explícito substituindo o
  `static POOL` + routing réplica/tenant + conexão SQLite única. Bloqueio: enfia
  um handle por TODA a API pública (`QuerySet`, migrações) — refactor L de alto
  risco, a desenhar com o ADR 0014 (multi-tenancy). Não arrancado por decisão de
  âmbito (fim de sessão longa).
- **B-03 (`= ANY($1)`):** reclassificado para a Vaga 5 — depende de um tipo array
  em `Value` (H5/B-14); o `binder` só liga escalares.
- Vagas 4-8 inteiras (tipos, consulta, objectos de BD, zero-downtime restante,
  Query IR / Data Plane poliglota).

Learned:
- PgBouncer ≥ 1.23 tem `max_prepared_statements` default ≠ 0 e re-prepara entre
  backends de forma transparente — o `26000` do H2 só reproduz com
  `MAX_PREPARED_STATEMENTS=0` + `DEFAULT_POOL_SIZE` baixo + concorrência.
- `migrate_live`/`sync_live` exigem BD **fresca** (ledger vazio) — criar um
  `dorm_<x>` descartável por corrida, nunca reutilizar `dorm_dev`.
- O ramo do dono `fix/typed-e2e-readonly-flag-race` é só workaround DE TESTE
  (`RwLock`); o `with_read_only` do B-12 torna-o desnecessário ao reconciliar.
- Nenhum dos dois ramos foi pushado — o merge/PR é do dono (guardrail).

Next session: Implementar B-13 — introduzir um handle `Database`/`Session`
explícito que substitua o `static POOL` e habilite routing primário/réplica e
multi-tenant, removendo o último estado global.
