//! What the project says about a duplicate pair: its module tag, and whether it's
//! acknowledged as deliberate, in `duped.toml` or by a doc comment on either side.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::analysis::{Judge, Verdict};
use crate::config::Acknowledged;
use crate::modules::{Home, ModuleGraph, ROOT_MODULE};
use crate::record::{Record, qualified};

/// Doc phrases that mark a copy as deliberate, matched case-insensitively.
const DELIBERATE: &[&str] = &[
    "structural twin",
    "kept in sync",
    "mirror of",
    "mirrors",
    "copy of",
];

pub struct ProjectJudge<'a> {
    graph: &'a ModuleGraph,
    records: &'a [Record],
    /// Module of each record, by index.
    modules: Vec<Option<usize>>,
    /// Module tags need the compared records to span at least two modules; otherwise every
    /// pair would be "same module".
    tagging: bool,
    /// What is compared, for the summary, e.g. `types`.
    noun: &'static str,
    acknowledged: &'a [Acknowledged],
    /// Directory the records' `file` paths are relative to.
    base: PathBuf,
    /// Leading comment of each file, lower-cased, read on first use.
    headers: RefCell<HashMap<String, String>>,
}

impl<'a> ProjectJudge<'a> {
    pub fn new(
        graph: &'a ModuleGraph,
        records: &'a [Record],
        base: PathBuf,
        acknowledged: &'a [Acknowledged],
        noun: &'static str,
        compared: impl Fn(&Record) -> bool,
    ) -> Self {
        let mut by_file: HashMap<&str, Option<usize>> = HashMap::new();
        let modules: Vec<Option<usize>> = records
            .iter()
            .map(|r| {
                let file = r.location().file.as_str();
                *by_file
                    .entry(file)
                    .or_insert_with(|| graph.module_of(&base.join(file)))
            })
            .collect();
        let spanned: HashSet<Option<usize>> = records
            .iter()
            .zip(&modules)
            .filter(|(r, _)| compared(r))
            .map(|(_, &m)| m)
            .collect();
        ProjectJudge {
            graph,
            records,
            modules,
            tagging: spanned.len() >= 2,
            noun,
            acknowledged,
            base,
            headers: RefCell::default(),
        }
    }

    fn acknowledgement(&self, a: usize, b: usize) -> Option<String> {
        let (ta, tb) = (&self.records[a], &self.records[b]);
        let (ma, mb) = (
            self.graph.name(self.modules[a]),
            self.graph.name(self.modules[b]),
        );
        for entry in self.acknowledged {
            let forward = side_matches(&entry.a, ta, ma) && side_matches(&entry.b, tb, mb);
            let backward = side_matches(&entry.a, tb, mb) && side_matches(&entry.b, ta, ma);
            if forward || backward {
                return Some(
                    entry
                        .reason
                        .clone()
                        .unwrap_or_else(|| "duped.toml".to_string()),
                );
            }
        }
        // A deliberate-copy phrase only counts when it names the other side: the other type
        // as a whole word (not this type's own name), or the other module by full name, last
        // name segment or directory, the latter two as whole words.
        let sides = [(ta, tb, mb, self.modules[b]), (tb, ta, ma, self.modules[a])];
        for (this, other, other_module, other_index) in sides {
            let text = format!(
                "{} {}",
                this.doc().unwrap_or_default().to_lowercase(),
                self.header(&this.location().file)
            );
            let module_words = self.module_words(other_index);
            let names_other = |clause: &str| {
                let names_type = other.name() != this.name()
                    && identifiers(clause).any(|word| word == other.name().to_lowercase());
                let names_module = self.tagging
                    && other_module != ROOT_MODULE
                    && (contains_module(clause, &other_module.to_lowercase())
                        || module_words
                            .iter()
                            .any(|word| package_words(clause).any(|w| w == word)));
                names_type || names_module
            };
            // The name must follow the phrase in the same sentence: "mirrors `X`", "twin of the
            // evals `X`", not a phrase in one paragraph and a name in another.
            for phrase in DELIBERATE {
                if text
                    .match_indices(phrase)
                    .any(|(at, _)| names_other(clause_after(&text, at)))
                {
                    return Some(format!("doc: {phrase}"));
                }
            }
        }
        None
    }

    /// Short names a doc may use for a module: its name's last segment and its directory,
    /// e.g. `unified-evals` and `evals` for `@x/unified-evals` in `packages/evals`.
    fn module_words(&self, module: Option<usize>) -> Vec<String> {
        let Some(module) = module.map(|m| &self.graph.modules[m]) else {
            return Vec::new();
        };
        let last = module.name.rsplit('/').next().unwrap_or(&module.name);
        let dir = module
            .root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let mut words: Vec<String> = [last, dir]
            .iter()
            .map(|w| w.to_lowercase())
            .filter(|w| w.len() >= 3)
            .collect();
        words.dedup();
        words
    }

    /// The comment block at the top of `file`, e.g. a header saying the file mirrors another.
    fn header(&self, file: &str) -> String {
        if let Some(text) = self.headers.borrow().get(file) {
            return text.clone();
        }
        let source = std::fs::read_to_string(self.base.join(file)).unwrap_or_default();
        let mut text = leading_comment(&source).to_lowercase();
        // Licence headers say "a copy of the License"; they never mark a type as deliberate.
        if text.contains("license") || text.contains("copyright") {
            text.clear();
        }
        self.headers
            .borrow_mut()
            .insert(file.to_string(), text.clone());
        text
    }
}

impl Judge for ProjectJudge<'_> {
    fn verdict(&self, a: usize, b: usize) -> Verdict {
        Verdict {
            tag: self
                .tagging
                .then(|| self.graph.tag(self.modules[a], self.modules[b])),
            acknowledged: self.acknowledgement(a, b),
        }
    }

    fn module(&self, record: usize) -> Option<&str> {
        self.tagging.then(|| self.graph.name(self.modules[record]))
    }

    fn home(&self, members: &[usize]) -> Option<Home> {
        if !self.tagging {
            return None;
        }
        let modules: Vec<Option<usize>> = members.iter().map(|&m| self.modules[m]).collect();
        self.graph.home(&modules)
    }

    fn summary(&self) -> Option<String> {
        let found = self.graph.modules.len();
        Some(if self.tagging {
            format!("{found} modules")
        } else if found >= 2 {
            format!(
                "{found} modules, but the scanned {} are in fewer than 2, so pairs are not tagged",
                self.noun
            )
        } else {
            "fewer than 2 modules, so pairs are not tagged".to_string()
        })
    }
}

/// Module tags for whole files, by index; no acknowledgements.
pub struct FileJudge<'a> {
    graph: &'a ModuleGraph,
    modules: Vec<Option<usize>>,
    /// As for [`ProjectJudge`]: the scanned files must span at least two modules.
    tagging: bool,
}

impl<'a> FileJudge<'a> {
    /// `files` are relative to `base`.
    pub fn new<'f>(
        graph: &'a ModuleGraph,
        base: &Path,
        files: impl IntoIterator<Item = &'f str>,
    ) -> Self {
        let modules: Vec<Option<usize>> = files
            .into_iter()
            .map(|f| graph.module_of(&base.join(f)))
            .collect();
        let spanned: HashSet<Option<usize>> = modules.iter().copied().collect();
        FileJudge {
            graph,
            tagging: spanned.len() >= 2,
            modules,
        }
    }
}

impl Judge for FileJudge<'_> {
    fn verdict(&self, a: usize, b: usize) -> Verdict {
        Verdict {
            tag: self
                .tagging
                .then(|| self.graph.tag(self.modules[a], self.modules[b])),
            acknowledged: None,
        }
    }

    fn module(&self, file: usize) -> Option<&str> {
        self.tagging.then(|| self.graph.name(self.modules[file]))
    }

    /// File pairs aren't clustered.
    fn home(&self, _: &[usize]) -> Option<Home> {
        None
    }

    fn summary(&self) -> Option<String> {
        let found = self.graph.modules.len();
        Some(if self.tagging {
            format!("{found} modules")
        } else {
            "the scanned files are in fewer than 2 modules, so pairs are not tagged".to_string()
        })
    }
}

/// `Name`, `Scope.Name`, or either prefixed with `module:` (full or last `/` segment).
fn side_matches(pattern: &str, t: &Record, module: &str) -> bool {
    let (wanted_module, name) = match pattern.split_once(':') {
        Some((m, n)) => (Some(m), n),
        None => (None, pattern),
    };
    let name_matches = name == t.name() || name == qualified(t.scope(), t.name());
    let module_matches =
        wanted_module.is_none_or(|m| m == module || module.rsplit('/').next() == Some(m));
    name_matches && module_matches
}

/// Lower-cased identifier words of `text`, e.g. `mirrors \`FooBar\`` → `mirrors`, `foobar`.
fn identifiers(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
        .filter(|w| !w.is_empty())
}

/// From `at` to the end of its sentence, at most 200 bytes: `mirrors \`X\` from …`.
fn clause_after(text: &str, at: usize) -> &str {
    let mut end = (at + 200).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let clause = &text[at..end];
    match clause.find(". ") {
        Some(stop) => &clause[..stop],
        None => clause,
    }
}

/// Lower-cased words of `text`, keeping `-` so `unified-evals` stays one word.
fn package_words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
        .filter(|w| !w.is_empty())
}

/// `module` appears in `text` with no package-name character on either side.
fn contains_module(text: &str, module: &str) -> bool {
    let name_char = |c: char| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '@');
    text.match_indices(module).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + module.len()..].chars().next();
        !before.is_some_and(name_char) && !after.is_some_and(name_char)
    })
}

/// Comments before the first line of code, in any supported language.
fn leading_comment(source: &str) -> String {
    let mut out = Vec::new();
    let mut in_block = false;
    for line in source.lines() {
        let line = line.trim();
        if in_block {
            out.push(line);
            in_block = !line.contains("*/");
            continue;
        }
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("/*") {
            out.push(line);
            in_block = !rest.contains("*/");
        } else if line.starts_with("//") || line.starts_with('#') {
            out.push(line);
        } else {
            break;
        }
    }
    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::Module;
    use crate::record::{Language, Location, TypeKind, TypeRecord};

    fn ty(name: &str, file: &str, doc: Option<&str>) -> Record {
        Record::Type(TypeRecord {
            language: Language::TypeScript,
            name: name.into(),
            kind: TypeKind::Interface,
            scope: None,
            fields: Vec::new(),
            extends: Vec::new(),
            exported: true,
            location: Location {
                file: file.into(),
                start_line: 1,
                end_line: 1,
            },
            doc: doc.map(str::to_string),
        })
    }

    fn is_type(r: &Record) -> bool {
        matches!(r, Record::Type(_))
    }

    fn graph() -> ModuleGraph {
        ModuleGraph::new(vec![
            Module {
                name: "@x/canvas".into(),
                root: "/repo/canvas".into(),
                deps: vec![],
                aliases: Vec::new(),
            },
            Module {
                name: "@x/unified-evals".into(),
                root: "/repo/evals".into(),
                deps: vec![],
                aliases: Vec::new(),
            },
        ])
    }

    #[test]
    fn explicit_acknowledgements_match_either_order_and_module_prefix() {
        let g = graph();
        let records = vec![
            ty("Row", "canvas/a.ts", None),
            ty("Row", "evals/b.ts", None),
        ];
        let acks = vec![Acknowledged {
            a: "unified-evals:Row".into(),
            b: "@x/canvas:Row".into(),
            reason: Some("boundary".into()),
        }];
        let judge = ProjectJudge::new(&g, &records, "/repo".into(), &acks, "types", is_type);
        assert_eq!(
            judge.verdict(0, 1).acknowledged.as_deref(),
            Some("boundary")
        );
        assert_eq!(judge.module(0), Some("@x/canvas"));

        let wrong = vec![Acknowledged {
            a: "canvas2:Row".into(),
            b: "Row".into(),
            reason: None,
        }];
        let judge = ProjectJudge::new(&g, &records, "/repo".into(), &wrong, "types", is_type);
        assert_eq!(judge.verdict(0, 1).acknowledged, None);
    }

    #[test]
    fn doc_phrases_count_only_when_they_name_the_other_side() {
        let g = graph();
        let records = vec![
            ty(
                "Anchor",
                "canvas/a.ts",
                Some("Structural twin of the type in `@x/unified-evals`."),
            ),
            ty("Anchor", "evals/b.ts", None),
            ty("Other", "canvas/c.ts", Some("Returns a copy of the list.")),
            ty("Thing", "evals/d.ts", None),
        ];
        let judge = ProjectJudge::new(&g, &records, "/repo".into(), &[], "types", is_type);
        assert_eq!(
            judge.verdict(0, 1).acknowledged.as_deref(),
            Some("doc: structural twin")
        );
        assert_eq!(judge.verdict(2, 3).acknowledged, None);
    }

    #[test]
    fn file_headers_count_as_docs() {
        let dir = std::env::temp_dir().join(format!("duped-judge-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("canvas")).unwrap();
        std::fs::write(
            dir.join("canvas/a.ts"),
            "/**\n * Structural twins of the canonical types in `@x/unified-evals`.\n */\nexport interface Anchor {}\n",
        )
        .unwrap();
        let records = vec![
            ty("Anchor", "canvas/a.ts", None),
            ty("Anchor", "evals/b.ts", None),
        ];
        let base = dir.canonicalize().unwrap();
        let g = ModuleGraph::new(vec![
            Module {
                name: "@x/canvas".into(),
                root: base.join("canvas"),
                deps: vec![],
                aliases: Vec::new(),
            },
            Module {
                name: "@x/unified-evals".into(),
                root: base.join("evals"),
                deps: vec![],
                aliases: Vec::new(),
            },
        ]);
        let judge = ProjectJudge::new(&g, &records, base, &[], "types", is_type);
        assert_eq!(
            judge.verdict(0, 1).acknowledged.as_deref(),
            Some("doc: structural twin")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn judges_function_pairs_by_function_modules() {
        use crate::record::FunctionRecord;
        let function = |name: &str, file: &str| {
            Record::Function(FunctionRecord {
                language: Language::TypeScript,
                name: name.into(),
                scope: None,
                params: Vec::new(),
                returns: None,
                exported: true,
                location: Location {
                    file: file.into(),
                    start_line: 1,
                    end_line: 1,
                },
                doc: None,
                body: Vec::new(),
            })
        };
        let g = graph();
        // The only type is in canvas; functions span both modules.
        let records = vec![
            ty("T", "canvas/t.ts", None),
            function("load", "canvas/a.ts"),
            function("fetch", "evals/b.ts"),
        ];
        let acks = vec![Acknowledged {
            a: "unified-evals:fetch".into(),
            b: "load".into(),
            reason: None,
        }];
        let judge = ProjectJudge::new(&g, &records, "/repo".into(), &acks, "functions", |r| {
            matches!(r, Record::Function(_))
        });
        let verdict = judge.verdict(1, 2);
        assert_eq!(verdict.tag, Some(crate::modules::Tag::Boundary));
        assert_eq!(verdict.acknowledged.as_deref(), Some("duped.toml"));
    }

    #[test]
    fn a_single_module_disables_tags() {
        let g = ModuleGraph::new(vec![Module {
            name: "only".into(),
            root: "/repo".into(),
            deps: vec![],
            aliases: Vec::new(),
        }]);
        let records = vec![ty("A", "a.ts", None), ty("B", "b.ts", None)];
        let judge = ProjectJudge::new(&g, &records, "/repo".into(), &[], "types", is_type);
        assert_eq!(judge.verdict(0, 1).tag, None);
        assert_eq!(judge.module(0), None);
    }

    #[test]
    fn tags_only_when_the_scanned_types_span_two_modules() {
        // Two modules exist, but every scanned type is in one of them.
        let g = graph();
        let records = vec![ty("A", "canvas/a.ts", None), ty("B", "canvas/b.ts", None)];
        let judge = ProjectJudge::new(&g, &records, "/repo".into(), &[], "types", is_type);
        assert_eq!(judge.verdict(0, 1).tag, None);
        assert!(judge.summary().unwrap().contains("fewer than 2"));
    }

    #[test]
    fn doc_mentions_must_be_whole_words_and_name_the_other_side() {
        let g = graph();
        let ack = |doc: &str, a: &str, b: &str| {
            let records = vec![ty(a, "canvas/a.ts", Some(doc)), ty(b, "evals/b.ts", None)];
            ProjectJudge::new(&g, &records, "/repo".into(), &[], "types", is_type)
                .verdict(0, 1)
                .acknowledged
        };
        // `browserstate` contains `row`, `metadata` contains `data`: not mentions.
        assert_eq!(ack("Mirrors BrowserState.", "Foo", "Row"), None);
        assert_eq!(
            ack("Returns a copy of the node's metadata.", "Snapshot", "Data"),
            None
        );
        // A same-name pair: the doc naming its own type doesn't name the other side.
        assert_eq!(ack("Row mirrors the DB row layout.", "Row", "Row"), None);
        // Short module names count only as whole words: not inside `evaluation`.
        assert_eq!(ack("Mirrors the evaluation view.", "Row", "Row"), None);
        assert_eq!(
            ack("Mirrors the unified-evals view.", "Row", "Row").as_deref(),
            Some("doc: mirrors")
        );
        assert_eq!(
            ack("Structural twin of the evals `Row`.", "Row", "Row").as_deref(),
            Some("doc: structural twin")
        );
        assert_eq!(
            ack("Mirrors `@x/unified-evals`.", "Row", "Row").as_deref(),
            Some("doc: mirrors")
        );
        assert_eq!(
            ack("Mirrors `Bar` for the wire.", "Foo", "Bar").as_deref(),
            Some("doc: mirrors")
        );
        // A phrase in one sentence and a name in another are unrelated.
        assert_eq!(
            ack(
                "The on-disk mirror of the resource set. See the canvas `Bar`.",
                "Foo",
                "Bar"
            ),
            None
        );
        assert_eq!(
            ack(
                "The on-disk mirror of the resource set. Used by @x/unified-evals.",
                "Row",
                "Row"
            ),
            None
        );
    }

    #[test]
    fn licence_headers_are_not_acknowledgements() {
        let dir = std::env::temp_dir().join(format!("duped-judge-licence-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("canvas")).unwrap();
        std::fs::write(
            dir.join("canvas/a.ts"),
            "// Licensed under the Apache License. You may obtain a copy of the License at …\nexport interface Foo {}\n",
        )
        .unwrap();
        let base = dir.canonicalize().unwrap();
        let g = ModuleGraph::new(vec![
            Module {
                name: "c".into(),
                root: base.join("canvas"),
                deps: vec![],
                aliases: Vec::new(),
            },
            Module {
                name: "e".into(),
                root: base.join("evals"),
                deps: vec![],
                aliases: Vec::new(),
            },
        ]);
        let records = vec![
            ty("Foo", "canvas/a.ts", None),
            ty("License", "evals/b.ts", None),
        ];
        let judge = ProjectJudge::new(&g, &records, base, &[], "types", is_type);
        assert_eq!(judge.verdict(0, 1).acknowledged, None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reads_leading_comments() {
        assert_eq!(
            leading_comment("\n// one\n/* two */\nimport x;\n// later"),
            "// one /* two */"
        );
        assert_eq!(leading_comment("# py\n\"\"\"doc\"\"\""), "# py");
        assert_eq!(
            leading_comment("/*\nMirrors Canonical.\n*/\nimport x;"),
            "/* Mirrors Canonical. */"
        );
    }
}
