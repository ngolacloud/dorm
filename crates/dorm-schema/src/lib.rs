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

pub use codegen::generate_rust;
pub use codegen_api::generate_axum;
pub use format::format;
pub use parser::{parse, ParseError};

/// A parsed `dorm.schema`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    pub datasource: Option<Datasource>,
    pub models: Vec<Model>,
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
    /// The table, defaulting to the snake_case plural of `name`.
    pub table: String,
    pub fields: Vec<Field>,
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
}

impl Scalar {
    /// The Rust type the column decodes as.
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
