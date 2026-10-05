//! Per-language extractors; each turns source text into shared records.

pub mod typescript;

use rayon::prelude::*;
use tree_sitter::Parser;

use crate::record::{Language, Record};
use crate::walk::SourceFile;

pub struct Extraction {
    pub records: Vec<Record>,
    /// Files that could not be read, e.g. non-UTF-8 content.
    pub skipped: Vec<String>,
}

/// Parse all files in parallel; records come back ordered by file, then line.
pub fn extract_files(files: &[SourceFile]) -> Extraction {
    let results: Vec<Result<Vec<Record>, String>> = files
        .par_iter()
        .map_init(Parser::new, |parser, file| {
            let source = std::fs::read_to_string(&file.path).map_err(|_| file.rel.clone())?;
            Ok(match file.language {
                Language::TypeScript | Language::JavaScript => {
                    typescript::extract(parser, &source, &file.rel, file.language)
                }
            })
        })
        .collect();

    let mut records = Vec::new();
    let mut skipped = Vec::new();
    for result in results {
        match result {
            Ok(mut file_records) => {
                file_records.sort_by_key(|r| r.location().start_line);
                records.append(&mut file_records);
            }
            Err(rel) => skipped.push(rel),
        }
    }
    Extraction { records, skipped }
}
