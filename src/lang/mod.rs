//! Per-language extractors; each turns source text into shared records.

pub mod csharp;
pub mod typescript;

use std::cmp::Reverse;

use rayon::prelude::*;
use tree_sitter::Parser;

use crate::record::{Field, Language, Record};
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
/// as some Windows tooling writes.
fn decode(bytes: &[u8]) -> String {
    let utf16 = |pairs: std::slice::ChunksExact<u8>, le: bool| {
        let units: Vec<u16> = pairs
            .map(|p| {
                if le {
                    u16::from_le_bytes([p[0], p[1]])
                } else {
                    u16::from_be_bytes([p[0], p[1]])
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    };
    match bytes {
        [0xFF, 0xFE, rest @ ..] => utf16(rest.chunks_exact(2), true),
        [0xFE, 0xFF, rest @ ..] => utf16(rest.chunks_exact(2), false),
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
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
        assert_eq!(decode(&[0xEF, 0xBB, 0xBF, b'x']), "x");
        assert_eq!(decode(&[0xFF, 0xFE, b'h', 0, b'i', 0]), "hi");
        assert_eq!(decode(&[0xFE, 0xFF, 0, b'h', 0, b'i']), "hi");
    }
}
