# ADR 0047 — Bloco `enum` de topo no `dorm.schema` emite campos de enum nativos

Status: proposto · 2026-11-03 · estende o esquema declarativo
([ADR 0001](0001-declarative-schema.md)) e assenta na capacidade de **enums
nativos** do derive ([ADR 0046](0046-native-enum-types.md)).

> Nota de numeração. O livro de ADRs *real* deste repositório vai até `0020`
> (o código é a fonte de verdade — ARCHITECTURE.md). Os números `0046`/`0047`
> são os da frente de trabalho dos enums nativos: a `0046` introduziu a
> capacidade no `#[derive(Model)]` e provou-a ao vivo; esta `0047` fecha a via
> **declarativa**, gerando essa mesma capacidade a partir do `dorm.schema`. As
> duas são irmãs e entram juntas.

## Contexto

A capacidade nativa já existe (ADR 0046): o `#[derive(Model)]` aceita
`#[field(enum_type = "order_status", enum_variants = "pending, paid, shipped")]`
num campo `String` e traduz isso num `CREATE TYPE … AS ENUM` nativo do Postgres,
imposto pela base de dados. Essa via está provada ao vivo (`enum_types_live.rs`).

O que faltava era a via **declarativa**: um utilizador que descreve o seu domínio
em `dorm.schema` não tinha como declarar um enum. Pior, o parser interpretava mal
a intenção — um campo `status OrderStatus` cujo tipo não é um escalar conhecido é
classificado como **relação** (`Field::ToOne`) para um modelo chamado
`OrderStatus`, porque `fn field()` só distingue escalares de relações. Logo o
schema não tinha sequer como exprimir «este campo é um dos destes valores», e o
`docs/schema.md` listava «a top-level `enum` block» em *Not yet*.

Três forças moldam a decisão:

1. **Aproveitar a capacidade, não duplicá-la.** A emissão de SQL, a imposição e a
   prova ao vivo são todas da ADR 0046. Esta ADR só gera o atributo a partir do
   bloco — não toca em `dorm-core`, `dorm-derive` nem no migrador.
2. **A ordem de declaração não pode importar.** Um enum pode ser declarado depois
   do modelo que o usa (como em Prisma). O parser não pode decidir no sítio se
   `OrderStatus` é um enum ou um modelo — só no fim, quando todo o schema é
   conhecido.
3. **O formatador continua a trabalhar sobre texto.** O `dorm format` é lossless
   por construção (formata tokens e linhas, não o AST). Um bloco `enum` tem de
   sobreviver a um round-trip de formatação como qualquer outro bloco.

## Decisão

Adicionar um bloco `enum <Name> { v1 v2 … }` de topo ao `dorm.schema`, trabalhando
**exclusivamente em `crates/dorm-schema`**. As variantes são identificadores nus
separados por espaço/newline, até `}` — o estilo Prisma. Um campo tipado como um
enum declarado é compilado no atributo nativo da ADR 0046.

### AST

- `EnumDecl { name, variants }` e `Schema.enums: Vec<EnumDecl>`.
- Uma variante nova `Field::Enum(EnumField { name, column, enum_name, nullable })`.

Ambos derivam os mesmos traits que os restantes nós (`Debug, Clone, PartialEq,
Eq`). `ModelMeta`, `Value` e os restantes tipos do núcleo **não mudam** — este é
texto-para-texto, não toca no núcleo do ORM.

### Parser: resolução no fim, não no sítio

`fn enum_decl()` parseia o bloco e é ligado ao despacho de `parse()` em
`kw == "enum"`. Como a ordem de declaração não pode importar, `fn field()`
**não** tenta resolver o tipo: um `status OrderStatus` continua a nascer como
`Field::ToOne(Relation { target: "OrderStatus", … })`, exactamente como hoje.

Depois de todo o schema estar parseado, corre um **passo de resolução**: para cada
campo de cada modelo que é uma relação-a-um cujo `target` é o nome de um `enum`
declarado, reescreve-o em `Field::Enum`, preservando a `column` e a `nullable` da
relação (incluindo um `@map("col")` e o `?`). Um `target` que não é um enum fica
relação e segue o fluxo existente — um tipo inexistente já é um erro desse fluxo,
pelo que um `Field::Enum`, por construção, nunca refere algo inexistente.

### Codegen

Para um `Field::Enum`, emite

```rust
#[field(enum_type = "<snake(enum_name)>", enum_variants = "v1, v2, v3")]
pub <snake(name)>: String,
```

(ou `Option<String>` se `nullable`). O nome do tipo é o snake_case do nome do enum
(`OrderStatus` → `order_status`), via o helper `snake()` existente; as variantes
são emitidas tal como declaradas, juntas por `", "`. Um schema **sem** enum gera
byte-a-byte o que gerava antes — nenhum `enum_type` aparece.

### Validação

`validate()` ganha, por cada `enum`: erro se não tiver variantes; erro se tiver
variantes duplicadas; erro se o nome colidir com um modelo (ambos compilam num
objecto nomeado da base de dados).

### Formatação

O formatador é textual, pelo que um bloco `enum` com uma variante por linha
sobrevive verbatim e é um ponto fixo, como qualquer bloco. Um teste fixa isto.

## Consequências

- Um `dorm.schema` passa a exprimir enums de forma declarativa; a emissão é a via
  nativa da ADR 0046, já provada ao vivo.
- A ordem de declaração é irrelevante — o passo de resolução só corre quando todo
  o schema é conhecido.
- Um schema sem enum é inalterado; a dependência e a saída são idênticas.
- O núcleo do ORM não muda: sem novas variantes de `Value`/`FieldType`, sem campos
  novos em `ModelMeta`.

## Prova

Esta ADR **não precisa de prova ao vivo própria**: o comportamento em PostgreSQL é
o da ADR 0046 (`enum_types_live.rs`), e esta emissão apenas passa a produzir, a
partir do `dorm.schema`, exactamente o atributo que esse teste já exercita. O que
se prova aqui é a emissão — offline, por testes unitários de `dorm-schema`:

- parse de `enum OrderStatus { pending paid shipped }` + `status OrderStatus`
  (enum declarado *depois* do modelo) → `Schema.enums` tem o `EnumDecl` e o campo
  é `Field::Enum` com `enum_name == "OrderStatus"`; um `OrderStatus?` com `@map`
  preserva nulabilidade e coluna;
- `generate_rust` contém exactamente
  `#[field(enum_type = "order_status", enum_variants = "pending, paid, shipped")]`
  e `pub status: String,` (e `Option<String>` quando nulável);
- um schema sem enum não emite `enum_type`/`enum_variants`;
- `validate` recusa um enum vazio, um com variante duplicada e um que colide com
  um modelo;
- o bloco `enum` sobrevive a um round-trip de `format` e é um ponto fixo.

`make check` (fmt + clippy `-D warnings` + testes offline) fica verde.

## O que isto não é

- **Não é uma mudança ao `dorm-core` nem ao `dorm-derive`.** A capacidade nativa é
  da ADR 0046; esta ADR só gera o atributo a partir do schema.
- **Não gera `ALTER TYPE … ADD VALUE`.** Acrescentar uma variante a um enum vivo
  continua fora, tal como na ADR 0046 — mudar o conjunto de variantes de um enum
  já migrado não é, por agora, uma migração gerada.
- **Não é uma prova ao vivo nova.** A imposição em Postgres é a da ADR 0046; aqui
  prova-se a emissão, offline.
