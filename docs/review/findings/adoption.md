# Adopção, DX, documentação e mercado — achados

> Eixo: fricção de adopção por um projecto maduro, experiência de programador,
> documentação como registo de decisões, e posicionamento face ao mercado
> (Diesel, SeaORM, sqlx; Prisma/Django como inspiração). Apenas leitura.
> Narrativa em português europeu (pré-1990); identificadores em inglês.
>
> Base: `docs/review/00-discovery.md` (H1–H17), `ARCHITECTURE.md`, todos os
> `crates/*/Cargo.toml`, `README.md`, `CONTRIBUTING.md`, `docs/` (book + ADRs),
> `crates/dorm-example/src/main.rs`, `crates/dorm-cli/src/`,
> `docs/cli-contract.md`, `docs/roadmap.md`, `.github/workflows/`.
>
> Os achados técnicos de correcção, concorrência, tipos, migrações, desempenho,
> segurança e poliglotismo são tratados pelos outros eixos
> (`dba.md`, `design.md`, `migrations.md`, `performance.md`, `security.md`,
> `gateway.md`); aqui só se **referenciam** enquanto decisores de adopção.

---

## [ADOPT-01] Sem crate-chapéu `dorm` e o nome está ocupado em crates.io — High
- **Hipótese:** nova (H18 — proposta).
- **Evidência:** `crates/dorm-cli/Cargo.toml:26` em comentário: *"the name `dorm`
  is taken on crates.io"*. O consumidor de biblioteca tem de montar **três**
  crates à mão — `README.md:79-84` e `docs/existing-project.md:13-22` pedem
  `dorm-core = "0.6"`, `dorm-derive = "0.3"`, `dorm-migrations = "0.7"` —, cada um
  numa linha de versão **independente** (`00-discovery.md:19-30`). Não existe
  nenhum crate `dorm` que reexporte um conjunto coerente.
- **Impacto:** o mercado com que compete entrega **uma** dependência: `diesel`,
  `sea-orm`, `sqlx`, `@prisma/client`. O DORM obriga o arquitecto a conhecer a
  topologia interna (que crate traz o quê, que versões combinam) logo no primeiro
  `Cargo.toml` — e a repeti-lo em cada crate de modelos do projecto. Pior: a
  solução idiomática (publicar um facade `dorm = { … }` que fixe o trio
  compatível) está **bloqueada** porque o nome `dorm` pertence a outrem em
  crates.io. É simultaneamente um atrito de DX e um problema de marca (o produto
  não controla o seu próprio namespace de distribuição).
- **Recomendação:** (a) publicar um crate-chapéu sob um nome livre
  (p. ex. `dorm-orm` ou `delonix-dorm`) com `pub use dorm_core::*` + feature
  `derive`/`migrations`, fixando internamente um trio testado; (b) em paralelo,
  tentar adquirir/transferir o nome `dorm` (RFC de squatting da crates.io, se
  aplicável); (c) até lá, a página de instalação deve dar **uma linha
  copiável** com as três versões coladas e dizer explicitamente que têm de
  andar em conjunto.
- **Esforço:** M
- **ADR/Tijolo:** ADR novo "Crate de fachada e política de versões"; tijolo:
  crate `dorm-*` facade + CI que valida o trio.

## [ADOPT-02] Versões independentes sem contrato SemVer entre `dorm-derive` e `dorm-core` — High
- **Hipótese:** nova (H19 — proposta); adjacente a H14.
- **Evidência:** `crates/dorm-derive/Cargo.toml` **não depende de `dorm-core`**
  (é proc-macro pura: só `syn`/`quote`/`proc-macro2`). O comentário de versão diz
  *"paired with dorm-core 0.6 and cannot run against 0.5"* — mas nada no Cargo o
  **impõe**. As versões movem-se em linhas próprias por desenho
  (`dorm-core 0.6.0`, `dorm-derive 0.3.0`, `dorm-migrations 0.7.1`,
  `dorm-schema 0.3.1`, `dorm-studio 0.1.1`, `dorm-sqlite 0.2.1`,
  `dorm-cli 0.13.0`). O `release.yml` publica cada um "anything not yet on the
  index", reforçando a divergência.
- **Impacto:** um consumidor que escreva `dorm-core = "0.5"` + `dorm-derive = "0.3"`
  (ou qualquer combinação futura desalinhada) resolve **com sucesso** no Cargo e
  só depois falha a compilar o código **gerado pela macro**, com um erro de tipo
  obscuro dentro de código que ele não escreveu — o pior tipo de erro de
  adopção, porque não aponta para a causa (versões incompatíveis). O `dorm-cli`
  fixa o seu trio (`=0.6.0`/`=0.7.1`/…), mas o utilizador de biblioteca não tem
  essa rede. Diesel e SeaORM evitam isto com versionamento em lockstep (todos os
  crates na mesma versão) ou fazendo o crate de derive depender de uma versão
  concreta do core.
- **Recomendação:** o crate-chapéu de ADOPT-01 resolve a maioria. Adicionalmente:
  fazer o código **gerado** pela derive referir uma constante de versão do core
  (`dorm_core::DERIVE_ABI`) verificada em tempo de compilação, de modo a produzir
  uma mensagem clara ("`dorm-derive 0.3` exige `dorm-core >= 0.6`") em vez de um
  erro de tipo cru. Ou adoptar lockstep para o par core/derive.
- **Esforço:** M
- **ADR/Tijolo:** mesmo ADR de política de versões; tijolo: guarda de ABI
  derive↔core.

## [ADOPT-03] H14 — colisão do binário `dorm` (refutada em crates.io, confirmada no workspace) — Medium
- **Hipótese:** H14 — **parcialmente refutada, parcialmente confirmada**.
- **Evidência:** `dorm-cli/Cargo.toml:27-29` e `dorm-shell/Cargo.toml:14-18`
  declaram ambos `[[bin]] name = "dorm"`. **Mas** `dorm-shell` é
  `publish = false` (`Cargo.toml:…` + `release.yml` não o publica — a lista é
  core/derive/migrations/schema/sqlite/studio/cli). Logo `cargo install dorm-shell`
  **não existe**: no caminho do utilizador final (crates.io) há **um único**
  `dorm`, o do `dorm-cli`. A hipótese "cargo install de ambos colide" está
  **refutada** para quem adopta. A colisão real é **local, para quem desenvolve
  o DORM**: (1) `cargo build --workspace` gera dois alvos com o mesmo nome de
  ficheiro de saída (`target/debug/dorm`) → aviso de *output filename collision*;
  (2) `cargo run --bin dorm` é ambíguo (desambiguado só por `-p`); (3)
  `cargo install --path crates/dorm-shell` sobreporia `~/.cargo/bin/dorm`. O
  `doc = false` no shell (`dorm-shell/Cargo.toml:16-18`) já trata **só** a
  colisão de `cargo doc`, não a do artefacto de build.
- **Impacto:** nulo para o adoptante; baixo mas presente para o contribuidor
  (aviso de build ruidoso, `cargo run` que exige `-p` sempre). Um aviso de
  colisão no build de um projecto que se quer "grau DBA, maduro" arranha a
  primeira impressão de quem clona o repo para contribuir.
- **Recomendação:** renomear o binário do REPL para `dorm-repl` (ou
  `dorm-shell`) em `dorm-shell/Cargo.toml`. Custo zero para utilizadores
  (crate não publicado, é prova de independência) e elimina o aviso de workspace.
- **Esforço:** S
- **ADR/Tijolo:** tijolo pequeno; sem ADR.

## [ADOPT-04] Curva de entrada: o "cargo lane" é um imposto conceptual inerente a um ORM de compilação — Medium
- **Hipótese:** nova (H20 — proposta).
- **Evidência:** `crates/dorm-cli/src/main.rs:1-16` e `docs/cli-contract.md:64-73`
  ("Lanes"): porque os modelos só existem quando **compilados no binário do
  utilizador**, metade dos comandos (`validate`, `migrate dev/deploy/diff`,
  `push`, `shell`) não correm no binário instalado — reconstroem e correm um
  binário do **próprio projecto** via `cargo`. Só `doctor`, `db *`,
  `migrate status/resolve`, `init`, `new model`, `generate`, `studio` (nativo)
  correm no `dorm` instalado.
- **Impacto:** o modelo mental "instalo `dorm` e ele gere o meu projecto" (que
  o Prisma dá) é **parcial** no DORM: `dorm migrate` recompila o projecto. Para
  quem vem do Prisma/Django é uma surpresa — a CLI está honestamente documentada,
  mas a honestidade não remove a fricção: build do projecto antes de cada
  migração, `--json` "forwarded to the binary", e um `dorm migrate` que falha se
  o projecto não compilar. É o preço (legítimo) da segurança em tempo de
  compilação, mas é um custo de adopção que o mercado interpretado (Prisma) não
  cobra.
- **Recomendação:** manter o desenho (é correcto), mas: (a) uma página de guia
  dedicada "Porque é que `dorm migrate` compila o meu projecto" com o modelo
  mental explícito; (b) cache/feedback de build claro (tempo de recompilação
  visível); (c) um `dorm doctor` que detecte "projecto não compila" cedo e o
  explique, em vez de o erro vir cru do `cargo`.
- **Esforço:** S (docs) / M (UX de feedback)
- **ADR/Tijolo:** sem ADR; tijolo de UX + página de book.

## [ADOPT-05] Tecto de capacidades vs. "projecto grande e maduro" — Medium
- **Hipótese:** H5/H1/H4/H6/H8 (confirmadas noutros eixos) — relevância de adopção.
- **Evidência:** `docs/roadmap.md` e os achados de `dba.md`/`migrations.md`:
  PostgreSQL é o único backend **shipped** (SQLite parcial, `roadmap.md:60-89`);
  conjunto de tipos estreito (H5 — faltam f64, DATE/TIME, BYTEA, arrays, enums,
  INET; `value.rs`); pool global único sem routing por tenant/réplica (H1); sem
  controlo de concorrência/retry (H4); objectos de BD não geridos — views,
  funções, triggers, RLS, partições (H6). O `codegen_api`/API gerada não é de
  produção (H11).
- **Impacto:** para o arquitecto que pondera um projecto **grande** hoje, estes
  são os verdadeiros travões de adopção — não o H14. Um sistema multi-tenant com
  réplicas de leitura, enums de domínio, RLS e migrações expand/contract online
  **não** cabe no DORM 0.x actual. O roadmap reconhece-o com honestidade (ponto
  forte), mas a decisão de adopção tem de ser lida como "PG simples, esquema
  modesto, equipa Rust que valoriza segurança de compilação" — não "substituir o
  Django/Prisma num monólito maduro".
- **Recomendação:** publicar uma **matriz de capacidades honesta** (ver a tabela
  no fim) na documentação, no formato que o próprio roadmap promete ("reflects
  only what is demonstrated"), para que a decisão de adopção seja informada e não
  por descoberta tardia. Posicionar explicitamente o público-alvo actual.
- **Esforço:** S (documentar) — a implementação é dos outros eixos/roadmap.
- **ADR/Tijolo:** sem ADR novo; alimenta o roadmap e a página de matriz.

## [ADOPT-06] Documentação: cobertura forte, lacunas de adopção concretas — Medium
- **Hipótese:** n/a (observação de DX).
- **Evidência:** `docs/SUMMARY.md` — book completo (introduction, installation,
  quickstart, existing-project, schema, models, queries, migrations,
  connection-and-transactions, managing-the-database, studio, cli-contract,
  api-reference, roadmap + 9 ADRs). `cli-contract.md` com legenda de estado
  (**done/partial/planned**) e a regra "No command is shipped empty to simulate
  parity" — **exemplar**. Dois percursos de arranque (`quickstart.md` com CLI,
  `existing-project.md` library-first sem BD).
- **Impacto:** a cobertura é das melhores que um ORM 0.x costuma ter; o
  `cli-contract.md` é um instrumento de confiança raro. **Lacunas** que pesam na
  adopção: (1) os guias "Prisma → DORM" e "Django → DORM" ainda são roadmap
  (`roadmap.md:142-144`, Fase 7) — são **exactamente** o que converte um
  adoptante que já conhece o mercado; (2) não há página de comparação/benchmark
  (o adoptante tem de inferir onde o DORM ganha/perde); (3) os links docs.rs do
  README dependem de publicação por release — para crates ainda não no índice
  ficam 404.
- **Recomendação:** priorizar os dois guias de migração de conceito (são o
  maior multiplicador de adopção por esforço), e uma página "DORM vs o mercado"
  com a matriz honesta. Manter a disciplina do `cli-contract.md` em todo o lado.
- **Esforço:** M
- **ADR/Tijolo:** sem ADR; tijolos de documentação (Fase 7 do roadmap).

## [ADOPT-07] Sinais de maturidade para um DBA contribuir — ponto forte (Low)
- **Hipótese:** n/a (observação).
- **Evidência:** `CONTRIBUTING.md` — "`make check` é o portão inteiro"
  (fmt+clippy -D warnings+test), tudo **offline** (compilador puro, snapshots sem
  conexão); "Rules that are tests" (SQL fixado em snapshot *com ordem de
  parâmetros*, nenhum identificador vem do chamador, nenhum valor formatado em
  SQL, gerador de esquema provado contra oráculo congelado); Conventional Commits
  com `!`/`BREAKING CHANGE`. CI em GitHub (`ci.yml`: fast lane sem BD + docs com
  `-D warnings` + integration com PG 16 service) **e** espelho GitLab
  (`.gitlab-ci.yml`). ADRs 0001–0009 como registo de decisões.
- **Impacto:** um DBA/engenheiro que avalie contribuir encontra um portão
  reproduzível, testes que **são** as regras de correcção, e decisões
  documentadas — barreira de entrada baixa e confiança alta. Os ADRs são o maior
  atractor: explicam o *porquê* (multi-backend, db genérico, TLS, SQL raw,
  observabilidade) e não só o *o quê*. É o diferencial mais forte do DORM face a
  muitos ORMs jovens.
- **Lacunas menores:** sem `CHANGELOG.md` visível (as notas de versão vivem em
  comentários de `Cargo.toml` e no `generate_release_notes` do GitHub); sem
  templates de issue/PR em `.github/`; sem `rust-toolchain.toml` (MSRV declarado
  em `rust-version = 1.75`, mas não fixado para o contribuidor).
- **Recomendação:** acrescentar `CHANGELOG.md` por crate (Keep a Changelog),
  templates de issue/PR, e opcionalmente `rust-toolchain.toml`. Baixo custo,
  completa a impressão de maturidade.
- **Esforço:** S
- **ADR/Tijolo:** sem ADR.

## [ADOPT-08] `dorm-example` prova a independência — ponto forte (Low)
- **Hipótese:** independência (ADR 0073) — **confirmada**.
- **Evidência:** `crates/dorm-example/Cargo.toml` depende **só** de
  `dorm-core`+`dorm-derive`+`dorm-migrations`+`chrono`; `publish = false`;
  comentário: *"`cargo tree -p dorm-example` showing only `dorm-*` + third-party
  is the proof that the ORM is usable by any Rust application"*.
  `crates/dorm-example/src/main.rs` (~76 linhas): define `Author`/`Post`, imprime
  o esquema a rever, a migração inicial e o SQL de uma query tipada — **tudo sem
  base de dados**. `dorm-shell` é uma segunda prova (REPL que só lê o registry).
- **Impacto:** é a melhor peça de marketing técnico do repo: em 40 linhas um
  avaliador vê o valor central (SQL inspeccionável sem BD, segurança por
  construção) e confirma que não há acoplamento ao control-plane NgolaCloud.
  `cargo run -p dorm-example` é uma demo honesta de 5 segundos.
- **Recomendação:** promovê-lo no topo do README como o **primeiro** passo
  ("experimenta sem instalar nada"); hoje aparece a meio ("Try it without a
  database", `README.md:107-114`). Considerar um teste de CI que corre
  `cargo tree -p dorm-example` e falha se aparecer qualquer dependência fora de
  `dorm-*`+conhecidas — tornaria a "prova de independência" executável.
- **Esforço:** S
- **ADR/Tijolo:** tijolo de CI (independence gate).

## [ADOPT-09] Ecossistema de ferramentas (CLI/Studio/REPL) como atractor — ponto forte (Low)
- **Hipótese:** n/a (observação de posicionamento).
- **Evidência:** `cli-contract.md` — `dorm init/generate/db pull/format` (fluxo
  Prisma-CLI), `dorm studio` (UI web estilo Prisma Studio, `dorm-studio`), `dorm
  shell` (REPL com DML + transacções interactivas `:begin/:commit/:rollback`,
  ADR 0006), `dorm api generate/serve` (CRUD + OpenAPI). `dorm.schema`
  Prisma-fiel (`quickstart.md:41-61`, ADR 0001). Completions, `--json` estável,
  códigos de saída, redacção de credenciais.
- **Impacto:** nenhum ORM Rust do mercado oferece o conjunto
  *schema declarativo + Studio GUI + REPL dinâmico + CLI de migrações*. Diesel e
  sqlx têm CLIs mínimas; SeaORM tem `sea-orm-cli` (geração) mas sem Studio/REPL.
  É **o** diferencial de adopção face aos pares Rust: aproxima a DX do Prisma,
  que é o padrão-ouro de ergonomia. Para uma equipa que hesita em trocar o
  conforto do Prisma por Rust, este ecossistema é o argumento decisivo.
- **Recomendação:** tornar o Studio e o schema declarativo o **herói** do
  material de adopção (README + landing do book), porque é aí que o DORM ganha a
  comparação emocional contra Diesel/sqlx. Garantir paridade de documentação com
  capturas de ecrã do Studio.
- **Esforço:** S
- **ADR/Tijolo:** sem ADR.

---

## DORM vs mercado

Honesta, com base no estado **shipped** (não roadmap). Legenda: ✅ sólido ·
🟡 parcial/limitado · ❌ ausente · n/a não se aplica.

| Capacidade | DORM | Diesel | SeaORM | sqlx | Prisma (TS) |
|---|---|---|---|---|---|
| Segurança de query em tempo de compilação | ✅ identificadores `&'static`, valores sempre `$n` | ✅ tipos fortes DSL | 🟡 entities tipadas, query builder mais solto | 🟡 `query!` verifica contra BD viva em build | 🟡 tipos gerados, runtime JS |
| Dois front-ends (tipado + dinâmico) sobre **um** compilador | ✅ único no mercado (prova por snapshot) | ❌ | ❌ | ❌ | ❌ |
| SQL revisável **sem** base de dados | ✅ compilador puro, snapshots, `query.sql()` | 🟡 `debug_query` | 🟡 `build().to_string()` | ❌ `query!` precisa de BD em build | 🟡 `migrate diff` precisa shadow DB |
| ADRs como registo de decisões | ✅ 0001–0009, excepcional p/ 0.x | 🟡 RFCs dispersos | ❌ | ❌ | 🟡 blog/specs |
| Backends suportados | 🟡 só PG (SQLite parcial) | ✅ PG/MySQL/SQLite | ✅ PG/MySQL/SQLite | ✅ PG/MySQL/SQLite/MSSQL | ✅ PG/MySQL/SQLite/MSSQL/Mongo |
| Amplitude de tipos | 🟡 estreita (sem f64/DATE/BYTEA/arrays/enums) — H5 | ✅ ampla | ✅ ampla | ✅ ampla (tipos nativos) | ✅ ampla |
| Concorrência (isolamento, retry, locks) | ❌ pool global, sem retry/FOR UPDATE — H1/H4 | 🟡 via diesel-async | 🟡 | ✅ controlo manual explícito | 🟡 interactive tx |
| Objectos de BD (views/triggers/RLS/partições) | ❌ — H6 | 🟡 via `sql_function!`/raw | ❌ | ✅ (escreves o SQL) | 🟡 limitado |
| Schema declarativo + codegen | ✅ `dorm.schema` Prisma-fiel → Rust | ❌ (schema.rs gerado da BD) | 🟡 `sea-orm-cli generate` da BD | ❌ | ✅ `schema.prisma` (a referência) |
| GUI/Studio | ✅ `dorm studio` (edição opt-in) | ❌ | ❌ | ❌ | ✅ Prisma Studio |
| REPL dinâmico | ✅ `dorm shell` (DML + tx interativas) | ❌ | ❌ | ❌ | 🟡 `prisma studio`/`db execute` |
| Uma só dependência (crate-chapéu) | ❌ montar 3 crates, versões independentes — ADOPT-01/02 | ✅ `diesel` | ✅ `sea-orm` | ✅ `sqlx` | ✅ `@prisma/client` |
| Maturidade/escala em produção | 🟡 0.x, PG simples | ✅ anos em produção | ✅ | ✅ amplamente usado | ✅ padrão da indústria TS |

---

## Mensagem final

Os **três maiores atritos de adopção**, por ordem de peso para um arquitecto:

1. **Tecto de capacidades (ADOPT-05).** Não é o H14 — é a amplitude de tipos
   estreita, o PostgreSQL como único backend sólido, o pool global sem routing e
   a ausência de objectos de BD (views/RLS/partições) e de controlo de
   concorrência. Um projecto *grande e maduro* não cabe hoje; o DORM serve
   esquemas PG modestos com equipas Rust que valorizam segurança de compilação.
2. **Fricção de empacotamento (ADOPT-01/02).** Não há crate-chapéu `dorm` — e o
   nome está ocupado em crates.io —, pelo que o adoptante monta três crates de
   versões independentes **sem** contrato SemVer entre `dorm-derive` e
   `dorm-core`, com risco de erros de compilação obscuros por combinação
   desalinhada. O mercado inteiro entrega uma só dependência coerente.
3. **O "cargo lane" (ADOPT-04).** `dorm migrate`/`push`/`shell` recompilam o
   projecto do utilizador — preço legítimo de um ORM de compilação, mas uma
   surpresa de modelo mental para quem vem do Prisma/Django, agravada pela
   ausência (ainda) dos guias de migração de conceito.

O H14 (colisão de binários) é, para o adoptante, um **não-problema**: `dorm-shell`
não é publicado, logo em crates.io só existe um `dorm`; a colisão é um aviso de
build local, de correcção trivial (renomear o binário do REPL). Em contrapartida,
os pontos fortes são reais e invulgares num ORM 0.x: ADRs como registo de
decisões, `cli-contract.md` com estado honesto e a regra "nenhum comando vazio",
o `dorm-example` como prova executável de independência, e um ecossistema
(schema declarativo + Studio + REPL + CLI Prisma-style) que nenhum par Rust
oferece.

**Posicionamento numa frase:** *o DORM é o ORM de Rust com a ergonomia do Prisma —
schema declarativo, Studio, REPL e CLI de migrações — e segurança SQL por
construção provada sem base de dados, hoje para PostgreSQL de esquema modesto, e
ainda a caminho das capacidades de escala e multi-backend que o mercado maduro
exige.*
