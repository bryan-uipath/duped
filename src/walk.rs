//! Source discovery: gitignore-aware walk, test detection, root-relative excludes.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;

use crate::record::Language;

pub struct WalkOptions {
    pub include_tests: bool,
    /// Globs matched against root-relative `/` paths and their parent directories,
    /// e.g. `examples` or `src/gen/*.ts`.
    pub excludes: Vec<String>,
}

pub struct SourceFile {
    pub path: PathBuf,
    /// Root-relative, `/`-separated; the file name when the root is a file.
    pub rel: String,
    pub language: Language,
}

/// Directories never worth parsing, even in repos without a `.gitignore`.
const SKIPPED_DIRS: &[&str] = &["node_modules", "dist", "build", "out", "coverage", "target"];

/// Walk `root`, honouring `.gitignore` even outside a git checkout. Unreadable
/// entries are reported on stderr and skipped rather than ending the walk.
pub fn discover(root: &Path, options: &WalkOptions) -> Result<Vec<SourceFile>> {
    let excludes = build_globs(&options.excludes)?;
    let mut files = Vec::new();
    let walker = WalkBuilder::new(root)
        .require_git(false)
        .filter_entry(|entry| !SKIPPED_DIRS.iter().any(|dir| entry.file_name() == *dir))
        .build();
    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                eprintln!("duped: skipping {err}");
                continue;
            }
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.into_path();
        let Some(language) = language_for(&path) else {
            continue;
        };
        let rel = relative(root, &path);
        if is_excluded(&excludes, &rel) || (!options.include_tests && is_test_path(&rel)) {
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
        "cs" => Some(Language::CSharp),
        _ => None,
    }
}

/// Test, mock and fixture paths, e.g. `src/a.test.ts`, `src/a.mock.ts`, `src/mocks/a.ts`.
pub fn is_test_path(rel: &str) -> bool {
    const DIRS: &[&str] = &[
        "test",
        "tests",
        "__tests__",
        "__mocks__",
        "mocks",
        "fixtures",
        "fixture",
        "__fixtures__",
        "__snapshots__",
        "e2e",
    ];
    const FILE_MARKERS: &[&str] = &[".test.", ".spec.", ".e2e-spec.", ".mock.", ".fixture."];
    // C# test projects and classes, e.g. `Foo.UnitTests/RepoTests.cs`.
    const DIR_SUFFIXES: &[&str] = &[".Tests", ".Test", ".UnitTests", ".IntegrationTests"];
    const CS_FILE_SUFFIXES: &[&str] = &["Tests.cs", "Test.cs"];
    let mut segments = rel.split('/').peekable();
    while let Some(segment) = segments.next() {
        if segments.peek().is_none() {
            return FILE_MARKERS.iter().any(|marker| segment.contains(marker))
                || CS_FILE_SUFFIXES
                    .iter()
                    .any(|suffix| segment.ends_with(suffix));
        }
        if DIRS.contains(&segment) || DIR_SUFFIXES.iter().any(|suffix| segment.ends_with(suffix)) {
            return true;
        }
    }
    false
}

/// A path is excluded when a glob matches it or any of its parent directories.
fn is_excluded(excludes: &GlobSet, rel: &str) -> bool {
    excludes.is_match(rel)
        || rel
            .match_indices('/')
            .any(|(i, _)| excludes.is_match(&rel[..i]))
}

fn relative(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    if rel.as_os_str().is_empty() {
        // The root itself is the file, e.g. `duped extract src/a.ts`.
        return path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
    }
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// `*` stays within one path segment; `**` crosses them.
fn build_globs(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .with_context(|| format!("invalid --exclude glob `{pattern}`"))?;
        builder.add(glob);
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
        assert!(is_test_path("src/app.e2e-spec.ts"));
        assert!(is_test_path("src/user.mock.ts"));
        assert!(is_test_path("src/db.fixture.ts"));
        assert!(is_test_path("src/__tests__/a.ts"));
        assert!(is_test_path("src/mocks/handlers.ts"));
        assert!(is_test_path("packages/x/test/helpers.ts"));
        assert!(!is_test_path("src/testing.ts"));
        assert!(!is_test_path("src/latest/a.ts"));
        assert!(!is_test_path("src/openapi-spec.ts"));
        assert!(is_test_path("src/RepoTests.cs"));
        assert!(is_test_path("src/ParserTest.cs"));
        assert!(is_test_path("Acme.Core.UnitTests/Helpers.cs"));
        assert!(is_test_path("Acme.Api.Tests/Fakes.cs"));
        assert!(!is_test_path("src/TestHelpers.cs"));
        assert!(!is_test_path("src/Contest.cs"));
        assert!(!is_test_path("src/RepoTests.ts"));
        assert!(!is_test_path("Acme.Testing/Repo.cs"));
    }

    #[test]
    fn maps_extensions() {
        assert_eq!(language_for(Path::new("a.tsx")), Some(Language::TypeScript));
        assert_eq!(language_for(Path::new("a.mjs")), Some(Language::JavaScript));
        assert_eq!(language_for(Path::new("Repo.cs")), Some(Language::CSharp));
        assert_eq!(language_for(Path::new("a.rs")), None);
    }

    #[test]
    fn discovers_sources_with_filters() {
        let root = std::env::temp_dir().join(format!("duped-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for file in [
            "src/a.ts",
            "src/a.test.ts",
            "src/gen/b.ts",
            "src/gen/deep/c.ts",
            "src/legacy/d.ts",
            "generated/e.ts",
            "node_modules/pkg/f.ts",
            "README.md",
        ] {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        // Not a git checkout: `.gitignore` must still apply.
        std::fs::write(root.join(".gitignore"), "generated/\n").unwrap();

        let rels = |options: &WalkOptions| -> Vec<String> {
            discover(&root, options)
                .unwrap()
                .into_iter()
                .map(|f| f.rel)
                .collect()
        };
        let excludes = vec!["src/gen/*.ts".to_string(), "src/legacy".to_string()];
        assert_eq!(
            rels(&WalkOptions {
                include_tests: false,
                excludes
            }),
            vec!["src/a.ts", "src/gen/deep/c.ts"]
        );
        assert_eq!(
            rels(&WalkOptions {
                include_tests: true,
                excludes: Vec::new()
            })
            .len(),
            5
        );

        let single = discover(
            &root.join("src/a.ts"),
            &WalkOptions {
                include_tests: false,
                excludes: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(single[0].rel, "a.ts");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
