//! What the project says about a duplicate pair of types or functions: its module tag, and
//! whether it's acknowledged as deliberate, in `duped.toml` or by a doc comment on either side.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::analysis::{Judge, Verdict};
use globset::GlobMatcher;

use crate::config::{Acknowledged, file_glob};
use crate::lang::imported_packages;
use crate::modules::{self, Home, ModuleGraph, ROOT_MODULE, Tag};
use crate::record::{Record, qualified};
use crate::walk::{SourceFile, language_for};

/// Doc phrases that mark a copy as deliberate, matched case-insensitively.
const DELIBERATE: &[&str] = &[
    "structural twin",
    "kept in sync",
    "mirror of",
    "mirrors",
    "copy of",
];

/// Doc phrases that declare a copy to maintain: deliberate only while the two still match, so
/// a port that drifted stays reported.
const DECLARED: &[&str] = &[
    "port of",
    "keep in step",
    "keep the two in step",
    "keep them in step",
];

/// `duped.toml`'s `[[acknowledged]]` entries, their file globs compiled.
#[derive(Default)]
pub struct Acks {
    /// Each entry's sides and reason.
    entries: Vec<(Side, Side, String)>,
    /// The scan directory under the project root, `packages/web/` or empty: globs are
    /// relative to the root, scanned files to the scan directory.
    prefix: String,
}

/// One side of an entry: a name, or a file glob covering all that matching files declare.
enum Side {
    Name(String),
    Files(GlobMatcher),
}

impl Side {
    fn new(pattern: &str) -> Self {
        match file_glob(pattern) {
            Some(Ok(glob)) => Side::Files(glob),
            _ => Side::Name(pattern.to_string()),
        }
    }

    fn file(&self, prefix: &str, file: &str) -> bool {
        matches!(self, Side::Files(glob) if glob.is_match(format!("{prefix}{file}")))
    }

    fn matches(&self, prefix: &str, t: &Record, module: &str) -> bool {
        match self {
            Side::Name(pattern) => side_matches(pattern, t, module),
            Side::Files(_) => self.file(prefix, &t.location().file),
        }
    }
}

impl Acks {
    /// `base` is the scan directory, under the project `root`.
    pub fn new(entries: &[Acknowledged], root: &Path, base: &Path) -> Self {
        let mut prefix = modules::relative(root, base);
        if !prefix.is_empty() {
            prefix.push('/');
        }
        let entries = entries
            .iter()
            .map(|e| {
                let reason = e.reason.clone().unwrap_or_else(|| "duped.toml".to_string());
                (Side::new(&e.a), Side::new(&e.b), reason)
            })
            .collect();
        Acks { entries, prefix }
    }

    /// Why the pair is acknowledged, from the first entry matching it either way round.
    fn reason(&self, (ta, ma): (&Record, &str), (tb, mb): (&Record, &str)) -> Option<String> {
        let p = &self.prefix;
        self.entries
            .iter()
            .find(|(a, b, _)| {
                a.matches(p, ta, ma) && b.matches(p, tb, mb)
                    || a.matches(p, tb, mb) && b.matches(p, ta, ma)
            })
            .map(|(_, _, reason)| reason.clone())
    }

    /// Whether an entry with a file glob on both sides covers scanned files `a` and `b`:
    /// every finding between them is deliberate.
    pub fn files(&self, a: &str, b: &str) -> bool {
        let p = &self.prefix;
        self.entries
            .iter()
            .any(|(x, y, _)| x.file(p, a) && y.file(p, b) || x.file(p, b) && y.file(p, a))
    }
}

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
    acknowledged: &'a Acks,
    /// Directory the records' `file` paths are relative to.
    base: PathBuf,
    /// Leading comment of each file, lower-cased, read on first use.
    headers: RefCell<HashMap<String, String>>,
    packages: Packages,
}

/// Packages each file imports, read on first use: what a move-down home must declare.
struct Packages {
    base: PathBuf,
    cache: RefCell<HashMap<String, Vec<String>>>,
}

impl Packages {
    fn new(base: &Path) -> Self {
        Packages {
            base: base.to_path_buf(),
            cache: RefCell::default(),
        }
    }

    /// Packages `sides` (files relative to `base`, with their modules) import that their own
    /// module declares, sorted. An undeclared import, like Node's `events`, is a built-in or
    /// a hoisted package, so a home needn't declare it either.
    fn of(&self, graph: &ModuleGraph, sides: &[(&str, Option<usize>)]) -> Vec<String> {
        let mut all: Vec<String> = Vec::new();
        for &(file, module) in sides {
            let mut cache = self.cache.borrow_mut();
            let packages = cache.entry(file.to_string()).or_insert_with(|| {
                let path = self.base.join(file);
                language_for(&path).map_or_else(Vec::new, |language| {
                    imported_packages(&SourceFile {
                        path,
                        rel: file.to_string(),
                        language,
                    })
                })
            });
            all.extend(
                packages
                    .iter()
                    .filter(|p| module.is_some_and(|m| graph.declares(m, p)))
                    .cloned(),
            );
        }
        all.sort_unstable();
        all.dedup();
        all
    }
}

impl<'a> ProjectJudge<'a> {
    pub fn new(
        graph: &'a ModuleGraph,
        records: &'a [Record],
        base: PathBuf,
        acknowledged: &'a Acks,
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
            packages: Packages::new(&base),
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
        if let Some(reason) = self.acknowledged.reason((ta, ma), (tb, mb)) {
            return Some(reason);
        }
        // The name must follow the phrase in the same sentence: "mirrors `X`", "twin of the
        // evals `X`", not a phrase in one paragraph and a name in another.
        self.doc_phrase(a, b, DELIBERATE, clause_after)
    }

    /// `doc: <phrase>` for the first of `phrases` in either side's doc or file header that
    /// names the other side within `scope` of it.
    fn doc_phrase(
        &self,
        a: usize,
        b: usize,
        phrases: &[&str],
        scope: fn(&str, usize) -> &str,
    ) -> Option<String> {
        let (ta, tb) = (&self.records[a], &self.records[b]);
        let (ma, mb) = (
            self.graph.name(self.modules[a]),
            self.graph.name(self.modules[b]),
        );
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
            // A phrase is a whole word from its start: `port of`, not `support of`.
            let starts_word = |at: usize| {
                !text[..at]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_alphanumeric)
            };
            for phrase in phrases {
                if text
                    .match_indices(phrase)
                    .any(|(at, _)| starts_word(at) && names_other(scope(&text, at)))
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
        let acknowledged = self.acknowledgement(a, b);
        // The name may precede the phrase: "same as `X`; keep the two in step".
        let declared = match acknowledged {
            Some(_) => None,
            None => self.doc_phrase(a, b, DECLARED, sentence_at),
        };
        Verdict {
            tag: self.tag(a, b),
            acknowledged,
            declared,
        }
    }

    fn tag(&self, a: usize, b: usize) -> Option<Tag> {
        self.tagging.then(|| {
            let sides = [a, b].map(|i| (self.records[i].location().file.as_str(), self.modules[i]));
            let needs = self.packages.of(self.graph, &sides);
            self.graph.tag(self.modules[a], self.modules[b], &needs)
        })
    }

    fn module(&self, record: usize) -> Option<&str> {
        self.tagging.then(|| self.graph.name(self.modules[record]))
    }

    fn home(&self, members: &[usize]) -> Option<Home> {
        if !self.tagging {
            return None;
        }
        let modules: Vec<Option<usize>> = members.iter().map(|&m| self.modules[m]).collect();
        let sides: Vec<(&str, Option<usize>)> = members
            .iter()
            .map(|&m| (self.records[m].location().file.as_str(), self.modules[m]))
            .collect();
        self.graph
            .home(&modules, &self.packages.of(self.graph, &sides))
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
    files: Vec<String>,
    packages: Packages,
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
        let files: Vec<String> = files.into_iter().map(str::to_string).collect();
        let modules: Vec<Option<usize>> = files
            .iter()
            .map(|f| graph.module_of(&base.join(f)))
            .collect();
        let spanned: HashSet<Option<usize>> = modules.iter().copied().collect();
        FileJudge {
            graph,
            tagging: spanned.len() >= 2,
            modules,
            files,
            packages: Packages::new(base),
        }
    }
}

impl Judge for FileJudge<'_> {
    fn verdict(&self, a: usize, b: usize) -> Verdict {
        Verdict {
            tag: self.tag(a, b),
            acknowledged: None,
            declared: None,
        }
    }

    fn tag(&self, a: usize, b: usize) -> Option<Tag> {
        self.tagging.then(|| {
            let sides = [a, b].map(|i| (self.files[i].as_str(), self.modules[i]));
            let needs = self.packages.of(self.graph, &sides);
            self.graph.tag(self.modules[a], self.modules[b], &needs)
        })
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

/// The sentence holding `at`, looking back at most 200 bytes: `same as X; keep in step`.
fn sentence_at(text: &str, at: usize) -> &str {
    let mut start = at.saturating_sub(200);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let start = text[start..at]
        .rfind(". ")
        .map_or(start, |stop| start + stop + 2);
    &text[start..at + clause_after(text, at).len()]
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

    fn none() -> &'static Acks {
        Box::leak(Box::default())
    }

    /// Entries for a scan of `/repo`, the project root.
    fn acks(entries: &[(&str, &str, Option<&str>)]) -> Acks {
        let entries: Vec<Acknowledged> = entries
            .iter()
            .map(|&(a, b, reason)| Acknowledged {
                a: a.into(),
                b: b.into(),
                reason: reason.map(str::to_string),
            })
            .collect();
        Acks::new(&entries, Path::new("/repo"), Path::new("/repo"))
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
                external: Vec::new(),
            },
            Module {
                name: "@x/unified-evals".into(),
                root: "/repo/evals".into(),
                deps: vec![],
                aliases: Vec::new(),
                external: Vec::new(),
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
        let listed = acks(&[("unified-evals:Row", "@x/canvas:Row", Some("boundary"))]);
        let judge = ProjectJudge::new(&g, &records, "/repo".into(), &listed, "types", is_type);
        assert_eq!(
            judge.verdict(0, 1).acknowledged.as_deref(),
            Some("boundary")
        );
        assert_eq!(judge.module(0), Some("@x/canvas"));

        let wrong = acks(&[("canvas2:Row", "Row", None)]);
        let judge = ProjectJudge::new(&g, &records, "/repo".into(), &wrong, "types", is_type);
        assert_eq!(judge.verdict(0, 1).acknowledged, None);
    }

    #[test]
    fn a_file_glob_side_covers_everything_its_files_declare() {
        let g = graph();
        let records = vec![
            ty("Task", "sdk/src/types.ts", None),
            ty("Task", "crates/core/src/tasks.rs", None),
            ty("Pane", "crates/core/src/model.rs", None),
            ty("Task", "crates/app/main.rs", None),
        ];
        let reason = |acks: &Acks, a, b| {
            ProjectJudge::new(&g, &records, "/repo".into(), acks, "types", is_type)
                .verdict(a, b)
                .acknowledged
        };
        // Either way round; `*` stays within a directory.
        let mirror = acks(&[("crates/core/src/*.rs", "sdk/src/types.ts", Some("wire"))]);
        assert_eq!(reason(&mirror, 0, 1).as_deref(), Some("wire"));
        assert_eq!(reason(&mirror, 2, 0).as_deref(), Some("wire"));
        assert_eq!(reason(&mirror, 0, 3), None);
        assert_eq!(reason(&mirror, 1, 2), None);
        assert!(mirror.files("sdk/src/types.ts", "crates/core/src/model.rs"));
        assert!(!mirror.files("sdk/src/types.ts", "sdk/src/types.ts"));

        // A file on one side and a name on the other: no whole-file pair.
        let mixed = acks(&[("sdk/src/types.ts", "Pane", None)]);
        assert_eq!(reason(&mixed, 0, 2).as_deref(), Some("duped.toml"));
        assert_eq!(reason(&mixed, 0, 1), None);
        assert!(!mixed.files("sdk/src/types.ts", "crates/core/src/model.rs"));

        // Globs are relative to the project root, scanned files to the scan directory.
        let entry = Acknowledged {
            a: "sdk/src/types.ts".into(),
            b: "crates/**".into(),
            reason: None,
        };
        let scanned = Acks::new(&[entry], Path::new("/repo"), Path::new("/repo/sdk"));
        assert!(!scanned.files("sdk/src/types.ts", "crates/core/src/model.rs"));
        assert_eq!(scanned.prefix, "sdk/");
        assert!(scanned.entries[0].0.file(&scanned.prefix, "src/types.ts"));
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
        let judge = ProjectJudge::new(&g, &records, "/repo".into(), none(), "types", is_type);
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
                external: Vec::new(),
            },
            Module {
                name: "@x/unified-evals".into(),
                root: base.join("evals"),
                deps: vec![],
                aliases: Vec::new(),
                external: Vec::new(),
            },
        ]);
        let judge = ProjectJudge::new(&g, &records, base, none(), "types", is_type);
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
                body_lines: Vec::new(),
            })
        };
        let g = graph();
        // The only type is in canvas; functions span both modules.
        let records = vec![
            ty("T", "canvas/t.ts", None),
            function("load", "canvas/a.ts"),
            function("fetch", "evals/b.ts"),
        ];
        let listed = acks(&[("unified-evals:fetch", "load", None)]);
        let judge = ProjectJudge::new(&g, &records, "/repo".into(), &listed, "functions", |r| {
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
            external: Vec::new(),
        }]);
        let records = vec![ty("A", "a.ts", None), ty("B", "b.ts", None)];
        let judge = ProjectJudge::new(&g, &records, "/repo".into(), none(), "types", is_type);
        assert_eq!(judge.verdict(0, 1).tag, None);
        assert_eq!(judge.module(0), None);
    }

    #[test]
    fn homes_ignore_imports_the_files_own_module_does_not_declare() {
        // `a` and `b` import Node's `events`; only `web` declares it (a polyfill).
        let root = std::env::temp_dir().join(format!("duped-needs-{}", std::process::id()));
        for dir in ["a", "b"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            std::fs::write(
                root.join(dir).join("x.ts"),
                "import { EventEmitter } from 'events';\n",
            )
            .unwrap();
        }
        let module = |name: &str, deps: Vec<usize>, external: Vec<String>| Module {
            name: name.into(),
            root: root.join(name),
            deps,
            aliases: Vec::new(),
            external,
        };
        let g = ModuleGraph::new(vec![
            module("base", vec![], vec![]),
            module("web", vec![0], vec!["events".into()]),
            module("a", vec![0, 1], vec![]),
            module("b", vec![0, 1], vec![]),
        ]);
        let records = vec![ty("A", "a/x.ts", None), ty("B", "b/x.ts", None)];
        let judge = ProjectJudge::new(&g, &records, root.clone(), none(), "types", is_type);
        assert_eq!(
            judge.tag(0, 1),
            Some(Tag::MoveDown {
                home: "base".into(),
                missing: vec![]
            })
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tags_only_when_the_scanned_types_span_two_modules() {
        // Two modules exist, but every scanned type is in one of them.
        let g = graph();
        let records = vec![ty("A", "canvas/a.ts", None), ty("B", "canvas/b.ts", None)];
        let judge = ProjectJudge::new(&g, &records, "/repo".into(), none(), "types", is_type);
        assert_eq!(judge.verdict(0, 1).tag, None);
        assert!(judge.summary().unwrap().contains("fewer than 2"));
    }

    #[test]
    fn doc_mentions_must_be_whole_words_and_name_the_other_side() {
        let g = graph();
        let ack = |doc: &str, a: &str, b: &str| {
            let records = vec![ty(a, "canvas/a.ts", Some(doc)), ty(b, "evals/b.ts", None)];
            ProjectJudge::new(&g, &records, "/repo".into(), none(), "types", is_type)
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
    fn maintained_copy_phrases_declare_without_acknowledging() {
        let g = graph();
        let verdict = |doc: &str, a: &str, b: &str| {
            let records = vec![ty(a, "canvas/a.ts", Some(doc)), ty(b, "evals/b.ts", None)];
            ProjectJudge::new(&g, &records, "/repo".into(), none(), "types", is_type).verdict(0, 1)
        };
        let declared = |doc: &str, a: &str, b: &str| {
            let verdict = verdict(doc, a, b);
            assert_eq!(verdict.acknowledged, None, "{doc}");
            verdict.declared
        };
        assert_eq!(
            declared("Port of evals src/row.ts.", "Row", "Row").as_deref(),
            Some("doc: port of")
        );
        assert_eq!(
            declared("Keep in step with `Bar`.", "Foo", "Bar").as_deref(),
            Some("doc: keep in step")
        );
        // The name may come first, in the same sentence.
        assert_eq!(
            declared("Same shape as `Bar`; keep the two in step.", "Foo", "Bar").as_deref(),
            Some("doc: keep the two in step")
        );
        // `support of` and `export of` are not `port of`.
        assert_eq!(
            declared("Adds support of evals extensions.", "Row", "Row"),
            None
        );
        assert_eq!(declared("The Excel export of `Bar`.", "Foo", "Bar"), None);
        // Names nothing in the scan, or names it in another sentence.
        assert_eq!(declared("Port of BpmnExecution.cs.", "Row", "Row"), None);
        assert_eq!(
            declared(
                "Wraps `Bar`. Keep in step with the wire format.",
                "Foo",
                "Bar"
            ),
            None
        );
        assert_eq!(
            declared("Keep them in step. See `Bar`.", "Foo", "Bar"),
            None
        );
        // A twin phrase acknowledges outright.
        let twin = verdict("Mirrors `Bar`; keep the two in step.", "Foo", "Bar");
        assert_eq!(
            (twin.acknowledged.as_deref(), twin.declared),
            (Some("doc: mirrors"), None)
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
                external: Vec::new(),
            },
            Module {
                name: "e".into(),
                root: base.join("evals"),
                deps: vec![],
                aliases: Vec::new(),
                external: Vec::new(),
            },
        ]);
        let records = vec![
            ty("Foo", "canvas/a.ts", None),
            ty("License", "evals/b.ts", None),
        ];
        let judge = ProjectJudge::new(&g, &records, base, none(), "types", is_type);
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
