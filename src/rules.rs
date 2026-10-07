//! Per-language rules: members that don't count toward type similarity, and which paths
//! are tests. Built-in defaults per language, edited per repo under `[rules.<language>]`.

use std::collections::BTreeMap;

use anyhow::{Result, bail};

use crate::config::{LanguageRulesConfig, ListEdit};
use crate::record::Language;

#[derive(Debug, Clone)]
pub struct LanguageRules {
    /// Member names ignored when comparing types, e.g. `toString`.
    pub conventional_members: Vec<String>,
    /// A file is a test when its name matches one of these, e.g. `*.test.*`.
    pub test_files: Vec<String>,
    /// A file is a test when a directory above it matches one of these, ignoring case,
    /// e.g. `__tests__`.
    pub test_dirs: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Rules {
    languages: Vec<(Language, LanguageRules)>,
}

/// Every language with rules; a new extractor adds itself here and in [`defaults_for`].
const LANGUAGES: &[Language] = &[
    Language::TypeScript,
    Language::JavaScript,
    Language::Python,
    Language::Rust,
    Language::CSharp,
];

/// Test patterns every language starts with.
const TEST_FILES: &[&str] = &[
    "*.test.*",
    "*.spec.*",
    "*.e2e-spec.*",
    "*.mock.*",
    "*.fixture.*",
];
const TEST_DIRS: &[&str] = &[
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
    "unittests",
    "integrationtests",
    "benches",
];

fn defaults_for(language: Language) -> LanguageRules {
    let (conventional, files, dirs): (&[&str], &[&str], &[&str]) = match language {
        Language::TypeScript | Language::JavaScript => (
            &[
                "toString",
                "toJSON",
                "valueOf",
                "constructor",
                "[Symbol.iterator]",
            ],
            &[],
            &[],
        ),
        // pytest's conventions, and their `.pyi` stubs.
        Language::Python => (
            &[],
            &[
                "test_*.py",
                "*_test.py",
                "conftest.py",
                "test_*.pyi",
                "*_test.pyi",
                "conftest.pyi",
            ],
            &[],
        ),
        // Out-of-line `#[cfg(test)] mod tests;`.
        Language::Rust => (&[], &["tests.rs", "test.rs"], &[]),
        // Test classes and doubles, and test projects, e.g. `Foo.UnitTests/RepoTests.cs`.
        Language::CSharp => (
            &[],
            &["*Tests.cs", "*Mock.cs", "*Fake.cs", "Fake*.cs"],
            &["*.Tests", "*.Test", "*.UnitTests", "*.IntegrationTests"],
        ),
    };
    LanguageRules {
        conventional_members: strings(conventional),
        test_files: strings(&[TEST_FILES, files].concat()),
        test_dirs: strings(&[TEST_DIRS, dirs].concat()),
    }
}

impl Default for Rules {
    fn default() -> Self {
        Rules {
            languages: LANGUAGES.iter().map(|&l| (l, defaults_for(l))).collect(),
        }
    }
}

impl Rules {
    /// Defaults with `[rules.<language>]` edits applied; unknown language names are errors.
    pub fn with_config(config: &BTreeMap<String, LanguageRulesConfig>) -> Result<Self> {
        let mut rules = Rules::default();
        for (name, edits) in config {
            let Some((_, language)) = rules
                .languages
                .iter_mut()
                .find(|(l, _)| language_name(*l) == *name)
            else {
                let known: Vec<String> = LANGUAGES.iter().map(|&l| language_name(l)).collect();
                bail!(
                    "[rules.{name}]: unknown language; expected one of {}",
                    known.join(", ")
                );
            };
            edit(
                &mut language.conventional_members,
                &edits.conventional_members,
            );
            edit(&mut language.test_files, &edits.test_files);
            edit(&mut language.test_dirs, &edits.test_dirs);
        }
        Ok(rules)
    }

    pub fn get(&self, language: Language) -> &LanguageRules {
        &self
            .languages
            .iter()
            .find(|(l, _)| *l == language)
            .expect("every language has rules")
            .1
    }
}

impl LanguageRules {
    pub fn is_conventional(&self, member: &str) -> bool {
        self.conventional_members.iter().any(|m| m == member)
    }

    /// e.g. `src/a.test.ts`, `src/a.mock.ts`, `src/mocks/a.ts`.
    pub fn is_test_path(&self, rel: &str) -> bool {
        let mut segments = rel.split('/').peekable();
        while let Some(segment) = segments.next() {
            if segments.peek().is_none() {
                return self.test_files.iter().any(|p| wildcard(p, segment));
            }
            // Case-insensitive: .NET repos use `Tests/` and `UnitTests/`.
            let segment = segment.to_ascii_lowercase();
            if self
                .test_dirs
                .iter()
                .any(|p| wildcard(&p.to_ascii_lowercase(), &segment))
            {
                return true;
            }
        }
        false
    }
}

/// `*` matches any run of characters, e.g. `test_*.py` matches `test_api.py`; a pattern
/// without `*` is an exact name.
fn wildcard(pattern: &str, text: &str) -> bool {
    let mut parts: Vec<&str> = pattern.split('*').collect();
    let first = parts.remove(0);
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    let Some(last) = parts.pop() else {
        return rest.is_empty();
    };
    for part in parts {
        match rest.find(part) {
            Some(i) => rest = &rest[i + part.len()..],
            None => return false,
        }
    }
    rest.ends_with(last)
}

/// The language's name as written in records and config, e.g. `typescript`.
pub fn language_name(language: Language) -> String {
    serde_json::to_value(language)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn edit(list: &mut Vec<String>, change: &Option<ListEdit>) {
    match change {
        None => {}
        Some(ListEdit::Replace(items)) => *list = items.clone(),
        Some(ListEdit::Edit(edits)) => {
            list.retain(|item| !edits.remove.contains(item));
            for item in &edits.add {
                if !list.contains(item) {
                    list.push(item.clone());
                }
            }
        }
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EditList, LanguageRulesConfig};

    #[test]
    fn detects_test_paths() {
        let rules = Rules::default();
        let ts = rules.get(Language::TypeScript);
        assert!(ts.is_test_path("src/a.test.ts"));
        assert!(ts.is_test_path("src/a.spec.tsx"));
        assert!(ts.is_test_path("src/app.e2e-spec.ts"));
        assert!(ts.is_test_path("src/user.mock.ts"));
        assert!(ts.is_test_path("src/db.fixture.ts"));
        assert!(ts.is_test_path("src/__tests__/a.ts"));
        assert!(ts.is_test_path("src/mocks/handlers.ts"));
        assert!(ts.is_test_path("packages/x/test/helpers.ts"));
        assert!(!ts.is_test_path("src/testing.ts"));
        assert!(!ts.is_test_path("src/latest/a.ts"));
        assert!(!ts.is_test_path("src/openapi-spec.ts"));
        assert!(ts.is_test_path("src/Tests/helpers.ts"));
        assert!(ts.is_test_path("benches/parse.ts"));
        assert!(!ts.is_test_path("src/UserMock.ts"));
        assert!(!ts.is_test_path("Acme.Tests/client.ts"));

        let rs = rules.get(Language::Rust);
        assert!(rs.is_test_path("src/parser/tests.rs"));
        assert!(rs.is_test_path("src/test.rs"));
        assert!(!rs.is_test_path("src/contest.rs"));

        let py = rules.get(Language::Python);
        assert!(py.is_test_path("pkg/test_api.py"));
        assert!(py.is_test_path("pkg/api_test.py"));
        assert!(py.is_test_path("pkg/conftest.py"));
        assert!(py.is_test_path("pkg/test_api.pyi"));
        assert!(!py.is_test_path("pkg/testing.py"));
        assert!(!py.is_test_path("pkg/latest.py"));

        let cs = rules.get(Language::CSharp);
        assert!(cs.is_test_path("src/RepoTests.cs"));
        assert!(cs.is_test_path("Acme/UnitTests/Repo.cs"));
        assert!(cs.is_test_path("src/UserMock.cs"));
        assert!(cs.is_test_path("src/ClockFake.cs"));
        assert!(cs.is_test_path("src/FakeClock.cs"));
        assert!(!cs.is_test_path("src/Mockingbird.cs"));
        assert!(
            !cs.is_test_path("src/LoadTest.cs"),
            "`*Test.cs` is often production code"
        );
        assert!(cs.is_test_path("Acme.Core.UnitTests/Helpers.cs"));
        assert!(cs.is_test_path("Acme.Api.Tests/Fakes.cs"));
        assert!(!cs.is_test_path("src/TestHelpers.cs"));
        assert!(!cs.is_test_path("src/Contest.cs"));
        assert!(!cs.is_test_path("Acme.Testing/Repo.cs"));
    }

    #[test]
    fn matches_wildcards() {
        assert!(wildcard("*.test.*", "a.test.ts"));
        assert!(wildcard("test_*.py", "test_.py"));
        assert!(wildcard("a*b*c", "abbc"));
        assert!(wildcard("tests.rs", "tests.rs"));
        assert!(!wildcard("tests.rs", "contests.rs"));
        assert!(!wildcard("*_test.py", "_test.pyi"));
        assert!(!wildcard("a*a", "a"));
    }

    #[test]
    fn config_replaces_or_edits_lists() {
        let mut config = BTreeMap::new();
        config.insert(
            "typescript".to_string(),
            LanguageRulesConfig {
                conventional_members: Some(ListEdit::Edit(EditList {
                    add: vec!["dispose".into()],
                    remove: vec!["toJSON".into()],
                })),
                test_dirs: Some(ListEdit::Replace(vec!["qa".into()])),
                test_files: None,
            },
        );
        let rules = Rules::with_config(&config).unwrap();
        let ts = rules.get(Language::TypeScript);
        assert!(ts.is_conventional("dispose") && ts.is_conventional("toString"));
        assert!(!ts.is_conventional("toJSON"));
        assert!(ts.is_test_path("qa/a.ts") && !ts.is_test_path("tests/a.ts"));
        assert!(ts.is_test_path("src/a.test.ts"));
        // Other languages keep their defaults.
        assert!(rules.get(Language::JavaScript).is_conventional("toJSON"));
    }

    #[test]
    fn rejects_unknown_languages() {
        let mut config = BTreeMap::new();
        config.insert("cobol".to_string(), LanguageRulesConfig::default());
        let err = Rules::with_config(&config).unwrap_err().to_string();
        assert!(err.contains("[rules.cobol]") && err.contains("typescript"));
    }
}
