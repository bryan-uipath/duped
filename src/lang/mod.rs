//! Per-language extractors; each turns source text into shared records.

pub mod csharp;
pub mod python;
pub mod rust;
pub mod typescript;

use std::cmp::Reverse;

use rayon::prelude::*;
use tree_sitter::{Node, Parser};

use crate::record::{Field, Language, Param, Record, Token, format_params, token_hash};
use crate::walk::SourceFile;

pub struct Extraction {
    pub records: Vec<Record>,
    /// Files that could not be read.
    pub skipped: usize,
}

/// Parse all files in parallel; records come back ordered by file, then line.
pub fn extract_files(files: &[SourceFile]) -> Extraction {
    let results: Vec<Option<Vec<Record>>> = files
        .par_iter()
        .map_init(Parser::new, |parser, file| {
            let source = decode(&std::fs::read(&file.path).ok()?);
            Some(match file.language {
                Language::TypeScript | Language::JavaScript => {
                    typescript::extract(parser, &source, &file.rel, file.language)
                }
                Language::CSharp => csharp::extract(parser, &source, &file.rel),
                Language::Python => python::extract(parser, &source, &file.rel),
                Language::Rust => rust::extract(parser, &source, &file.rel),
            })
        })
        .collect();

    let mut records = Vec::new();
    let mut skipped = 0;
    for result in results {
        match result {
            Some(mut file_records) => {
                // Enclosing declarations first when two start on the same line.
                file_records
                    .sort_by_key(|r| (r.location().start_line, Reverse(r.location().end_line)));
                records.append(&mut file_records);
            }
            None => skipped += 1,
        }
    }
    Extraction { records, skipped }
}

/// UTF-8 (lossy, so legacy single-byte files still parse), or UTF-16 with a byte-order mark,
/// as some Windows tooling writes. A UTF-8 byte-order mark is left for the parser to skip.
fn decode(bytes: &[u8]) -> String {
    let utf16 = |rest: &[u8], from: fn([u8; 2]) -> u16| {
        let units: Vec<u16> = rest.as_chunks::<2>().0.iter().map(|p| from(*p)).collect();
        String::from_utf16_lossy(&units)
    };
    match bytes {
        [0xFF, 0xFE, rest @ ..] => utf16(rest, u16::from_le_bytes),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, u16::from_be_bytes),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// How a language's tokenizer treats a node kind; unclassed leaves are kept as text.
pub enum TokenClass {
    /// Comments; dropped with their subtree.
    Skip,
    /// One token for the whole subtree, e.g. a string with its quotes and fragments.
    Literal,
    Identifier,
}

/// Leaves of `node` in source order as body tokens, classed by `class`; whitespace-only
/// leaves (such as JSX text between tags) are dropped.
pub fn body_tokens(node: Node, source: &str, class: fn(&str) -> Option<TokenClass>) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut cursor = node.walk();
    'walk: loop {
        let node = cursor.node();
        let text = &source[node.byte_range()];
        let class = class(node.kind());
        let leaf = class.is_some() || node.child_count() == 0;
        if leaf && !text.trim().is_empty() {
            tokens.extend(match class {
                Some(TokenClass::Skip) => None,
                Some(TokenClass::Identifier) => Some(Token::Identifier),
                Some(TokenClass::Literal) | None => Some(Token::Text(token_hash(text))),
            });
        }
        if !leaf && cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                break 'walk;
            }
        }
    }
    tokens
}

pub fn has_token(node: Node, token: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|c| !c.is_named() && c.kind() == token)
}

pub fn join_scope(scope: &[String]) -> Option<String> {
    (!scope.is_empty()).then(|| scope.join("."))
}

/// Method field type, e.g. `(id: string, force?: boolean) => void`.
pub fn signature(params: &[Param], returns: Option<&str>) -> String {
    format!(
        "({}) => {}",
        format_params(params),
        returns.unwrap_or("unknown")
    )
}

/// Add `field` unless its name is already present; an untyped entry takes the new type.
/// Overloads and getter/setter pairs share a name.
pub fn push_field(fields: &mut Vec<Field>, field: Option<Field>) {
    let Some(field) = field else { return };
    match fields.iter_mut().find(|f| f.name == field.name) {
        Some(existing) => {
            if existing.ty.is_none() {
                existing.ty = field.ty;
            }
        }
        None => fields.push(field),
    }
}

/// `{ a:\n  'x  y' }` → `{ a: 'x  y' }`: collapse whitespace runs outside quotes.
pub fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut quote = None;
    let mut escaped = false;
    let mut pending_space = false;
    for c in text.chars() {
        if let Some(q) = quote {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        if c.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        if matches!(c, '\'' | '"' | '`') {
            quote = Some(c);
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_legacy_and_utf16_sources() {
        assert_eq!(
            decode(b"class A {} // caf\xe9"),
            "class A {} // caf\u{FFFD}"
        );
        assert_eq!(decode(&[0xFF, 0xFE, b'h', 0, b'i', 0]), "hi");
        assert_eq!(decode(&[0xFE, 0xFF, 0, b'h', 0, b'i']), "hi");
    }
}
