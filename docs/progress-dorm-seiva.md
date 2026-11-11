# Progress — DORM ↔ Delonix Seiva

> Tracker da campanha de dois produtos (DORM = ORM; Delonix Seiva = framework).
> Uma sessão nova começa por ler o prompt único e **este ficheiro**.
> A partir da Fase 1 do Seiva, o tracker canónico passa a `seiva/docs/progress.md`.

## Baseline medida (2026-11-03)

- **DORM:** `main` em `ad5073d` (Merge PR #24, campanha L1–L16). Árvore limpa (só `.claude/` por versionar).
- **Workspace:** 16 crates — `dorm-core`, `dorm-derive`, `dorm-migrations`, `dorm-schema`,
  `dorm-sqlite`, `dorm-shell`, `dorm-studio`, `dorm-cli`, `dorm-rag`, `dorm-mcp`,
  `dorm-outbox`, `dorm-jobs`, `dorm-eventstore`, `dorm-messaging`, `dorm-models`, `dorm-example`.
- **ADRs:** 0001–0053 presentes. **8 números duplicados** confirmados: 0010, 0011, 0012,
  0013, 0014, 0015, 0016, 0017 (aceites vs. propostas do ramo de revisão). → Parte DORM-C.
- **Docs de revisão (31 Out):** `docs/review/{REPORT,ROADMAP_BRICKS,TARGET_ARCHITECTURE,VERIFICATION}.md`,
  `gap-measurement-ngolacloud.md`, `pr-ngolacloud-gaps.md`, `CAPABILITY_MATRIX.md` — todos presentes.
- **`seiva/`:** ainda não existe. **`seiva.toml` / crate `delonix-seiva` / CLI `seiva`:** por criar.

## Nota de contexto

O prompt único recebido nesta sessão foi **truncado em «Precedência (12…»** (fim da Parte C,
afinação da base). Falta-me o que vinha depois — presumivelmente a lista numerada de fases,
o resto da Parte C e as Partes D+. **Confirmar o plano de fases antes de executar para lá de
DORM-C / DORM-B1.**

## Mapa de trabalho (derivado das Partes do brief)

### Parte I — DORM (precede o Seiva: o Seiva depende da API de biblioteca publicada)
- [ ] **DORM-C** Higiene: renumerar ADRs duplicadas (aceites mantêm nº; propostas recebem nº
      novo + nota de redirecção); ADR nova «Fronteira do ORM» (fixa a tabela da Parte DORM-A);
      alinhar `roadmap.md` + `ROADMAP_BRICKS.md`; secção «DORM com frameworks» no README.
- [ ] **DORM-B1** Pré-condições da revisão: (1) prova de `get_or_create` savepointed sob
      concorrência atrás de PgBouncer; (2) expand/contract (→ B2b); (3) autodetect recusa
      DROP+ADD de colunas compatíveis sem `@map`; (4) modo estrito sem `DEFAULT_DB` +
      config programática + chave de cursor sem default fora de dev.
- [ ] **DORM-B2** Namespaces no schema + camada de dados gerada (Generation Gap; metadados por API).
- [ ] **DORM-B2b** Evolutionary DB Design: classificação expand/contract + `apply --phase`.
- [ ] **DORM-B3** Primitivas transaccionais: `on_commit`/`on_rollback`, `select_for_update`
      (skip_locked/nowait) tipado, advisory locks tipados, `LISTEN`/`NOTIFY` dedicado.
- [ ] **DORM-B4** API de biblioteca p/ todos os comandos da CLI (resultados tipados; config programática).
- [ ] **DORM-B5** Observabilidade sem exportadores (spans OTel semconv DB; métricas).
- [ ] **DORM-B6** Mecanismo de afinação de parâmetros da base (`db settings show|diff|apply`).
- [ ] **DORM-A** Mover o que é de framework p/ o Seiva (com histórico `git filter-repo`):
      `dorm-messaging`→`seiva-messaging`; `dorm api`; contas/`orm_accounts`; `rag worker`;
      MCP HTTP; despacho CQRS (ADR 0012). Depreciar na 0.3, remover na 0.4.

### Parte II — Delonix Seiva (repo novo `ngolacloud/seiva`)
- [ ] **ADR 0001 do Seiva**: repositório, origem do contrato (Rust-first), barramento (NATS JetStream).
- [ ] Núcleo: portas (`seiva-ports`), 5 camadas por módulo, Gateway `seiva-data-dorm`.
- [ ] CLI `seiva`: `project new`, `module new`, `db dev`, `lint`, `expand`, ...
- [ ] (restantes fases dependem da parte truncada do prompt)

## Estado por fase

- **DORM-C FEITO (2026-11-03)** — ramo `docs/orm-boundary-and-adr-hygiene`, commit
  `a3fd6c5` (NÃO pushado; por abrir PR). Higiene + peça-chave:
  - ADR 0054 «The ORM boundary» (regra dos 2 testes + tabela fica/sai + depreciação
    0.3→0.4). **Genérica: NÃO nomeia o Seiva** (fala de «a framework»).
  - 8 ADRs propostas (ramo revisão, colidiam com 0010–0017 aceites) renumeradas
    0055–0062 com nota de redirecção + status real: 0055 query-ir/0057 data-plane/
    0058 named-SQL/0059 security = Rejeitadas; 0056 catalog = pendente (API metadados);
    0060 concurrency = parcial(0023); 0061 zero-downtime = superseded(0022/0038);
    0062 types = superseded(0025). Refs cruzadas remapeadas.
  - roadmap.md (secção fronteira + nota no `dorm api`), ROADMAP_BRICKS.md (banner +
    citações remapeadas, Vaga 8 rejeitada), README (secção «DORM with frameworks»).
  - Só docs; nenhum código → `make check` inalterado. SUMMARY intocado (0026–0053 já
    órfãs; 0054 fica órfã como as vizinhas). `progress-dorm-seiva.md` NÃO commitado
    (nomeia o Seiva).
  - **Falta DORM-C:** abrir o PR.
- **DORM-B1 (parcial) — chave de cursor + modo estrito (2026-11-03)** — ramo
  `feat/dorm-b1-strict-sessions-and-cursor-key` (NÃO pushado; off main), 2 commits:
  - `0fcdba8` **chave de cursor** (`cursor.rs`): a chave de assinatura (ADR 0024)
    deixa de ler o ambiente e de ter default; configurada por API (`set_cursor_key`),
    erro tipado se não configurada (fim dos cursores forjáveis); `set_dev_cursor_key`
    p/ dev (CLI). Regra pura `key_or_error(Option)`. `pagination_live` configura no setup.
  - `1c8e7bf` **modo estrito** (`db.rs`+`error.rs`): `set_strict_sessions(bool)` (off
    por omissão); com estrito ON e sem sessão, uma statement dá `Error::SessionMissing`
    tipado em vez de cair no `DEFAULT_DB`. Imposto em `acquire_once` (conexões) **e**
    `backend()` (dispatch raw-SQL), antes de ligar/ler DSN; `dialect()` fica leniente
    (compile offline). Decisão pura `no_session_decision` + teste que prova recusa
    antes de ligar.
  - `658b151` **config por API** (`db.rs`): `ConnectOptions{pool_max,acquire_timeout_ms,
    statement_timeout_ms,ssl_mode,ssl_root_cert}`; `build_pool` recebe-a em vez de ler o
    ambiente. `ConnectOptions::from_env()` = o único ponto que lê o ambiente pool/TLS;
    `Database::connect_with(...)` = caminho env-free (framework define pool/TLS por módulo);
    `Database::from_env` agora **público** (conveniência nomeada, usada pela CLI). Legados
    `connect*` preservam comportamento via `from_env()`. Provado sem BD (deadpool lazy):
    pool dimensionado pela API.
  - 102 testes de lib `dorm-core` verdes; clippy --all-features -D warnings limpo; só código.
- **DORM-B1 item 3 — autodetect recusa rename ambíguo (2026-11-03)** — ramo
  `feat/dorm-b1-autodetect-refuse-ambiguous-rename` (off main, NÃO pushado), commit `295f6aa`.
  `autodetect.rs`+`lib.rs`: `diff_table` recusa um DROP+ADD de colunas do **mesmo tipo** não
  declarado (candidato a rename que perderia dados) com `Error::AmbiguousColumnChange`; guard
  corre antes do ALTER Postgres **e** do rebuild SQLite (que também perderia). Escape: declarar
  rename (ColumnRename/`@map`) ou separar em 2 migrações; tipos diferentes continuam permitidos.
  62 testes verdes; clippy limpo. Prompt interactivo fica p/ a CLI `migrate dev` (ainda não liga o autodetect).
  - **B1 COMPLETA** p/ efeitos do gateway: strict + config-por-API + cursor + item 3. Resta só
    `NO_PREPARE`/retry-slow ainda env em tempo de statement (menor) e a prova de concorrência live.
- **DORM-B2 (1.º corte) — namespaces no schema (2026-11-03)** — ramo
  `feat/dorm-b2-schema-namespaces` (off main, NÃO pushado), commit `ad70a26`.
  `dorm-schema`: `Model.namespace: Option<String>`; parser aceita
  `namespace NAME { model… enum… }` (sem aninhar); `validate` **recusa relação que
  atravessa namespace** (um modelo só se relaciona com modelos do seu namespace;
  ambos top-level coincidem). Schema sem namespaces inalterado (gera byte-a-byte
  igual). 41 testes verdes; clippy limpo. **Falta no B2:** história/pastas de
  migração por namespace; `generate` p/ caminho/crate escolhido com opções; API de
  metadados do schema; tipos de tabelas de suporte.
  - **Próximo DORM:** completar B2 (generate por namespace + metadados API) ou
    DORM-B4 (API de biblioteca); ou abrir os 4 PRs (DORM-C, B1×2, B2).
- **Seiva fundado (2026-11-03).** Repo `projects/seiva` criado e commitado
  (`1148506`), lado a lado com o DORM. Workspace de 3 crates verde (build/test/
  clippy/fmt); ADR 0001 aceite. **O tracker canónico passa a `../seiva/docs/progress.md`.**
- Lado DORM: nada iniciado. Próxima frente DORM recomendada: **DORM-C** (higiene
  de ADRs + ADR «Fronteira do ORM») ou **DORM-B4** (API de biblioteca), de que o
  Seiva depende para o gateway `seiva-data-dorm`.

## Decisões tomadas nesta sessão

- Arranque da campanha pela fundação do repo Seiva (escolha do dono).
- Núcleo do Seiva **não** liga ao DORM nesta fase (decisão 5): build offline; o
  DORM entrará só via `seiva-data-dorm` atrás da feature `dorm`.
- DORM continua intocado em `ad5073d`; nenhuma mudança no DORM nesta sessão.
