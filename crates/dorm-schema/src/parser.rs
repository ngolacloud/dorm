//! Recursive-descent parser: tokens → [`Schema`].

use crate::lexer::{lex, Tok};
use crate::{
    snake, Auto, Datasource, Field, Model, OnDelete, RagBlock, Relation, Scalar, ScalarField,
    Schema, Url,
};

/// A parse failure, with the 1-based line when known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ParseError {}

/// Parse a `dorm.schema` source into a [`Schema`].
pub fn parse(src: &str) -> Result<Schema, ParseError> {
    let toks = lex(src).map_err(ParseError)?;
    Parser { toks, pos: 0 }.schema()
}

struct Parser {
    toks: Vec<(Tok, usize)>,
    pos: usize,
}

type R<T> = Result<T, ParseError>;

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|(t, _)| t)
    }

    fn line(&self) -> usize {
        self.toks
            .get(self.pos)
            .or_else(|| self.toks.last())
            .map(|(_, l)| *l)
            .unwrap_or(0)
    }

    fn err<T>(&self, msg: impl Into<String>) -> R<T> {
        Err(ParseError(format!("line {}: {}", self.line(), msg.into())))
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).map(|(t, _)| t.clone());
        self.pos += 1;
        t
    }

    fn ident(&mut self) -> R<String> {
        match self.next() {
            Some(Tok::Ident(s)) => Ok(s),
            _ => {
                self.pos -= 1;
                self.err("expected an identifier")
            }
        }
    }

    fn punct(&mut self, c: char) -> R<()> {
        match self.next() {
            Some(Tok::Punct(p)) if p == c => Ok(()),
            _ => {
                self.pos -= 1;
                self.err(format!("expected `{c}`"))
            }
        }
    }

    fn eat_punct(&mut self, c: char) -> bool {
        if matches!(self.peek(), Some(Tok::Punct(p)) if *p == c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn schema(&mut self) -> R<Schema> {
        let mut datasource = None;
        let mut models = Vec::new();
        while let Some(tok) = self.peek() {
            match tok {
                Tok::Ident(kw) if kw == "datasource" => {
                    self.pos += 1;
                    datasource = Some(self.datasource()?);
                }
                Tok::Ident(kw) if kw == "model" => {
                    self.pos += 1;
                    models.push(self.model()?);
                }
                Tok::Ident(kw) if kw == "generator" => {
                    // Accepted and ignored: DORM generates via `dorm generate`,
                    // not a `generator` block, but tolerating it eases paste-in
                    // from a Prisma schema.
                    self.pos += 1;
                    let _ = self.ident();
                    self.skip_block()?;
                }
                _ => return self.err("expected `datasource` or `model`"),
            }
        }
        Ok(Schema { datasource, models })
    }

    fn skip_block(&mut self) -> R<()> {
        self.punct('{')?;
        let mut depth = 1;
        while depth > 0 {
            match self.next() {
                Some(Tok::Punct('{')) => depth += 1,
                Some(Tok::Punct('}')) => depth -= 1,
                None => return self.err("unterminated block"),
                _ => {}
            }
        }
        Ok(())
    }

    fn datasource(&mut self) -> R<Datasource> {
        let name = self.ident()?;
        self.punct('{')?;
        let mut provider = String::new();
        let mut url = Url::Literal(String::new());
        while !self.eat_punct('}') {
            let key = self.ident()?;
            self.punct('=')?;
            match key.as_str() {
                "provider" => provider = self.string()?,
                "url" => url = self.url_value()?,
                _ => {
                    // Unknown key: consume its value and move on.
                    self.value_any();
                }
            }
        }
        Ok(Datasource {
            name,
            provider,
            url,
        })
    }

    fn string(&mut self) -> R<String> {
        match self.next() {
            Some(Tok::Str(s)) => Ok(s),
            _ => {
                self.pos -= 1;
                self.err("expected a string")
            }
        }
    }

    fn url_value(&mut self) -> R<Url> {
        match self.peek() {
            Some(Tok::Str(_)) => Ok(Url::Literal(self.string()?)),
            Some(Tok::Ident(kw)) if kw == "env" => {
                self.pos += 1;
                self.punct('(')?;
                let v = self.string()?;
                self.punct(')')?;
                Ok(Url::Env(v))
            }
            _ => self.err("expected a string or env(\"VAR\")"),
        }
    }

    fn value_any(&mut self) {
        // Best-effort skip of one value token (string, int, or ident(...)).
        match self.next() {
            Some(Tok::Ident(_)) if matches!(self.peek(), Some(Tok::Punct('('))) => {
                let _ = self.punct('(');
                let mut depth = 1;
                while depth > 0 {
                    match self.next() {
                        Some(Tok::Punct('(')) => depth += 1,
                        Some(Tok::Punct(')')) => depth -= 1,
                        None => break,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn model(&mut self) -> R<Model> {
        let name = self.ident()?;
        let mut table = pluralized_table(&name);
        let mut rag = None;
        let mut unique_together: Vec<Vec<String>> = Vec::new();
        let mut composite_id: Vec<String> = Vec::new();
        self.punct('{')?;
        let mut fields = Vec::new();
        loop {
            match self.peek() {
                Some(Tok::Punct('}')) => {
                    self.pos += 1;
                    break;
                }
                Some(Tok::AtAt) => {
                    self.pos += 1;
                    match self.block_attr()? {
                        BlockAttr::Map(t) => table = t,
                        BlockAttr::Rag(r) => rag = Some(r),
                        BlockAttr::Unique(cols) => unique_together.push(cols),
                        BlockAttr::Id(cols) => composite_id = cols,
                        BlockAttr::Ignored => {}
                    }
                }
                Some(_) => fields.push(self.field()?),
                None => return self.err("unterminated model"),
            }
        }
        Ok(Model {
            name,
            table,
            fields,
            rag,
            unique_together,
            composite_id,
        })
    }

    /// A block-level `@@…` attribute: `@@map` (the table), `@@rag` (ADR 0019), or
    /// anything else (kept only to stay valid — the formatter preserves it from
    /// the text, not the AST).
    fn block_attr(&mut self) -> R<BlockAttr> {
        let name = self.ident()?;
        if name == "rag" {
            return Ok(BlockAttr::Rag(self.rag_block()?));
        }
        // `@@unique([a, b])` and `@@id([a, b])` carry a column list that the AST
        // now models (ADR 0043); `@@index([…])` and anything else are still parsed
        // and dropped (a composite index has no home in `ModelMeta` yet).
        if (name == "unique" || name == "id") && self.peek() == Some(&Tok::Punct('(')) {
            self.punct('(')?;
            let cols = self.ident_list()?;
            self.punct(')')?;
            return Ok(if name == "unique" {
                BlockAttr::Unique(cols)
            } else {
                BlockAttr::Id(cols)
            });
        }
        let mut result = BlockAttr::Ignored;
        if self.eat_punct('(') {
            if name == "map" {
                result = BlockAttr::Map(self.string()?);
            }
            // Consume the rest of the args (e.g. `@@index([a, b])`).
            let mut depth = 1;
            while depth > 0 {
                match self.next() {
                    Some(Tok::Punct('(')) => depth += 1,
                    Some(Tok::Punct(')')) => depth -= 1,
                    None => return self.err("unterminated attribute"),
                    _ => {}
                }
            }
        }
        Ok(result)
    }

    /// A bracketed list of identifiers: `[a, b, c]` — the shape `@@unique`/`@@id`
    /// take.
    fn ident_list(&mut self) -> R<Vec<String>> {
        self.punct('[')?;
        let mut out = Vec::new();
        loop {
            if self.eat_punct(']') {
                break;
            }
            out.push(self.ident()?);
            if !self.eat_punct(',') {
                self.punct(']')?;
                break;
            }
        }
        Ok(out)
    }

    /// Parse the args of `@@rag(fields: [title, body], embedder: "default", …)`.
    /// Consumes its own enclosing `( … )`.
    fn rag_block(&mut self) -> R<RagBlock> {
        self.punct('(')?;
        let mut rb = RagBlock::default();
        loop {
            if self.eat_punct(')') {
                break;
            }
            let key = self.ident()?;
            self.punct(':')?;
            match key.as_str() {
                "fields" => rb.fields = self.ident_or_str_list()?,
                "metadata" => rb.metadata = self.ident_or_str_list()?,
                "template" => rb.template = Some(self.string()?),
                "embedder" => rb.embedder = Some(self.string()?),
                "distance" => rb.distance = Some(self.string()?),
                "chunk_size" => rb.chunk_size = Some(self.int()? as u32),
                "chunk_overlap" => rb.chunk_overlap = Some(self.int()? as u32),
                other => {
                    return self.err(format!(
                        "unknown @@rag option `{other}`; known: fields, template, embedder, \
                         chunk_size, chunk_overlap, metadata, distance"
                    ))
                }
            }
            // A comma between entries is optional, like Prisma's attribute args.
            self.eat_punct(',');
        }
        Ok(rb)
    }

    /// A `[a, b]` list whose elements are field names or strings — the shape
    /// `fields`/`metadata` take.
    fn ident_or_str_list(&mut self) -> R<Vec<String>> {
        self.punct('[')?;
        let mut out = Vec::new();
        loop {
            if self.eat_punct(']') {
                break;
            }
            match self.next() {
                Some(Tok::Ident(s)) | Some(Tok::Str(s)) => out.push(s),
                _ => {
                    self.pos -= 1;
                    return self.err("expected a field name or a string in the list");
                }
            }
            self.eat_punct(',');
        }
        Ok(out)
    }

    fn field(&mut self) -> R<Field> {
        let fname = self.ident()?;
        let type_name = self.ident()?;
        let mut nullable = false;
        let mut is_list = false;
        if self.eat_punct('?') {
            nullable = true;
        } else if self.eat_punct('[') {
            self.punct(']')?;
            is_list = true;
        }

        let attrs = self.field_attrs()?;

        if let Some(scalar) = Scalar::parse(&type_name) {
            let column = attrs
                .iter()
                .find_map(|a| match a {
                    FieldAttr::Map(c) => Some(c.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| snake(&fname));
            let primary_key = attrs.iter().any(|a| matches!(a, FieldAttr::Id));
            let auto_increment = attrs.iter().any(|a| matches!(a, FieldAttr::DefaultAutoinc));
            let auto = attrs.iter().find_map(|a| match a {
                FieldAttr::DefaultNow => Some(Auto::NowAdd),
                FieldAttr::UpdatedAt => Some(Auto::Now),
                _ => None,
            });
            let default_sql = attrs.iter().find_map(|a| match a {
                FieldAttr::DefaultLiteral(s) => Some(s.clone()),
                _ => None,
            });
            return Ok(Field::Scalar(ScalarField {
                name: fname,
                column,
                ty: scalar,
                nullable,
                primary_key,
                auto_increment,
                unique: attrs.iter().any(|a| matches!(a, FieldAttr::Unique)),
                index: attrs.iter().any(|a| matches!(a, FieldAttr::Index)),
                max_length: attrs.iter().find_map(|a| match a {
                    FieldAttr::MaxLength(n) => Some(*n),
                    _ => None,
                }),
                auto,
                default_sql,
            }));
        }

        // A model-typed field: a relation.
        if is_list {
            return Ok(Field::ToMany {
                name: fname,
                target: type_name,
            });
        }
        let on_delete = attrs
            .iter()
            .find_map(|a| match a {
                FieldAttr::Relation(od) => Some(*od),
                _ => None,
            })
            .unwrap_or(OnDelete::Cascade);
        let column = attrs
            .iter()
            .find_map(|a| match a {
                FieldAttr::Map(c) => Some(c.clone()),
                _ => None,
            })
            .unwrap_or_else(|| snake(&fname));
        Ok(Field::ToOne(Relation {
            name: fname,
            column,
            target: type_name,
            nullable,
            on_delete,
        }))
    }

    fn field_attrs(&mut self) -> R<Vec<FieldAttr>> {
        let mut attrs = Vec::new();
        while self.eat_punct('@') {
            attrs.push(self.one_attr()?);
        }
        Ok(attrs)
    }

    fn one_attr(&mut self) -> R<FieldAttr> {
        let name = self.ident()?;
        match name.as_str() {
            "id" => Ok(FieldAttr::Id),
            "unique" => Ok(FieldAttr::Unique),
            "index" => Ok(FieldAttr::Index),
            "updatedAt" => Ok(FieldAttr::UpdatedAt),
            "db.Text" => Ok(FieldAttr::Ignored),
            "db.VarChar" => {
                self.punct('(')?;
                let n = self.int()?;
                self.punct(')')?;
                Ok(FieldAttr::MaxLength(n as u32))
            }
            "map" => {
                self.punct('(')?;
                let c = self.string()?;
                self.punct(')')?;
                Ok(FieldAttr::Map(c))
            }
            "default" => self.default_attr(),
            "relation" => self.relation_attr(),
            other => self.err(format!("unknown attribute `@{other}`")),
        }
    }

    fn default_attr(&mut self) -> R<FieldAttr> {
        self.punct('(')?;
        let attr = match self.next() {
            Some(Tok::Ident(f)) if f == "autoincrement" => {
                self.punct('(')?;
                self.punct(')')?;
                FieldAttr::DefaultAutoinc
            }
            Some(Tok::Ident(f)) if f == "now" => {
                self.punct('(')?;
                self.punct(')')?;
                FieldAttr::DefaultNow
            }
            Some(Tok::Ident(b)) if b == "true" || b == "false" => FieldAttr::DefaultLiteral(b),
            Some(Tok::Str(s)) => FieldAttr::DefaultLiteral(format!("'{}'", s.replace('\'', "''"))),
            Some(Tok::Int(n)) => FieldAttr::DefaultLiteral(n.to_string()),
            _ => {
                self.pos -= 1;
                return self.err("unsupported @default(...)");
            }
        };
        self.punct(')')?;
        Ok(attr)
    }

    fn relation_attr(&mut self) -> R<FieldAttr> {
        self.punct('(')?;
        let mut on_delete = OnDelete::Cascade;
        // Parse `key: value` / positional args until ')', keeping onDelete.
        loop {
            match self.next() {
                Some(Tok::Punct(')')) => break,
                Some(Tok::Ident(k)) if k == "onDelete" => {
                    self.punct(':')?;
                    let action = self.ident()?;
                    on_delete = OnDelete::parse(&action)
                        .ok_or_else(|| ParseError(format!("unknown onDelete `{action}`")))?;
                }
                Some(Tok::Punct('[')) => {
                    // skip a list like fields: [tenantId]
                    while !matches!(self.next(), Some(Tok::Punct(']')) | None) {}
                }
                None => return self.err("unterminated @relation"),
                _ => {}
            }
        }
        Ok(FieldAttr::Relation(on_delete))
    }

    fn int(&mut self) -> R<i64> {
        match self.next() {
            Some(Tok::Int(n)) => Ok(n),
            _ => {
                self.pos -= 1;
                self.err("expected a number")
            }
        }
    }
}

enum FieldAttr {
    Id,
    Unique,
    Index,
    UpdatedAt,
    DefaultAutoinc,
    DefaultNow,
    DefaultLiteral(String),
    MaxLength(u32),
    Map(String),
    Relation(OnDelete),
    Ignored,
}

/// A block-level `@@…` attribute's parsed meaning.
enum BlockAttr {
    /// `@@map("table")` — the table name.
    Map(String),
    /// `@@rag(..)` — the RAG declaration (ADR 0019).
    Rag(RagBlock),
    /// `@@unique([a, b])` — a composite-unique group (ADR 0043).
    Unique(Vec<String>),
    /// `@@id([a, b])` — a composite primary key (ADR 0043).
    Id(Vec<String>),
    /// Anything else (`@@index`, …): valid, but not modelled by the AST.
    Ignored,
}

fn pluralized_table(model: &str) -> String {
    crate::pluralize(&snake(model))
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn a_rag_block_parses_into_every_field() {
        let src = r#"
            model Ticket {
              id    Int    @id @default(autoincrement())
              title String
              body  String
              @@rag(fields: [title, body], template: "{title}", embedder: "e1", chunk_size: 500, chunk_overlap: 50, metadata: [status], distance: "l2")
            }
        "#;
        let schema = parse(src).expect("parses");
        let rag = schema.models[0].rag.as_ref().expect("has rag");
        assert_eq!(rag.fields, vec!["title", "body"]);
        assert_eq!(rag.template.as_deref(), Some("{title}"));
        assert_eq!(rag.embedder.as_deref(), Some("e1"));
        assert_eq!(rag.chunk_size, Some(500));
        assert_eq!(rag.chunk_overlap, Some(50));
        assert_eq!(rag.metadata, vec!["status"]);
        assert_eq!(rag.distance.as_deref(), Some("l2"));
    }

    #[test]
    fn a_model_without_rag_has_none_and_an_unknown_rag_key_is_an_error() {
        let plain = parse("model T {\n  id Int @id\n}\n").expect("parses");
        assert!(plain.models[0].rag.is_none());

        let bad = parse("model T {\n  id Int @id\n  @@rag(fyelds: [id])\n}\n");
        assert!(bad.is_err());
        assert!(bad.unwrap_err().0.contains("unknown @@rag option"));
    }

    #[test]
    fn an_unknown_double_at_attribute_is_still_ignored() {
        // `@@index` is not modelled; it must parse (and be dropped) as before.
        let schema = parse("model T {\n  id Int @id\n  name String\n  @@index([name])\n}\n")
            .expect("parses");
        assert!(schema.models[0].rag.is_none());
        assert_eq!(schema.models[0].fields.len(), 2);
    }
}
