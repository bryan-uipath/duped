//! Per-language extractors; each turns source text into shared records.

pub mod typescript;

use std::cmp::Reverse;

use rayon::prelude::*;
use tree_sitter::Parser;

use crate::record::{Language, Record};
use crate::walk::SourceFile;

pub struct Extraction {
    pub records: Vec<Record>,
    /// Files that could not be read, e.g. non-UTF-8 content.
    pub skipped: usize,
}

/// Parse all files in parallel; records come back ordered by file, then line.
pub fn extract_files(files: &[SourceFile]) -> Extraction {
    let results: Vec<Option<Vec<Record>>> = files
        .par_iter()
        .map_init(Parser::new, |parser, file| {
            let source = std::fs::read_to_string(&file.path).ok()?;
            Some(match file.language {
                Language::TypeScript | Language::JavaScript => {
                    typescript::extract(parser, &source, &file.rel, file.language)
                }
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
