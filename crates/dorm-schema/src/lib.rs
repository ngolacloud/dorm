//! `dorm-schema` — parse a Prisma-style `dorm.schema` and generate the Rust it
//! compiles to.
//!
//! DORM is a **compile-time** ORM, so the declarative schema is not read at
//! runtime: [`parse`] turns `dorm.schema` into a [`Schema`], and
//! [`generate_rust`] emits a `models.rs` of `#[derive(Model)]` structs. The
//! generated file is the single source of truth's *output* — you edit the
//! schema and re-run `dorm generate`, never the Rust. This keeps the schema's
//! Prisma ergonomics and the typed, zero-overhead Rust at once.
//!
//! The crate is pure: text in, text out, no I/O and no dependency on the ORM
//! core. The CLI writes the file.
//!
//! ## Supported grammar (v0.1)
//!
//! ```text
//! datasource db { provider = "postgresql"  url = env("DELONIX_ORM_DSN") }
//!
//! model Tenant {
//!   id        Int      @id @default(autoincrement())
//!   slug      String   @unique @index @db.VarChar(63)
//!   name      String   @db.VarChar(200)
//!   createdAt DateTime @default(now())
//!   apps      Application[]                 // reverse relation
//! }
//!
//! model Application {
//!   id     Int    @id @default(autoincrement())
//!   tenant Tenant @relation(onDelete: Cascade)   // → related_name "apps"
//!   name   String @db.VarChar(63) @index
//! }
//! ```
//!
//! Scalars: `Int`→i32, `BigInt`→i64, `String`→String, `Boolean`→bool,
//! `DateTime`→`DateTime<Utc>`, `Decimal`→Decimal, `Json`→serde_json::Value,
//! `Uuid`→uuid::Uuid. `@id @default(autoincrement())` becomes a `BIGSERIAL`
//! (i64) key. `@default(now())`→`auto_now_add`, `@updatedAt`→`auto_now`,
//! `@db.VarChar(n)`→`max_length`, `@map`/`@@map` rename a column/table, `?`
//! makes a column nullable (`Option<T>`).

#![forbid(unsafe_code)]

mod codegen;
mod codegen_api;
mod format;
mod lexer;
mod parser;
mod validate;

pub use codegen::generate_rust;
pub use codegen_api::generate_axum;
pub use format::format;
pub use parser::{parse, ParseError};
pub use validate::{validate, Issue, Severity};

/// A parsed `dorm.schema`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    pub datasource: Option<Datasource>,
    pub models: Vec<Model>,
    /// Top-level `enum` blocks (ADR 0047). A model field typed as one of these
    /// becomes a [`Field::Enum`], compiled to a native database enum (ADR 0046).
    pub enums: Vec<EnumDecl>,
}

/// A top-level `enum Name { v1 v2 … }` block (ADR 0047).
///
/// It declares a named set of string variants. A `String`-shaped field typed as
/// the enum (`status OrderStatus`) is compiled by [`generate_rust`] into the
/// native `#[field(enum_type = …, enum_variants = …)]` the `Model` derive turns
/// into a `CREATE TYPE … AS ENUM` (ADR 0046).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumDecl {
    /// The schema name, e.g. `OrderStatus` (snake_cased to the SQL type name).
    pub name: String,
    /// The variants, in declaration order, spelled exactly as written.
    pub variants: Vec<String>,
}

/// The `datasource` block. Only `provider` and `url` are read today.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Datasource {
    pub name: String,
    pub provider: String,
    pub url: Url,
}

/// A datasource URL: an `env("VAR")` reference or an inline string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Url {
    Env(String),
    Literal(String),
}

/// One `model` block → one Rust struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    /// The Rust type name, e.g. `Tenant`.
    pub name: String,
    /// The PostgreSQL schema (`namespace`) this model belongs to, or `None` for a
    /// top-level model (ADR: namespaces, DORM-B2). The namespace — not the file —
    /// is a model's identity: moving a `namespace` block between files changes
    /// nothing. A relation may not cross namespaces; [`validate()`] enforces that
    /// boundary.
    pub namespace: Option<String>,
    /// The table, defaulting to the snake_case plural of `name`.
    pub table: String,
    pub fields: Vec<Field>,
    /// The `@@rag(..)` block, when the model opts into RAG (ADR 0019). `None`
    /// for an ordinary model — and a schema with no `@@rag` anywhere generates
    /// byte-for-byte what it did before, pulling in no `dorm-rag` dependency.
    pub rag: Option<RagBlock>,
    /// `@@unique([a, b])` groups — composite-unique constraints (ADR 0043).
    pub unique_together: Vec<Vec<String>>,
    /// `@@id([a, b])` — the composite primary key's columns, empty for a simple
    /// or absent key (ADR 0043).
    pub composite_id: Vec<String>,
    /// `@@index([a, b])` groups — composite indexes (ADR 0044).
    pub indexes: Vec<Vec<String>>,
}

/// A model's `@@rag(..)` declaration, parsed from the schema.
///
/// Compiled by [`generate_rust`] into `#[derive(Rag)]` + `#[rag(..)]` and checked
/// by [`validate()`]. The field names are the schema spellings; the generator
/// snake_cases them to match the generated Rust fields and columns.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RagBlock {
    /// The source fields to embed (`fields: [title, body]`).
    pub fields: Vec<String>,
    /// The document template (`template: "{title}\n\n{body}"`), if given.
    pub template: Option<String>,
    /// The embedder id (`embedder: "default"`), if given.
    pub embedder: Option<String>,
    /// `chunk_size: 800`, if given.
    pub chunk_size: Option<u32>,
    /// `chunk_overlap: 100`, if given.
    pub chunk_overlap: Option<u32>,
    /// Filterable metadata columns (`metadata: [status]`).
    pub metadata: Vec<String>,
    /// `distance: "cosine"`, if given.
    pub distance: Option<String>,
}

impl Model {
    /// The primary-key scalar field, if the model declares one.
    pub fn primary_key(&self) -> Option<&ScalarField> {
        self.fields.iter().find_map(|f| match f {
            Field::Scalar(s) if s.primary_key => Some(s),
            _ => None,
        })
    }
}

/// One line inside a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field {
    Scalar(ScalarField),
    /// A to-one relation: this model holds the foreign key.
    ToOne(Relation),
    /// A to-many back-reference (`Target[]`): no column, it only names the
    /// `related_name` the far side's foreign key is reached by.
    ToMany {
        name: String,
        target: String,
    },
    /// A field typed as a top-level `enum` (ADR 0047): a `String` column backed
    /// by a native database enum (ADR 0046). Produced by the parser's resolution
    /// pass from a field whose declared type names a declared `enum`.
    Enum(EnumField),
}

/// A model field typed as a top-level `enum` (ADR 0047).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumField {
    /// The schema field name (Rust field and column are its snake_case form).
    pub name: String,
    pub column: String,
    /// The name of the referenced [`EnumDecl`], e.g. `OrderStatus`.
    pub enum_name: String,
    pub nullable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScalarField {
    /// The schema field name (Rust field and column are its snake_case form).
    pub name: String,
    pub column: String,
    pub ty: Scalar,
    pub nullable: bool,
    pub primary_key: bool,
    pub auto_increment: bool,
    pub unique: bool,
    pub index: bool,
    pub max_length: Option<u32>,
    /// `auto_now_add` (`@default(now())`) or `auto_now` (`@updatedAt`).
    pub auto: Option<Auto>,
    /// A literal SQL default, already rendered (e.g. `'active'`, `0`, `true`).
    pub default_sql: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    /// The schema field name (becomes the foreign-key column, snake_cased).
    pub name: String,
    pub column: String,
    pub target: String,
    pub nullable: bool,
    pub on_delete: OnDelete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scalar {
    Int,
    BigInt,
    String,
    Boolean,
    DateTime,
    Decimal,
    Json,
    Uuid,
    /// A host address, `INET` (ADR 0040). PostgreSQL only.
    Inet,
    /// A network block, `CIDR` (ADR 0048). PostgreSQL only.
    Cidr,
    /// A MAC address, `MACADDR` (ADR 0048). PostgreSQL only.
    MacAddr,
}

impl Scalar {
    /// The Rust type the column decodes as.
    ///
    /// Fully-qualified for the network types so the generated file needs no new
    /// `use`: the `Model` derive infers the `FieldType` from the path's last
    /// segment (`IpAddr`→INET, `IpCidr`→CIDR, `MacAddress`→MACADDR), so these
    /// resolve with no import even though a project that uses them must depend on
    /// the `cidr`/`eui48` crates (as a `Decimal` project depends on `rust_decimal`).
    pub fn rust_type(self) -> &'static str {
        match self {
            Scalar::Int => "i32",
            Scalar::BigInt => "i64",
            Scalar::String => "String",
            Scalar::Boolean => "bool",
            Scalar::DateTime => "DateTime<Utc>",
            Scalar::Decimal => "Decimal",
            Scalar::Json => "serde_json::Value",
            Scalar::Uuid => "uuid::Uuid",
            Scalar::Inet => "std::net::IpAddr",
            Scalar::Cidr => "cidr::IpCidr",
            Scalar::MacAddr => "eui48::MacAddress",
        }
    }

    fn parse(s: &str) -> Option<Scalar> {
        Some(match s {
            "Int" => Scalar::Int,
            "BigInt" => Scalar::BigInt,
            "String" => Scalar::String,
            "Boolean" => Scalar::Boolean,
            "DateTime" => Scalar::DateTime,
            "Decimal" => Scalar::Decimal,
            "Json" => Scalar::Json,
            "Uuid" => Scalar::Uuid,
            "Inet" => Scalar::Inet,
            "Cidr" => Scalar::Cidr,
            "MacAddr" => Scalar::MacAddr,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auto {
    NowAdd,
    Now,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnDelete {
    Cascade,
    Restrict,
    SetNull,
    NoAction,
}

impl OnDelete {
    /// The spelling the `#[field(on_delete = "…")]` attribute expects.
    pub fn attr(self) -> &'static str {
        match self {
            OnDelete::Cascade => "cascade",
            OnDelete::Restrict => "restrict",
            OnDelete::SetNull => "set_null",
            OnDelete::NoAction => "do_nothing",
        }
    }

    fn parse(s: &str) -> Option<OnDelete> {
        Some(match s {
            "Cascade" => OnDelete::Cascade,
            "Restrict" => OnDelete::Restrict,
            "SetNull" => OnDelete::SetNull,
            "NoAction" => OnDelete::NoAction,
            _ => return None,
        })
    }
}

/// `PascalCase`/`camelCase` → `snake_case`, for Rust field and column names.
pub(crate) fn snake(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i != 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// A modest English pluraliser for the default table name.
pub(crate) fn pluralize(word: &str) -> String {
    let consonant_y = word.ends_with('y')
        && word
            .chars()
            .rev()
            .nth(1)
            .map(|c| !"aeiou".contains(c))
            .unwrap_or(false);
    if consonant_y {
        format!("{}ies", &word[..word.len() - 1])
    } else if word.ends_with('s')
        || word.ends_with('x')
        || word.ends_with('z')
        || word.ends_with("ch")
        || word.ends_with("sh")
    {
        format!("{word}es")
    } else {
        format!("{word}s")
    }
}
