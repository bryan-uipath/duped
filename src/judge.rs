//! What the project says about a duplicate pair: its module tag, and whether it's
//! acknowledged as deliberate, in `duped.toml` or by a doc comment on either side.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::analysis::{Judge, Verdict};
use crate::config::Acknowledged;
use crate::modules::{Home, ModuleGraph};
use crate::record::{Record, TypeRecord, qualified};

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
    /// Module tags need at least two modules; with one, every pair is "same module".
    tagging: bool,
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
    ) -> Self {
        let mut by_file: HashMap<&str, Option<usize>> = HashMap::new();
        let modules = records
            .iter()
            .map(|r| {
                let file = r.location().file.as_str();
                *by_file
                    .entry(file)
                    .or_insert_with(|| graph.module_of(&base.join(file)))
            })
            .collect();
        ProjectJudge {
            graph,
            records,
            modules,
            tagging: graph.modules.len() >= 2,
            acknowledged,
            base,
            headers: RefCell::default(),
        }
    }

    fn type_at(&self, index: usize) -> &TypeRecord {
        match &self.records[index] {
            Record::Type(t) => t,
            Record::Function(_) => unreachable!("pairs only reference type records"),
        }
    }

    fn acknowledgement(&self, a: usize, b: usize) -> Option<String> {
        let (ta, tb) = (self.type_at(a), self.type_at(b));
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
        // A deliberate-copy phrase only counts when it names the other side.
        for (this, other, other_module) in [(ta, tb, mb), (tb, ta, ma)] {
            let text = format!(
                "{} {}",
                this.doc.as_deref().unwrap_or_default().to_lowercase(),
                self.header(&this.location.file)
            );
            let Some(phrase) = DELIBERATE.iter().find(|p| text.contains(*p)) else {
                continue;
            };
            let mentions =
                |needle: &str| needle.len() >= 3 && text.contains(&needle.to_lowercase());
            let short_module = other_module.rsplit('/').next().unwrap_or(other_module);
            if mentions(&other.name)
                || (self.tagging && (mentions(other_module) || mentions(short_module)))
            {
                return Some(format!("doc: {phrase}"));
            }
        }
        None
    }

    /// The comment block at the top of `file`, e.g. a header saying the file mirrors another.
    fn header(&self, file: &str) -> String {
        if let Some(text) = self.headers.borrow().get(file) {
            return text.clone();
        }
        let source = std::fs::read_to_string(self.base.join(file)).unwrap_or_default();
        let text = leading_comment(&source).to_lowercase();
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
        Some(if self.tagging {
            format!("{} modules", self.graph.modules.len())
        } else {
            "fewer than 2 modules, so pairs are not tagged".to_string()
        })
    }
}

/// `Name`, `Scope.Name`, or either prefixed with `module:` (full or last `/` segment).
fn side_matches(pattern: &str, t: &TypeRecord, module: &str) -> bool {
    let (wanted_module, name) = match pattern.split_once(':') {
        Some((m, n)) => (Some(m), n),
        None => (None, pattern),
    };
    let name_matches = name == t.name || name == qualified(t.scope.as_deref(), &t.name);
    let module_matches =
        wanted_module.is_none_or(|m| m == module || module.rsplit('/').next() == Some(m));
    name_matches && module_matches
}

/// Comment lines before the first line of code, in any supported language.
fn leading_comment(source: &str) -> String {
    let mut out = Vec::new();
    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let is_comment = ["//", "/*", "*", "#"].iter().any(|p| line.starts_with(p));
        if !is_comment {
            break;
        }
        out.push(line);
    }
    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::Module;
    use crate::record::{Language, Location, TypeKind};

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

    fn graph() -> ModuleGraph {
        ModuleGraph::new(vec![
            Module {
                name: "@x/canvas".into(),
                root: "/repo/canvas".into(),
                deps: vec![],
            },
            Module {
                name: "@x/unified-evals".into(),
                root: "/repo/evals".into(),
                deps: vec![],
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
        let judge = ProjectJudge::new(&g, &records, "/repo".into(), &acks);
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
        let judge = ProjectJudge::new(&g, &records, "/repo".into(), &wrong);
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
        let judge = ProjectJudge::new(&g, &records, "/repo".into(), &[]);
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
            },
            Module {
                name: "@x/unified-evals".into(),
                root: base.join("evals"),
                deps: vec![],
            },
        ]);
        let judge = ProjectJudge::new(&g, &records, base, &[]);
        assert_eq!(
            judge.verdict(0, 1).acknowledged.as_deref(),
            Some("doc: structural twin")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_single_module_disables_tags() {
        let g = ModuleGraph::new(vec![Module {
            name: "only".into(),
            root: "/repo".into(),
            deps: vec![],
        }]);
        let records = vec![ty("A", "a.ts", None), ty("B", "b.ts", None)];
        let judge = ProjectJudge::new(&g, &records, "/repo".into(), &[]);
        assert_eq!(judge.verdict(0, 1).tag, None);
        assert_eq!(judge.module(0), None);
    }

    #[test]
    fn reads_leading_comments() {
        assert_eq!(
            leading_comment("\n// one\n/* two */\nimport x;\n// later"),
            "// one /* two */"
        );
        assert_eq!(leading_comment("# py\n\"\"\"doc\"\"\""), "# py");
    }
}
