# ADR 0056 — Catálogo de modelos em runtime (`SchemaDescriptor`)

Status: **Proposed (pending)** · reframed per [ADR 0054](0054-orm-boundary.md)

> Renumbered from ADR 0011 — a 2026-10-31 review-branch proposal that collided
> with the accepted [ADR 0011](0011-multi-backend-and-production-roadmap.md).
> **Reframed:** exporting the model catalogue (fields, keys, indexes, version) as
> a programmatic **library metadata API** is legitimate for any tool built on
> DORM, so this stays on the roadmap. The polyglot/data-plane framing is dropped
> (ADR 0054); the descriptor is a Rust API, not a wire catalogue for remote clients.

## Context

O catálogo de modelos é montado pelo *linker*: `ModelMeta` é `&'static`, todos os
campos `&'static str`/`&'static [..]` (`crates/dorm-core/src/meta.rs:99-119`), e
`inventory::collect!(ModelMeta)` (`meta.rs:160`) junta-os numa *distributed
slice* que `registry::models()` itera (`crates/dorm-core/src/registry.rs:14-16`).
`ModelMeta` não deriva `Serialize` e não há forma de o exportar.

A consequência (achado GW-02) é que um servidor de dados só pode servir os
modelos compilados para dentro do seu próprio binário — o ADR 0001 descreve
acrescentar um modelo como "a recompile, not an edit"
(`0001-declarative-schema.md:22`). Não existe descritor que descreva os modelos a
um cliente poliglota nem a um Data Plane multi-esquema. O resolvedor de
identificadores (`registry::resolve`, `registry.rs:210`), guardião do invariante
"nenhum identificador vem do cliente", opera exclusivamente sobre a *slice*
estática. Sem descritor exportável, os SDKs poliglotas também não teriam fonte de
verdade sobre campos/tipos/relações (o mesmo defeito do OpenAPI à mão, GW-04).

## Decision

Projectar `ModelMeta`/`FieldMeta`/`Relation` (`meta.rs:45-119`) num descritor
serializável, `dorm.schema.v1.SchemaDescriptor`, **gerado a partir de
`dorm.schema`** — a mesma fonte única do ADR 0001 — e **carregável em runtime**
pelo Data Plane. O descritor só descreve estrutura: nomes lógicos de modelos e
campos, tipos (`FieldType`), relações, `tenant_scoped`/`tenant_field`
(`meta.rs:107`), `checks` e índices. Não transporta valores nem gera SQL.

Decisão central: **o descritor convive com o caminho compilado do ADR 0001, não o
substitui.** São dois modos de descrever exactamente a mesma coisa:

- **Caminho compilado (ADR 0001, mantido):** quem embebe o ORM em Rust continua
  com `inventory` e os tipos `&'static`; `dorm generate` continua a compilar o
  esquema para `src/models.rs`. Nada muda para o embedder.
- **Caminho em runtime (novo):** o Data Plane carrega um `SchemaDescriptor` e
  constrói um registo equivalente à *distributed slice*, para poder servir
  modelos que não conhece em tempo de compilação. O descritor é também a fonte de
  verdade dos SDKs poliglotas.

Os dois convivem porque `registry::validate` (`registry.rs:65`) e
`registry::resolve` (`registry.rs:210`) correm igual sobre qualquer dos dois — o
descritor é `ModelMeta` projectado, não um segundo modelo de dados. Um crate novo
`dorm-descriptor` aloja os tipos do descritor e a projecção `ModelMeta` ↔
descritor.

### Como preserva os invariantes do `ARCHITECTURE.md`

- **I1 (nenhum identificador vem do cliente):** o descritor **é** a lista de
  identificadores permitidos. A resolução valida contra ele tal como hoje valida
  contra a *slice* estática. Um Data Plane sem o descritor de um modelo recusa
  qualquer consulta a esse modelo — o conjunto permitido é sempre o que o
  descritor declara, nunca o que o pedido nomeia.
- **I2 / I3:** o descritor só descreve estrutura; não transporta valores nem gera
  SQL, pelo que não toca na ligação `$n` nem na pureza do compilador. A geração
  do descritor a partir do esquema é a mesma classe de operação do `dorm generate`
  — compilação de uma fonte, não I/O de runtime no caminho de consulta.

## Consequences

- **Prós:** o Data Plane passa a poder ser multi-esquema sem recompilar; os SDKs
  poliglotas ganham uma fonte de verdade única sobre campos/tipos/relações; o
  descritor pode ser servido por um RPC de *reflection* para clientes se
  descobrirem o esquema.
- **Contras / custo:** passam a existir duas projecções da mesma verdade (a
  *slice* `&'static` e o descritor) que têm de manter-se de acordo; a correcção
  dessa equivalência tem de ser garantida por teste, senão o runtime e o
  compilado divergem.
- **Testes necessários:** paridade descritor ↔ `ModelMeta` (o descritor gerado de
  `dorm.schema` descreve exactamente os mesmos modelos/campos/relações que a
  *slice* compilada — um irmão de `schema_parity.rs`); um `resolve` sobre um
  registo construído do descritor produz o mesmo `FieldMeta`/joins que sobre a
  *slice*; um modelo ausente do descritor é recusado.
- **Dependências:** consumido pelo ADR 0055 (o IR nomeia o que o descritor
  declara) e pelo ADR 0057 (o Data Plane carrega o descritor); o registo de
  objectos de BD do ADR 0058 estende o descritor.
