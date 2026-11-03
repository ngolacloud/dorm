# Medição de lacunas — DORM como camada de dados da plataforma

Medido contra `main` em `fc080f1`..`3cf7cef` (2026-11-03), por leitura directa do
código. Cada linha traz evidência (`ficheiro:linha` ou nome de teste). Estados:
**fechado** (salta-se), **parcial** (falta o indicado), **aberto**.

> **Aviso de numeração de ADRs.** O ramo `review/dorm-e2e` (PR #23, já no `main`)
> introduziu ADRs *Proposed* **0010-0017 com numeração colidente** com os ADRs
> implementados de igual número (ex.: `0015-concurrency-control.md` vs
> `0015-connection-resilience.md`; `0016-zero-downtime-migrations.md` vs
> `0016-wider-value-float-and-bytes.md`). Essas propostas estão ainda ancoradas na
> visão "Data Plane gRPC" (ADR 0012/0011 do mesmo ramo), diferente das capacidades
> genéricas de ORM pedidas aqui. As ADRs novas desta campanha **continuam em 0021+**
> para não agravar a colisão, e reorientam a decisão para o núcleo do ORM. A
> renumeração do conjunto colidente fica como dívida documental a resolver à parte.

## Tabela de estado

| # | Lacuna | Estado | Evidência-chave |
|---|---|---|---|
| **L1** | Âmbito de inquilino imposto no QuerySet | **aberto** | `tenant_scoped` só é **declarado** (`meta.rs:108,134`); nunca lido pelo compilador/queryset (só comentário `compiler.rs:995`). Filtro aplicado à mão nas bordas (`dorm-mcp/tools.rs:412`, `dorm-shell/main.rs:336`). Sem `with_tenant`, sem `TenantScopeMissing` (`error.rs:18-90`), sem RLS (`grep` vazio). `:raw`/`db execute` cruzam inquilinos. → **ADR 0021** |
| **L2** | Migrações online (rolling) | **parcial→aberto** | Cadeia inteira num único `db::atomic` (`migrator.rs:96,129-146`). `CONCURRENTLY` existe mas **só no `sync`** (`ddl.rs:237`, `migrator.rs:253-277`), não no `migrate`. Sem `lock_timeout`/backoff, sem limpeza de INVALID, sem backfill, sem `migrate lint`, sem `check-compat`. Rename automático = drop+add destrutivo (`autodetect.rs:184`); data-preserving só se declarado. `deploy` aplica a cadeia completa (fechado). → **ADR 0022** |
| **L3** | Concorrência optimista | **aberto** | `save_instance` sobrescreve por PK e descarta o rowcount (`model.rs:99-113`). Sem flag `version` (`meta.rs:57-74`), sem `update_if_version`, sem `Error::Conflict`, sem `etag` (greps vazios). → **ADR 0023** |
| **L4** | Paginação por keyset | **aberto** | Só `OFFSET`/`LIMIT` (`compiler.rs:824`, `codegen_api.rs:153`). `compile_order` sem desempate por PK (`compiler.rs:736`) → ordem não-estável. Sem cursor/keyset/HMAC (greps vazios). → **ADR 0024** |
| **L5** | Tipos de control plane | **aberto** (parcial em CHECK/UNIQUE Rust) | `FieldType` 10 variantes (`value.rs:27-41`); sem `inet/cidr/macaddr`, sem array (recusa explícita `value.rs:88`), sem lookups de rede (`lookup.rs:14`). JSONB existe mas `__` = relações, não caminho JSON (`registry.rs:211`). `@@unique/@@index` parseados-e-ignorados (`parser.rs:496`); PK composta e enums em "Not yet" (`schema.md:76-79`). → **ADR 0025** |
| **L6** | Outbox/inbox genéricos | **parcial** | Existe outbox **específico do RAG**: tabela `dorm_rag_outbox` (`dorm-rag/migrations.rs:29,226`), captura por trigger (`:135-183`), worker com `FOR UPDATE SKIP LOCKED` + retries (`worker.rs:138,219`). **Falta**: genérico/multi-consumidor com cursor, `outbox::publish` em `db::atomic`, inbox `UNIQUE(consumer,message_id)`, backoff real (hoje re-claim imediato `worker.rs:219-220`), DLQ separada. |
| **L7** | Mensageria (nats/kafka/amqp) | **aberto** | Ausência total (greps vazios): sem `Publisher/Subscriber`, CloudEvents, `traceparent`, nats/kafka/amqp. |
| **L8** | Réplicas com lag/read-your-writes | **parcial→aberto** | Routing binário primário/réplica **única** (`db.rs:130-138,404-459`); RYW só por fixação à transação (`db.rs:448`, ADR 0010). **Falta**: várias réplicas, medição de lag (LSN/`pg_last_xact_replay_timestamp`), ejeção por `max_lag`, balanceamento (`round_robin`/`least_outstanding`/`p2c_ewma`), token LSN cross-request. |
| **L9** | Fila de trabalho + leases | **aberto** | Sem `dorm-jobs`. Tijolos adjacentes: advisory locks só p/ migrações (`migrator.rs:99`), lease in-row só do RAG (`worker.rs:40`). Sem agendamento futuro, prioridades, chave de unicidade, eleição de líder. |
| **L10** | Partições e retenção | **aberto** | Ausência total: sem `@@partition`/`retain`/`DETACH` (`findings/migrations.md:24` "ausente"). |
| **L11** | Vários schemas / várias bases | **parcial→aberto** | Multi-**base** por processo existe (`Database::connect` por DSN, `db.rs:419`). Multi-**schema** inexistente: `Database` sem `search_path` (`db.rs:404-413`), `ModelMeta` sem campo `schema` (`meta.rs:100`), derive não aceita `schema=`, `migrate deploy` sem `--database/--schema` (`main.rs:320`). |
| **L12** | Read models / Event Sourcing | **parcial→aberto** | CQRS base fechado (`cqrs.rs:52-110`, ADR 0012). Sem projeções materializadas, event store, `xid8`, snapshots, upcasting (greps vazios). |
| **L13** | Idempotency keys | **aberto** | Sem tabela/helper; sem `Error::IdempotencyMismatch` (`error.rs:18-90`). Só mencionado como futuro em ADR 0012/roadmap. |
| **L14** | Soft delete e purga | **aberto** | Ausência total: sem `@@softDelete`/`deleted_at`/`with_deleted`/`purge` (greps vazios). |
| **L15** | Compatibilidade PgBouncer | **parcial** | Cache prepared fechado (ADR 0007, `db.rs:979-1027`). Mitigação por flag **manual** `DELONIX_ORM_NO_PREPARE` (`db.rs:69-78`), provada live (`no_prepare_pgbouncer_live.rs`). **Falta**: auto-deteção do modo transação e erro próprio no modo incompatível (hoje vaza `26000` cru). |
| **L16** | Desempenho a escala | **parcial** | Um bench criterion (prepared vs ad-hoc, 100 linhas, `benches/prepared_cache.rs`, ADR 0007). **Falta**: harness a 10M linhas / 100 inquilinos p/ keyset, update-com-versão, tenant+RLS, consumo de outbox; sem portão de regressão em CI. |

## Mapa lacuna → ADR (campanha)

| Fase | Lacunas | ADRs |
|---|---|---|
| 1 (esta) | medição + L1-L5 | **0021, 0022, 0023, 0024, 0025** |
| 2 | L1, L3 | 0021, 0023 |
| 3 | L4, L5 | 0024, 0025 |
| 4 | L2 | 0022 |
| 5 | L6, L7, L9 | (a propor) |
| 6 | L8, L10, L11 | (a propor) |
| 7 | L12-L16 | (a propor) |

Nada aqui nomeia a NgolaCloud (ADR 0073): cada lacuna é uma capacidade genérica.
