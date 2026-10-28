//! A tiny tokenizer for `dorm.schema`. Whitespace and `//` comments are skipped;
//! everything else becomes one of a handful of tokens.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tok {
    /// An identifier, or an attribute name like `db.VarChar` (dots kept).
    Ident(String),
    Str(String),
    Int(i64),
    /// One of `{ } ( ) [ ] = : , ? @`.
    Punct(char),
    /// `@@` — a block-level attribute.
    AtAt,
}

/// Tokenize, or report the 1-based line where lexing failed.
pub fn lex(src: &str) -> Result<Vec<(Tok, usize)>, String> {
    let mut out = Vec::new();
    let bytes = src.as_bytes();
    let mut i = 0;
    let mut line = 1usize;
    while i < bytes.len() {
        let c = bytes[i] as char;
        match c {
            '\n' => {
                line += 1;
                i += 1;
            }
            c if c.is_whitespace() => i += 1,
            '/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            '@' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'@' {
                    out.push((Tok::AtAt, line));
                    i += 2;
                } else {
                    out.push((Tok::Punct('@'), line));
                    i += 1;
                }
            }
            '{' | '}' | '(' | ')' | '[' | ']' | '=' | ':' | ',' | '?' => {
                out.push((Tok::Punct(c), line));
                i += 1;
            }
            '"' => {
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\n' {
                        return Err(format!("line {line}: unterminated string"));
                    }
                    i += 1;
                }
                if i >= bytes.len() {
                    return Err(format!("line {line}: unterminated string"));
                }
                out.push((Tok::Str(src[start..i].to_string()), line));
                i += 1; // closing quote
            }
            c if c.is_ascii_digit() => {
                let start = i;
                while i < bytes.len() && (bytes[i] as char).is_ascii_digit() {
                    i += 1;
                }
                let n: i64 = src[start..i]
                    .parse()
                    .map_err(|_| format!("line {line}: bad number"))?;
                out.push((Tok::Int(n), line));
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                // An identifier, extended to allow `.` so `db.VarChar` is one
                // token — convenient for attribute names.
                while i < bytes.len() {
                    let b = bytes[i] as char;
                    if b.is_ascii_alphanumeric() || b == '_' || b == '.' {
                        i += 1;
                    } else {
                        break;
                    }
                }
                out.push((Tok::Ident(src[start..i].to_string()), line));
            }
            _ => return Err(format!("line {line}: unexpected character `{c}`")),
        }
    }
    Ok(out)
}
