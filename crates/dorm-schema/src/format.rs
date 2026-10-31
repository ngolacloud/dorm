//! Canonical pretty-printer for `dorm.schema` — the engine behind `dorm format`.
//!
//! This formatter is deliberately **lossless**: it works on the text, not the
//! [`crate::Schema`] AST. The AST is a lossy projection — it drops `//` comments,
//! forgets model-level attributes it does not model (`@@index`, an unknown
//! `@@…`), and normalises away spellings — so a parse→re-emit round-trip would
//! silently lose them. Formatting the *tokens and lines* instead keeps every byte
//! of meaning: comments, every field attribute, block-level attributes and the
//! `datasource` block all survive verbatim. Only layout is touched.
//!
//! What it normalises:
//!
//! * two-space indentation inside every block, no trailing whitespace;
//! * exactly one blank line between top-level blocks, none hugging a `{`/`}`,
//!   and runs of blank lines collapsed to one;
//! * field columns aligned — name, then type, then attributes — within a model,
//!   and the `=` aligned within a `datasource`/`generator` block;
//! * whitespace *between* tokens collapsed to a single space, while anything
//!   inside a string or an attribute's `(...)` is left exactly as written.
//!
//! The pass is idempotent by construction: formatting already-formatted text
//! re-tokenises to the same tokens and recomputes the same column widths, so
//! `format(format(x)) == format(x)`.

/// Format a `dorm.schema` source into its canonical form.
///
/// Pure: text in, text out, no I/O and no parsing through the AST — so nothing
/// is lost. The result always ends in a single trailing newline.
pub fn format(src: &str) -> String {
    let lines: Vec<Parsed> = src.lines().map(Parsed::parse).collect();
    let n = lines.len();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;

    while i < n {
        if lines[i].is_blank() {
            i += 1;
            continue;
        }

        // Gather a run of consecutive comment-only lines (a blank line breaks it).
        let mut comments: Vec<String> = Vec::new();
        while i < n && lines[i].is_comment_only() {
            comments.push(lines[i].comment.clone().unwrap());
            i += 1;
        }

        // Comments glued to a block header (no blank between) belong to it.
        let attaches = i < n && lines[i].is_block_header();

        if !comments.is_empty() && !attaches {
            // A standalone comment block, on its own.
            separate(&mut out);
            out.extend(comments);
            continue;
        }

        // Otherwise `comments` (possibly empty) lead into the next item.
        if i >= n {
            // Comments at end of file with nothing to attach to.
            separate(&mut out);
            out.extend(comments);
            break;
        }

        separate(&mut out);
        out.extend(comments);
        if lines[i].is_block_header() {
            i = format_block(&lines, i, &mut out);
        } else {
            // A stray top-level line (malformed input): keep it, collapsed.
            out.push(lines[i].render_code_with_comment(""));
            i += 1;
        }
    }

    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// Push a single blank-line separator before the next top-level item, unless the
/// output is still empty.
fn separate(out: &mut Vec<String>) {
    if !out.is_empty() {
        out.push(String::new());
    }
}

/// Format one block starting at the header line `lines[start]` (which ends in
/// `{`). Appends the header, the aligned body and the closing `}` to `out` and
/// returns the index just past the block.
fn format_block(lines: &[Parsed], start: usize, out: &mut Vec<String>) -> usize {
    let n = lines.len();

    // Header: `model Name {`, with exactly one space before the brace and any
    // trailing comment kept.
    let header_code = lines[start].code.trim_end_matches('{').trim_end();
    out.push(with_comment(
        format!("{header_code} {{"),
        &lines[start].comment,
    ));

    // Body runs until a line whose code is exactly `}` (there is no nesting in
    // the grammar), or end of file for malformed input.
    let body_start = start + 1;
    let mut i = body_start;
    while i < n && lines[i].code != "}" {
        i += 1;
    }
    let body = &lines[body_start..i];

    // Column widths over the body's field lines and key/value lines.
    let mut name_w = 0usize;
    let mut type_w = 0usize;
    let mut key_w = 0usize;
    for line in body {
        match line.classify() {
            BodyKind::Field { name, ty, .. } => {
                name_w = name_w.max(width(name));
                type_w = type_w.max(width(ty));
            }
            BodyKind::KeyValue { key, .. } => key_w = key_w.max(width(key)),
            _ => {}
        }
    }

    // Emit the body: two-space indent, interior blanks collapsed, leading and
    // trailing blanks dropped.
    let mut pending_blank = false;
    let mut emitted_any = false;
    for line in body {
        if line.is_blank() {
            pending_blank = emitted_any;
            continue;
        }
        if pending_blank {
            out.push(String::new());
            pending_blank = false;
        }
        emitted_any = true;
        out.push(format_body_line(line, name_w, type_w, key_w));
    }

    // Closing brace (keep any trailing comment on it).
    if i < n {
        out.push(lines[i].render_code_with_comment(""));
        i + 1
    } else {
        // Unterminated block: add the brace the input forgot, so output parses.
        out.push("}".to_string());
        i
    }
}

/// Render one body line, indented two spaces and aligned to the block's columns.
fn format_body_line(line: &Parsed, name_w: usize, type_w: usize, key_w: usize) -> String {
    const INDENT: &str = "  ";
    match line.classify() {
        BodyKind::CommentOnly => format!("{INDENT}{}", line.comment.clone().unwrap()),
        BodyKind::Field { name, ty, attrs } => {
            let mut body = format!("{name:<name_w$} {ty:<type_w$} {attrs}");
            body.truncate(body.trim_end().len());
            with_comment(format!("{INDENT}{body}"), &line.comment)
        }
        BodyKind::KeyValue { key, value } => {
            let body = format!("{key:<key_w$} = {value}");
            with_comment(format!("{INDENT}{body}"), &line.comment)
        }
        BodyKind::Other(code) => with_comment(format!("{INDENT}{code}"), &line.comment),
    }
}

/// Append a trailing comment after one space, if present.
fn with_comment(mut line: String, comment: &Option<String>) -> String {
    if let Some(c) = comment {
        line.push(' ');
        line.push_str(c);
    }
    line
}

/// The display width of a token, counted in characters.
fn width(s: &str) -> usize {
    s.chars().count()
}

/// A classified body line.
enum BodyKind<'a> {
    CommentOnly,
    /// `name Type[?|[]] @attr …` — a scalar or relation field.
    Field {
        name: &'a str,
        ty: &'a str,
        attrs: &'a str,
    },
    /// `key = value` — a `datasource`/`generator` setting.
    KeyValue {
        key: &'a str,
        value: &'a str,
    },
    /// A block-level attribute (`@@map(…)`) or anything else we keep verbatim.
    Other(String),
}

/// One source line, split into its code (comment stripped, whitespace collapsed
/// outside strings and parentheses, outer trim) and its trailing `//` comment.
struct Parsed {
    code: String,
    comment: Option<String>,
    /// The code with whitespace collapsed, cached for classification. Holds the
    /// same text as [`Parsed::code`]; kept so classify can borrow stable slices.
    collapsed: String,
}

impl Parsed {
    fn parse(line: &str) -> Parsed {
        let (code_raw, comment_raw) = split_code_comment(line);
        let code = collapse_ws_outside(code_raw);
        let comment = comment_raw.map(|c| c.trim_end().to_string());
        Parsed {
            collapsed: code.clone(),
            code,
            comment,
        }
    }

    fn is_blank(&self) -> bool {
        self.code.is_empty() && self.comment.is_none()
    }

    fn is_comment_only(&self) -> bool {
        self.code.is_empty() && self.comment.is_some()
    }

    fn is_block_header(&self) -> bool {
        self.code.ends_with('{')
    }

    /// Render the (already collapsed) code with its trailing comment. `prefix` is
    /// prepended before the code.
    fn render_code_with_comment(&self, prefix: &str) -> String {
        with_comment(format!("{prefix}{}", self.code), &self.comment)
    }

    fn classify(&self) -> BodyKind<'_> {
        if self.is_comment_only() {
            return BodyKind::CommentOnly;
        }
        let code = self.collapsed.as_str();
        if code.starts_with("@@") {
            return BodyKind::Other(code.to_string());
        }
        if let Some(eq) = top_level_eq(code) {
            let key = code[..eq].trim_end();
            let value = code[eq + 1..].trim_start();
            return BodyKind::KeyValue { key, value };
        }
        // A field line: first two whitespace tokens are the name and the type,
        // the rest (already single-spaced outside parens) is the attributes.
        let mut it = code.splitn(3, ' ');
        match (it.next(), it.next(), it.next()) {
            (Some(name), Some(ty), rest) => BodyKind::Field {
                name,
                ty,
                attrs: rest.unwrap_or(""),
            },
            // One bare token — not a field we can align; keep it verbatim.
            _ => BodyKind::Other(code.to_string()),
        }
    }
}

/// Split a line into its code part and its trailing `//` comment, if any. A `//`
/// inside a double-quoted string (as in a `postgres://…` URL) is not a comment.
fn split_code_comment(line: &str) -> (&str, Option<&str>) {
    let bytes = line.as_bytes();
    let mut in_str = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => in_str = !in_str,
            b'/' if !in_str && i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                return (&line[..i], Some(&line[i..]));
            }
            _ => {}
        }
        i += 1;
    }
    (line, None)
}

/// The byte index of the first `=` that is outside a string, or `None`.
fn top_level_eq(code: &str) -> Option<usize> {
    let mut in_str = false;
    for (i, c) in code.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '=' if !in_str => return Some(i),
            _ => {}
        }
    }
    None
}

/// Collapse runs of whitespace to a single space, but only *outside* strings and
/// outside `(...)`/`[...]` groups, where spacing is content the user owns (an
/// `@relation(onDelete: Cascade)` or a string literal). Trims the ends.
fn collapse_ws_outside(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_str = false;
    let mut depth = 0i32;
    let mut prev_space = false;
    for c in s.chars() {
        if in_str {
            out.push(c);
            if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                prev_space = false;
                out.push(c);
            }
            '(' | '[' => {
                depth += 1;
                prev_space = false;
                out.push(c);
            }
            ')' | ']' => {
                depth -= 1;
                prev_space = false;
                out.push(c);
            }
            c if c.is_whitespace() => {
                if depth > 0 {
                    out.push(c);
                } else if !prev_space {
                    out.push(' ');
                    prev_space = true;
                }
            }
            c => {
                prev_space = false;
                out.push(c);
            }
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::format;

    const SAMPLE: &str = r#"datasource db {
  provider = "postgresql"
  url      = env("DELONIX_ORM_DSN")
}

model Tenant {
  id        Int      @id @default(autoincrement())
  slug      String   @unique @index @db.VarChar(63)
  name      String   @db.VarChar(200)
  createdAt DateTime @default(now())
  posts     Post[]
}

model Post {
  id        Int      @id @default(autoincrement())
  tenant    Tenant   @relation(onDelete: Cascade)
  title     String   @index @db.VarChar(200)
  createdAt DateTime @default(now())
}
"#;

    #[test]
    fn canonical_sample_is_a_fixed_point() {
        // The scaffold schema is already canonical: formatting is a no-op.
        assert_eq!(format(SAMPLE), SAMPLE);
    }

    #[test]
    fn idempotent_on_messy_input() {
        let messy = "datasource   db{\nprovider=\"postgresql\"\n   url=env(\"X\")\n}\n\n\n\nmodel   Tenant{\nid Int @id    @default(autoincrement())\nslug    String @unique\n}\n";
        let once = format(messy);
        let twice = format(&once);
        assert_eq!(once, twice, "format must be idempotent");
    }

    #[test]
    fn comments_survive_everywhere() {
        let input = "// a leading file comment\ndatasource db {\n  provider = \"postgresql\"  // the backend\n  url      = env(\"DELONIX_ORM_DSN\")\n}\n\nmodel Tenant {\n  // a field comment\n  id   Int    @id @default(autoincrement())\n  name String // a trailing comment\n}\n";
        let out = format(input);
        assert!(
            out.contains("// a leading file comment"),
            "leading comment lost: {out}"
        );
        assert!(
            out.contains("// the backend"),
            "datasource trailing comment lost: {out}"
        );
        assert!(
            out.contains("// a field comment"),
            "field comment lost: {out}"
        );
        assert!(
            out.contains("// a trailing comment"),
            "field trailing comment lost: {out}"
        );
        // And still a fixed point.
        assert_eq!(format(&out), out);
    }

    #[test]
    fn block_level_and_unknown_attributes_survive() {
        // `@@index` and `@db.Text` are not fully modelled by the AST; a textual
        // formatter must keep them verbatim.
        let input = "model Post {\n  id    Int    @id @default(autoincrement())\n  body  String @db.Text\n  @@index([body])\n  @@map(\"posts\")\n}\n";
        let out = format(input);
        assert!(out.contains("@@index([body])"), "block attr lost: {out}");
        assert!(out.contains("@@map(\"posts\")"), "@@map lost: {out}");
        assert!(out.contains("@db.Text"), "@db.Text lost: {out}");
        assert_eq!(format(&out), out);
    }

    #[test]
    fn url_double_slash_is_not_a_comment() {
        let input = "datasource db {\n  provider = \"postgresql\"\n  url      = \"postgres://u:p@h/db?sslmode=require\"\n}\n";
        let out = format(input);
        assert!(
            out.contains("postgres://u:p@h/db?sslmode=require"),
            "URL mangled as comment: {out}"
        );
        assert_eq!(format(&out), out);
    }

    #[test]
    fn realigns_and_collapses_blank_runs() {
        let input = "model T {\n  id Int @id\n\n\n\n  description String @db.VarChar(500)\n}\n";
        let expected =
            "model T {\n  id          Int    @id\n\n  description String @db.VarChar(500)\n}\n";
        assert_eq!(format(input), expected);
    }

    #[test]
    fn inserts_blank_between_adjacent_blocks() {
        let input = "model A {\n  id Int @id\n}\nmodel B {\n  id Int @id\n}\n";
        let out = format(input);
        assert_eq!(
            out,
            "model A {\n  id Int @id\n}\n\nmodel B {\n  id Int @id\n}\n"
        );
    }
}
