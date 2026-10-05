//! Source discovery: gitignore-aware walk, test detection, root-relative excludes.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;

use crate::record::Language;

pub struct WalkOptions {
    pub include_tests: bool,
    /// Globs matched against root-relative `/` paths, e.g. `examples/**`.
    pub excludes: Vec<String>,
}

pub struct SourceFile {
    pub path: PathBuf,
    /// Root-relative, `/`-separated.
    pub rel: String,
    pub language: Language,
}

/// Directories never worth parsing, even in repos without a `.gitignore`.
const SKIPPED_DIRS: &[&str] = &["node_modules", "dist", "build", "out", "coverage", "target"];

pub fn discover(root: &Path, options: &WalkOptions) -> Result<Vec<SourceFile>> {
    let excludes = build_globs(&options.excludes)?;
    let mut files = Vec::new();
    let walker = WalkBuilder::new(root)
        .filter_entry(|entry| !SKIPPED_DIRS.iter().any(|dir| entry.file_name() == *dir))
        .build();
    for entry in walker {
        let entry = entry?;
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.into_path();
        let Some(language) = language_for(&path) else {
            continue;
        };
        let rel = relative(root, &path);
        if excludes.is_match(&rel) || (!options.include_tests && is_test_path(&rel)) {
            continue;
        }
        files.push(SourceFile {
            path,
            rel,
            language,
        });
    }
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(files)
}

pub fn language_for(path: &Path) -> Option<Language> {
    match path.extension()?.to_str()? {
        "ts" | "tsx" | "mts" | "cts" => Some(Language::TypeScript),
        "js" | "jsx" | "mjs" | "cjs" => Some(Language::JavaScript),
        _ => None,
    }
}

/// Test, mock and fixture paths, e.g. `src/a.test.ts`, `src/__tests__/a.ts`.
pub fn is_test_path(rel: &str) -> bool {
    const DIRS: &[&str] = &[
        "test",
        "tests",
        "__tests__",
        "__mocks__",
        "fixtures",
        "__fixtures__",
        "e2e",
    ];
    let mut segments = rel.split('/').peekable();
    while let Some(segment) = segments.next() {
        if segments.peek().is_none() {
            return segment.contains(".test.") || segment.contains(".spec.");
        }
        if DIRS.contains(&segment) {
            return true;
        }
    }
    false
}

fn relative(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn build_globs(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(
            Glob::new(pattern).with_context(|| format!("invalid --exclude glob `{pattern}`"))?,
        );
    }
    Ok(builder.build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_test_paths() {
        assert!(is_test_path("src/a.test.ts"));
        assert!(is_test_path("src/a.spec.tsx"));
        assert!(is_test_path("src/__tests__/a.ts"));
        assert!(is_test_path("packages/x/test/helpers.ts"));
        assert!(!is_test_path("src/testing.ts"));
        assert!(!is_test_path("src/latest/a.ts"));
    }

    #[test]
    fn maps_extensions() {
        assert_eq!(language_for(Path::new("a.tsx")), Some(Language::TypeScript));
        assert_eq!(language_for(Path::new("a.mjs")), Some(Language::JavaScript));
        assert_eq!(language_for(Path::new("a.rs")), None);
    }
}
