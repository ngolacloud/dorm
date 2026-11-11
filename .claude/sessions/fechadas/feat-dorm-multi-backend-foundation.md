---
session: dorm-multi-backend-20261101
branch: feat/dorm-multi-backend-foundation
status: closed
opened: 2026-11-01
closed: 2026-11-01T16:57:42+01:00
---

# Objetivo
Campanha DORM: fechar os bloqueadores de produção (Fase 1) e trazer multi-backend
(Fase 2, começando por CockroachDB e MySQL), com prova viva via delonix. Cresceu
organicamente do B-13 (handle Database) — sem contrato formal de `/escopo`.

# Critérios (verificados contra a realidade no fecho)
- [x] Fase 1 completa: prefetch (N+1), migrações reversíveis, resiliência de
  conexão, alargar Value (float/bytes), pool SQLite WAL — ADRs 0013-0017; make check rc 0.
- [x] CQRS nativo + trait Driver + capacidades + handle Database/routing réplica —
  ADRs 0010-0012; read_queries_route_to_replica + cqrs_routing_live verdes.
- [x] CockroachDB 15/15 (make compat-cockroach, PG-wire, zero código) — ADR 0011.
- [x] MySQL Inc. 0-2a: placeholder+positionalize, driver mysql_async (raw CRUD),
  transações+savepoints — ADR 0018; mysql_live verde contra MySQL 8 (delonix).
- [ ] MySQL suite typed_e2e verde (Inc. 2b-5): create via last_insert_id, ddl
  refinements, migrador GET_LOCK, generalizar typed_e2e. CARREGADO para a próxima.

# Handoff
Delivered: 19 commits em feat/dorm-multi-backend-foundation (base feat/dorm-correctness,
HEAD 2979186, NÃO pushada — merge é do dono). Fase 1 inteira (3 bloqueadores 🔴 +
Value + pool SQLite) + CQRS/Driver/capacidades/Database-handle + CockroachDB 15/15
+ MySQL até transações. make check rc 0; clippy --all-features limpo; provas live:
read_replica_routing_live, cqrs_routing_live, resilience_live, value_roundtrip_live,
sqlite_pool, mysql_live (todos verdes). ADRs 0010-0018 escritos. 2 Artifacts
publicados (roadmap + plano MySQL).
Not delivered: suite typed_e2e a verde em MySQL — falta Inc. 2b (ligar create via
last_insert_id: compile_insert_on sem RETURNING + model::insert_returning + db::insert
+ erro 1062), Inc. 3 (ddl VARCHAR-p/-TEXT-indexado, FK table-level, MODIFY COLUMN),
Inc. 4 (migrador GET_LOCK+ledger-por-versão), Inc. 5 (generalizar typed_e2e). O verbo
db::mysql::insert() já existe (allow(dead_code)).
Learned: (1) adicionar Backend::MySql cascateia matches exaustivos (ddl of_dsn/sql_type/
serial/guards/now, history backend_str, m2m ph) — tratar juntos. (2) Testes live com
pool global têm de partilhar UM runtime (um só #[tokio::test] com fases) senão "Tokio
context being shutdown". (3) Em SQLite/MySQL usar db::sql/fetch/run, NÃO db::query
(PG-nativo). (4) mysql: Timestamp vai como Value::Date nativo (rfc3339 recusado pelo
DATETIME); DECIMAL(38,9) senão trunca cêntimos. (5) cockroach --insecure recusa
listen-addr não-local. Tudo em memória [[dorm-multi-backend-roadmap]].
Next session: ligar o create do MySQL via last_insert_id (ADR 0018 Inc. 2b) para
passar os ~13 testes typed_e2e que hoje batem no RETURNING.

# Notas
Containers de teste a correr: dorm-pg17 :55432, dorm-pgbouncer :6432, dorm-cockroach
:26257, dorm-mysql :3306 (root/dorm). Contrato de sessão formal não existia (campanha
orgânica); este registo é o fecho.
