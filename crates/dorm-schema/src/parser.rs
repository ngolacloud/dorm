//! Recursive-descent parser: tokens → [`Schema`].

use crate::lexer::{lex, Tok};
use crate::{
    snake, Auto, Datasource, Field, Model, OnDelete, Relation, Scalar, ScalarField, Schema, Url,
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
                    if let Some(t) = self.block_attr()? {
                        table = t;
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
        })
    }

    /// A block-level `@@…` attribute. Returns `Some(table)` for `@@map`.
    fn block_attr(&mut self) -> R<Option<String>> {
        let name = self.ident()?;
        let mut table = None;
        if self.eat_punct('(') {
            if name == "map" {
                table = Some(self.string()?);
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
        Ok(table)
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

fn pluralized_table(model: &str) -> String {
    crate::pluralize(&snake(model))
}
