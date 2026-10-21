//! API-bypass coupling: imports that reach past a directory's entry file into its internals,
//! weighted by how many boundaries they cross. It measures; it never fails the command.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write;
use std::path::Path;

use rayon::prelude::*;
use serde_json::{Value, json};

use crate::lang::Import;
use crate::modules::{self, ModuleGraph};
use crate::record::{Language, Record};
use crate::walk::SourceFile;

const SCRIPT: &[&str] = &[".ts", ".tsx", ".js", ".jsx", ".mts", ".cts", ".mjs", ".cjs"];
const MANIFESTS: &[&str] = &["package.json", "Cargo.toml", "pyproject.toml", "setup.py"];
/// Boundaries declaring fewer symbols than this count as fully hiding.
const HIDING_FLOOR: usize = 5;

/// One import edge from a scanned file to another, after following re-exports.
#[derive(Debug)]
pub struct Edge {
    pub importer: String,
    pub target: String,
    /// The entry file the importer named, when it re-exports from outside its own directory.
    pub via: Option<String>,
    /// Imported names; empty for a whole-module or side-effect import.
    pub names: Vec<String>,
    /// Boundary directories exited on the way up to the common ancestor.
    pub ups: usize,
    /// Boundary directories entered on the way down to the target.
    pub downs: usize,
    /// Boundaries entered after the first, plus 1 when the target isn't its boundary's entry.
    pub bypass: usize,
    /// Boundary directories whose entry the edge goes past.
    bypassed: Vec<String>,
}

impl Edge {
    /// One symbol per name; a nameless import is one symbol named after its target.
    fn symbols(&self) -> Vec<&str> {
        if self.names.is_empty() {
            vec![self.target.as_str()]
        } else {
            self.names.iter().map(String::as_str).collect()
        }
    }
}

/// A directory with an entry file, a manifest, or the name `internal`.
#[derive(Debug)]
pub struct Boundary {
    /// Relative to the scanned directory; empty for the directory itself.
    pub dir: String,
    pub entries: Vec<String>,
    /// Names exported by the entry files, including one-hop re-exports.
    pub surface: Vec<String>,
    /// Surface names no file outside the boundary imports.
    pub unused: Vec<String>,
    /// Top-level declarations in files whose innermost boundary this is.
    pub declared: usize,
    /// Declarations neither on an entry's surface nor imported from outside the boundary.
    pub hidden: usize,
    /// Imported symbols from outside that land inside, and how many go past the entry.
    pub incoming: usize,
    pub bypass_in: usize,
    pub leaks: usize,
}

impl Boundary {
    pub fn bypass_rate(&self) -> f64 {
        ratio(self.bypass_in, self.incoming)
    }
    pub fn unused_rate(&self) -> f64 {
        ratio(self.unused.len(), self.surface.len())
    }
    /// Hidden share of declarations; 1 below [`HIDING_FLOOR`] declarations.
    pub fn hiding(&self) -> f64 {
        if self.declared < HIDING_FLOOR {
            1.0
        } else {
            ratio(self.hidden, self.declared)
        }
    }
    /// Nothing outside imports it (an app root, a published package): usage is unknown.
    pub fn no_consumers(&self) -> bool {
        self.incoming == 0
    }
    /// Mean of `1 − bypassRate`, `1 − unusedSurface` and hiding; higher is better.
    /// Without consumers, unused surface says nothing, so its term is 1.
    pub fn score(&self) -> f64 {
        let unused = if self.no_consumers() {
            0.0
        } else {
            self.unused_rate()
        };
        (2.0 - self.bypass_rate() - unused + self.hiding()) / 3.0
    }
}

/// A declaration in a non-entry file, imported from outside its boundary without the entry.
#[derive(Debug)]
pub struct Leak {
    pub file: String,
    pub symbol: String,
    pub boundary: String,
    pub importers: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Totals {
    pub files: usize,
    pub edges: usize,
    pub cross_edges: usize,
    pub bypass_edges: usize,
    /// Σ bypass over edges.
    pub bypass_depth: usize,
    pub leaks: usize,
    /// Imported symbols: one per (importer, target, name).
    pub symbols: usize,
    /// Symbols whose edge enters a boundary, and those of them that bypass an entry.
    pub cross_symbols: usize,
    pub bypass_symbols: usize,
    /// Σ bypass and Σ bypass² over symbols.
    pub symbol_depth: usize,
    pub bypass_cost: usize,
    /// Entry exports of boundaries with consumers, and those no outside file imports.
    pub surface: usize,
    pub unused_surface: usize,
    /// Declarations in all boundaries, and those hidden.
    pub declared: usize,
    pub hidden: usize,
}

impl Totals {
    /// Share of cross-boundary symbols that bypass an entry, 0–1.
    pub fn bypass_rate(&self) -> f64 {
        ratio(self.bypass_symbols, self.cross_symbols)
    }
    pub fn mean_bypass_depth(&self) -> f64 {
        ratio(self.symbol_depth, self.bypass_symbols)
    }
    pub fn unused_surface_rate(&self) -> f64 {
        ratio(self.unused_surface, self.surface)
    }
}

/// Hidden share of declarations; 1 when there are none.
fn hiding_of(hidden: usize, declared: usize) -> f64 {
    if declared == 0 {
        1.0
    } else {
        ratio(hidden, declared)
    }
}

fn ratio(a: usize, b: usize) -> f64 {
    if b == 0 { 0.0 } else { a as f64 / b as f64 }
}

#[derive(Debug, Default)]
pub struct Coupling {
    pub totals: Totals,
    /// Every resolved edge, most bypass first.
    pub edges: Vec<Edge>,
    pub boundaries: Vec<Boundary>,
    pub leaks: Vec<Leak>,
    /// Line counts per scanned file, for `--base`.
    lines: HashMap<String, usize>,
    /// Top-level declarations per file, with their line spans.
    declared: HashMap<String, HashMap<String, usize>>,
}

impl Coupling {
    /// `(bypassRate, unusedSurface, hiding)` over the boundaries under `folder`, or the
    /// whole scan when empty, where bypassRate is over cross-boundary symbols.
    pub fn ratios(&self, folder: &str) -> (f64, f64, f64) {
        if folder.is_empty() {
            let t = &self.totals;
            return (
                t.bypass_rate(),
                t.unused_surface_rate(),
                hiding_of(t.hidden, t.declared),
            );
        }
        let (mut incoming, mut bypass, mut surface, mut unused, mut declared, mut hidden) =
            (0, 0, 0, 0, 0, 0);
        for b in self
            .boundaries
            .iter()
            .filter(|b| b.dir == folder || within(&b.dir, folder))
        {
            incoming += b.incoming;
            bypass += b.bypass_in;
            if !b.no_consumers() {
                surface += b.surface.len();
                unused += b.unused.len();
            }
            declared += b.declared;
            hidden += b.hidden;
        }
        (
            ratio(bypass, incoming),
            ratio(unused, surface),
            hiding_of(hidden, declared),
        )
    }

    /// Mean of `1 − bypassRate`, `1 − unusedSurface` and hiding, from [`Self::ratios`].
    pub fn score(&self, folder: &str) -> f64 {
        let (bypass, unused, hiding) = self.ratios(folder);
        (2.0 - bypass - unused + hiding) / 3.0
    }

    /// `(importer, symbol)` → its target and bypass, the deepest when imported twice.
    fn symbol_map(&self) -> HashMap<(&str, &str), (&str, usize)> {
        let mut map: HashMap<(&str, &str), (&str, usize)> = HashMap::new();
        for e in &self.edges {
            for name in e.symbols() {
                let slot = map
                    .entry((e.importer.as_str(), name))
                    .or_insert((e.target.as_str(), e.bypass));
                if e.bypass > slot.1 {
                    *slot = (e.target.as_str(), e.bypass);
                }
            }
        }
        map
    }
}

/// Measure every import edge of `files` (relative to `base`) between scanned files.
pub fn analyze(
    base: &Path,
    graph: &ModuleGraph,
    files: &[SourceFile],
    imports: Vec<(String, Vec<Import>)>,
    records: &[Record],
) -> Coupling {
    let files: Vec<&SourceFile> = files
        .iter()
        .filter(|f| f.language != Language::CSharp)
        .collect();
    let language: HashMap<&str, Language> =
        files.iter().map(|f| (f.rel.as_str(), f.language)).collect();
    let lines: HashMap<String, usize> = files
        .par_iter()
        .filter_map(|f| {
            let bytes = std::fs::read(&f.path).ok()?;
            let newlines = bytes.iter().filter(|&&b| b == b'\n').count();
            let last = usize::from(bytes.last().is_some_and(|&b| b != b'\n'));
            Some((f.rel.clone(), newlines + last))
        })
        .collect();
    let mut declared: HashMap<String, HashMap<String, usize>> = HashMap::new();
    let mut exported: HashMap<&str, HashSet<&str>> = HashMap::new();
    for r in records.iter().filter(|r| r.scope().is_none()) {
        let loc = r.location();
        if !language.contains_key(loc.file.as_str()) {
            continue;
        }
        let span = loc.end_line + 1 - loc.start_line;
        declared
            .entry(loc.file.clone())
            .or_default()
            .insert(r.name().to_string(), span);
        let public = match r {
            Record::Function(f) => f.exported,
            Record::Type(t) => t.exported,
        };
        if public {
            exported.entry(&loc.file).or_default().insert(r.name());
        }
    }
    let exports = |file: &str, name: &str| exported.get(file).is_some_and(|s| s.contains(name));

    let tree = Tree::new(base, graph, files.iter().map(|f| f.rel.clone()).collect());
    let lang = |file: &str| language.get(file).copied().unwrap_or(Language::TypeScript);

    // Entry files' re-exports: (exported name, real file, its name there).
    let entries: HashSet<&str> = tree
        .boundaries
        .values()
        .flatten()
        .map(String::as_str)
        .collect();
    let mut reexports: HashMap<String, Vec<(String, String, String)>> = HashMap::new();
    for (file, list) in imports.iter().filter(|(f, _)| entries.contains(f.as_str())) {
        for import in list {
            for (name, outward) in import.names.iter().zip(&import.reexported) {
                let single = Import {
                    specifier: import.specifier.clone(),
                    names: vec![name.clone()],
                    reexported: Vec::new(),
                };
                let Some((target, names)) =
                    tree.resolve(file, lang(file), &single).into_iter().next()
                else {
                    continue;
                };
                if target != *file {
                    let original = names.first().unwrap_or(name).clone();
                    reexports.entry(file.clone()).or_default().push((
                        outward.clone(),
                        target,
                        original,
                    ));
                }
            }
        }
    }
    let hop = |entry: &str, name: &str| -> Option<(String, String)> {
        let list = reexports.get(entry)?;
        list.iter()
            .find(|(outward, ..)| outward == name)
            .map(|(_, t, o)| (t.clone(), o.clone()))
            .or_else(|| {
                list.iter()
                    .find(|(outward, t, _)| outward == "*" && exports(t, name))
                    .map(|(_, t, _)| (t.clone(), name.to_string()))
            })
    };

    // Resolve, follow re-exports, and note which declarations are used from outside.
    let mut merged: BTreeMap<(String, String), (Option<String>, BTreeSet<String>)> =
        BTreeMap::new();
    let mut used: HashSet<(String, String)> = HashSet::new();
    let mut used_all: HashSet<String> = HashSet::new();
    let mut mark = |tree: &Tree, importer: &str, file: &str, name: &str| {
        if tree.innermost(file).is_some_and(|b| !within(importer, &b)) {
            if name == "*" || name.is_empty() {
                used_all.insert(file.to_string());
            } else {
                used.insert((file.to_string(), name.to_string()));
            }
        }
    };
    for (file, list) in &imports {
        for import in list {
            for (target, names) in tree.resolve(file, lang(file), import) {
                if target == *file {
                    continue;
                }
                let mut stay = Vec::new();
                for name in &names {
                    mark(&tree, file, &target, name);
                    if let Some((real, original)) = hop(&target, name) {
                        mark(&tree, file, &real, &original);
                        if !within(&real, dir_of(&target)) && real != *file {
                            let edge = merged.entry((file.clone(), real)).or_default();
                            edge.0 = Some(target.clone());
                            edge.1.insert(original);
                            continue;
                        }
                    }
                    stay.push(name.clone());
                }
                if names.is_empty() {
                    mark(&tree, file, &target, "");
                }
                if !stay.is_empty() || names.is_empty() {
                    merged
                        .entry((file.clone(), target))
                        .or_default()
                        .1
                        .extend(stay);
                }
            }
        }
    }

    let mut totals = Totals {
        files: files.len(),
        ..Totals::default()
    };
    let mut incoming: HashMap<String, (usize, usize)> = HashMap::new();
    let mut leaks: BTreeMap<(String, String), (String, Vec<String>)> = BTreeMap::new();
    let mut edges = Vec::new();
    for ((importer, target), (via, names)) in merged {
        let (ups, downs, bypass, bypassed) = tree.score(&importer, &target);
        let edge = Edge {
            importer,
            target,
            via,
            names: names.into_iter().collect(),
            ups,
            downs,
            bypass,
            bypassed,
        };
        let symbols = edge.symbols().len();
        totals.symbols += symbols;
        if downs > 0 {
            totals.cross_edges += 1;
            totals.cross_symbols += symbols;
        }
        if bypass > 0 {
            totals.bypass_edges += 1;
            totals.bypass_depth += bypass;
            totals.bypass_symbols += symbols;
            totals.symbol_depth += symbols * bypass;
            totals.bypass_cost += symbols * bypass * bypass;
        }
        // Every boundary the edge enters from outside counts it as incoming.
        let mut dir = Some(dir_of(&edge.target));
        while let Some(d) = dir {
            if tree.counted(d) && !within(&edge.importer, d) {
                let slot = incoming.entry(d.to_string()).or_default();
                slot.0 += symbols;
                if edge.bypassed.iter().any(|b| b == d) {
                    slot.1 += symbols;
                }
            }
            dir = parent(d);
        }
        if let Some(boundary) = tree.innermost(&edge.target)
            && !tree.boundaries[&boundary].contains(&edge.target)
            && !within(&edge.importer, &boundary)
        {
            for name in &edge.names {
                if declared
                    .get(&edge.target)
                    .is_some_and(|d| d.contains_key(name))
                {
                    let leak = leaks
                        .entry((edge.target.clone(), name.clone()))
                        .or_insert_with(|| (boundary.clone(), Vec::new()));
                    leak.1.push(edge.importer.clone());
                }
            }
        }
        edges.push(edge);
    }
    totals.edges = edges.len();
    edges.sort_by(|a, b| {
        (b.bypass, b.downs, b.ups)
            .cmp(&(a.bypass, a.downs, a.ups))
            .then_with(|| (&a.importer, &a.target).cmp(&(&b.importer, &b.target)))
    });
    let leaks: Vec<Leak> = leaks
        .into_iter()
        .map(|((file, symbol), (boundary, importers))| Leak {
            file,
            symbol,
            boundary,
            importers,
        })
        .collect();
    totals.leaks = leaks.len();

    // Each counted boundary's own declarations, by innermost boundary.
    let mut owned: HashMap<String, Vec<(&str, &str)>> = HashMap::new();
    for (file, names) in &declared {
        if let Some(b) = tree.innermost(file) {
            owned
                .entry(b)
                .or_default()
                .extend(names.keys().map(|n| (file.as_str(), n.as_str())));
        }
    }
    let is_used = |file: &str, name: &str| {
        used_all.contains(file) || used.contains(&(file.to_string(), name.to_string()))
    };
    // Declarations on any entry's surface, directly or by one-hop re-export.
    let mut surfaced: HashSet<(&str, &str)> = HashSet::new();
    for entry in tree.boundaries.values().flatten() {
        let own = exported.get(entry.as_str()).into_iter().flatten();
        surfaced.extend(own.map(|n| (entry.as_str(), *n)));
        for (outward, target, original) in reexports.get(entry).into_iter().flatten() {
            if outward == "*" {
                let all = exported.get(target.as_str()).into_iter().flatten();
                surfaced.extend(all.map(|n| (target.as_str(), *n)));
            } else {
                surfaced.insert((target.as_str(), original.as_str()));
            }
        }
    }
    let mut boundaries = Vec::new();
    for (dir, entry_files) in &tree.boundaries {
        if tree.passthrough.contains(dir) {
            continue;
        }
        let mut surface: BTreeSet<String> = BTreeSet::new();
        let mut unused = BTreeSet::new();
        for entry in entry_files {
            let mut names: Vec<String> = exported
                .get(entry.as_str())
                .map(|s| s.iter().map(|n| n.to_string()).collect())
                .unwrap_or_default();
            for (outward, target, _) in reexports.get(entry).into_iter().flatten() {
                if outward == "*" {
                    let all = exported.get(target.as_str()).into_iter().flatten();
                    names.extend(all.map(|n| n.to_string()));
                } else {
                    names.push(outward.clone());
                }
            }
            for name in names {
                if !is_used(entry, &name) {
                    unused.insert(name.clone());
                }
                surface.insert(name);
            }
        }
        // A name re-exported by two entries is used if either is.
        let unused: Vec<String> = unused
            .into_iter()
            .filter(|n| !entry_files.iter().any(|e| is_used(e, n)))
            .collect();
        let own = owned.get(dir).map(Vec::as_slice).unwrap_or_default();
        let (incoming, bypass_in) = incoming.get(dir).copied().unwrap_or_default();
        let hidden = own
            .iter()
            .filter(|(f, n)| !is_used(f, n) && !surfaced.contains(&(*f, *n)))
            .count();
        if incoming > 0 {
            totals.surface += surface.len();
            totals.unused_surface += unused.len();
        }
        totals.declared += own.len();
        totals.hidden += hidden;
        boundaries.push(Boundary {
            dir: dir.clone(),
            entries: entry_files.clone(),
            surface: surface.into_iter().collect(),
            unused,
            declared: own.len(),
            hidden,
            incoming,
            bypass_in,
            leaks: leaks.iter().filter(|l| l.boundary == *dir).count(),
        });
    }
    Coupling {
        totals,
        edges,
        boundaries,
        leaks,
        lines,
        declared,
    }
}

// ----- the scanned tree -----

/// Scanned files, their boundaries, and how each language's imports resolve to them.
struct Tree<'a> {
    base: &'a Path,
    graph: &'a ModuleGraph,
    files: HashSet<String>,
    /// Manifest file names per directory.
    manifests: HashMap<String, Vec<String>>,
    /// Boundary directory → its entry files.
    boundaries: BTreeMap<String, Vec<String>>,
    /// Directories between a manifest and its entry file, e.g. a package's `src/`: part of
    /// the manifest's boundary, not boundaries of their own.
    passthrough: HashSet<String>,
    /// Where absolute Python imports start.
    python_roots: Vec<String>,
}

impl<'a> Tree<'a> {
    fn new(base: &'a Path, graph: &'a ModuleGraph, files: HashSet<String>) -> Self {
        let mut dirs: BTreeSet<String> = BTreeSet::new();
        for file in &files {
            let mut dir = Some(dir_of(file));
            while let Some(d) = dir {
                if !dirs.insert(d.to_string()) {
                    break;
                }
                dir = parent(d);
            }
        }
        let manifests: HashMap<String, Vec<String>> = dirs
            .iter()
            .filter_map(|d| {
                let names: Vec<String> = std::fs::read_dir(base.join(d))
                    .ok()?
                    .flatten()
                    .filter_map(|e| e.file_name().into_string().ok())
                    .filter(|n| MANIFESTS.contains(&n.as_str()) || n.ends_with(".csproj"))
                    .collect();
                (!names.is_empty()).then(|| (d.clone(), names))
            })
            .collect();
        // The scan root, Python projects and their `src/`, and the parents of top-level packages.
        let inits: BTreeSet<&str> = files
            .iter()
            .filter(|f| name_of(f) == "__init__.py")
            .map(|f| dir_of(f))
            .collect();
        let mut roots: BTreeSet<String> = BTreeSet::from([String::new()]);
        for (dir, names) in &manifests {
            if names
                .iter()
                .any(|n| n == "pyproject.toml" || n == "setup.py")
            {
                roots.insert(dir.clone());
                roots.insert(join(dir, "src"));
            }
        }
        for package in &inits {
            if let Some(p) = parent(package).filter(|p| !inits.contains(p)) {
                roots.insert(p.to_string());
            }
        }
        let mut tree = Tree {
            base,
            graph,
            files,
            manifests,
            boundaries: BTreeMap::new(),
            passthrough: HashSet::new(),
            python_roots: roots.into_iter().collect(),
        };
        let mut by_dir: HashMap<&str, Vec<String>> = HashMap::new();
        for file in tree.files.iter().filter(|f| is_entry_name(name_of(f))) {
            by_dir.entry(dir_of(file)).or_default().push(file.clone());
        }
        for dir in &dirs {
            let mut entries = by_dir.get(dir.as_str()).cloned().unwrap_or_default();
            // Rust 2018: `a.rs` is the entry of `a/`.
            let module = format!("{dir}.rs");
            if !dir.is_empty() && tree.files.contains(&module) {
                entries.push(module);
            }
            let manifest = tree.manifests.contains_key(dir);
            if entries.is_empty() && !manifest && name_of(dir) != "internal" {
                continue;
            }
            if manifest && let Some(main) = tree.manifest_entry(dir) {
                let mut d = dir_of(&main).to_string();
                while within(&d, dir) {
                    entries.extend(by_dir.get(d.as_str()).into_iter().flatten().cloned());
                    tree.passthrough.insert(d.clone());
                    d = parent(&d).unwrap_or_default().to_string();
                }
                entries.push(main);
            }
            if tree.has_manifest(dir, "package.json") {
                entries.extend(tree.export_entries(dir));
            }
            entries.sort();
            entries.dedup();
            tree.boundaries.insert(dir.clone(), entries);
        }
        tree
    }

    fn counted(&self, dir: &str) -> bool {
        self.boundaries.contains_key(dir) && !self.passthrough.contains(dir)
    }

    /// The deepest counted boundary containing `file`.
    fn innermost(&self, file: &str) -> Option<String> {
        let mut dir = Some(dir_of(file));
        while let Some(d) = dir {
            if self.counted(d) {
                return Some(d.to_string());
            }
            dir = parent(d);
        }
        None
    }

    /// `(ups, downs, bypass, boundaries bypassed)` for an edge from `importer` to `target`.
    fn score(&self, importer: &str, target: &str) -> (usize, usize, usize, Vec<String>) {
        let (from, to) = (dir_of(importer), dir_of(target));
        let common = common_dir(from, to);
        let ups = up_to(from, &common).filter(|d| self.counted(d)).count();
        let mut entered: Vec<String> = up_to(to, &common)
            .filter(|d| self.counted(d))
            .map(String::from)
            .collect();
        entered.reverse();
        // A package `exports` target is the package's entry, whatever lies between.
        if let Some(door) = entered
            .iter()
            .position(|b| self.boundaries[b].iter().any(|e| e == target))
        {
            entered.truncate(door + 1);
        }
        let Some(last) = entered.last() else {
            return (ups, 0, 0, Vec::new());
        };
        // A Python `_private` module is one more boundary, with no entry of its own.
        let private = is_private_python(target);
        let at_entry = !private && self.boundaries[last].iter().any(|e| e == target);
        let downs = entered.len() + usize::from(private);
        let bypass = downs - 1 + usize::from(!at_entry);
        if at_entry {
            entered.pop();
        }
        (ups, downs, bypass, entered)
    }

    /// Each scanned file `import` reaches, with the names it imports from it.
    fn resolve(
        &self,
        importer: &str,
        language: Language,
        import: &Import,
    ) -> Vec<(String, Vec<String>)> {
        match language {
            Language::Python => self.resolve_python(importer, import),
            Language::Rust => self.resolve_rust(importer, import).into_iter().collect(),
            _ => {
                let spec = &import.specifier;
                let target = if spec.starts_with('.') {
                    normalize(dir_of(importer), spec).and_then(|p| self.script_file(&p))
                } else {
                    self.package(spec)
                };
                target
                    .map(|t| (t, import.names.clone()))
                    .into_iter()
                    .collect()
            }
        }
    }

    // ----- TypeScript / JavaScript -----

    /// `path` as written, or with a script extension, or its `index`.
    fn script_file(&self, path: &str) -> Option<String> {
        if self.files.contains(path) {
            return Some(path.to_string());
        }
        let stem = strip_script(path);
        SCRIPT
            .iter()
            .map(|ext| format!("{stem}{ext}"))
            .chain(SCRIPT.iter().map(|ext| join(path, &format!("index{ext}"))))
            .find(|f| self.files.contains(f))
    }

    /// A workspace package name, plus any subpath, to the file it names.
    fn package(&self, spec: &str) -> Option<String> {
        let (name, dir) = self
            .graph
            .modules
            .iter()
            .filter(|m| m.root.starts_with(self.base))
            .flat_map(|m| {
                std::iter::once(&m.name)
                    .chain(&m.aliases)
                    .map(move |n| (n, m))
            })
            .filter(|(n, _)| {
                spec == n.as_str()
                    || spec
                        .strip_prefix(n.as_str())
                        .is_some_and(|r| r.starts_with('/'))
            })
            .map(|(n, m)| (n, modules::relative(self.base, &m.root)))
            .filter(|(_, dir)| self.has_manifest(dir, "package.json"))
            .max_by_key(|(n, _)| n.len())?;
        let sub = spec[name.len()..].trim_start_matches('/');
        if sub.is_empty() {
            return self.package_entry(&dir);
        }
        let exported = self.package_json(&dir).and_then(|json| {
            let exports = json.get("exports")?.as_object()?;
            let key = format!("./{sub}");
            // `./*` patterns substitute the subpath into their target.
            let (value, star) = exports.get(&key).map(|v| (v, None)).or_else(|| {
                exports.iter().find_map(|(k, v)| {
                    let (prefix, suffix) = k.split_once('*')?;
                    let star = key.strip_prefix(prefix)?.strip_suffix(suffix)?;
                    Some((v, Some(star.to_string())))
                })
            })?;
            let path = first_string(value)?;
            Some(match star {
                Some(star) => path.replace('*', &star),
                None => path.to_string(),
            })
        });
        exported
            .and_then(|e| self.export_file(&dir, &e))
            .or_else(|| self.script_file(&join(&dir, sub)))
            .or_else(|| self.script_file(&join(&dir, &format!("src/{sub}"))))
    }

    fn package_json(&self, dir: &str) -> Option<Value> {
        let bytes = std::fs::read(self.base.join(dir).join("package.json")).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// `exports["."]`, `main`, `module` or `types`, else `index` or `src/index`.
    fn package_entry(&self, dir: &str) -> Option<String> {
        let json = self.package_json(dir).unwrap_or_default();
        let exports = json.get("exports").map(|e| e.get(".").unwrap_or(e));
        let declared = [
            exports,
            json.get("main"),
            json.get("module"),
            json.get("types"),
        ]
        .into_iter()
        .flatten()
        .filter_map(first_string);
        declared
            .filter_map(|path| self.export_file(dir, path))
            .next()
            .or_else(|| self.script_file(&join(dir, "index")))
            .or_else(|| self.script_file(&join(dir, "src/index")))
    }

    /// Every file `main`, `module`, `types` or an `exports` path (`*` patterns too) names.
    fn export_entries(&self, dir: &str) -> Vec<String> {
        let Some(json) = self.package_json(dir) else {
            return Vec::new();
        };
        let mut paths = Vec::new();
        for key in ["main", "module", "types", "exports"] {
            if let Some(value) = json.get(key) {
                all_strings(value, &mut paths);
            }
        }
        let mut found = Vec::new();
        for path in paths {
            if path.contains('*') {
                found.extend(self.export_pattern(dir, path));
            } else {
                found.extend(self.export_file(dir, path));
            }
        }
        found
    }

    /// A package path to a scanned file, trying `src/` for a missing `dist/` one:
    /// `./dist/a/index.js` (or `.d.ts`) → `src/a/index.ts`.
    fn export_file(&self, dir: &str, path: &str) -> Option<String> {
        let path = normalize(dir, path)?;
        let path = strip_declaration(&path);
        self.script_file(path)
            .or_else(|| self.script_file(&dist_to_src(dir, path)?))
    }

    /// Scanned files under `dir` an `exports` pattern such as `./dist/*/index.js` matches,
    /// as written or mapped from `dist/` to `src/`.
    fn export_pattern(&self, dir: &str, pattern: &str) -> Vec<String> {
        let Some(path) = normalize(dir, pattern) else {
            return Vec::new();
        };
        let stem = strip_script(strip_declaration(&path)).to_string();
        let mut patterns = vec![stem.clone()];
        patterns.extend(dist_to_src(dir, &stem));
        let mut found = Vec::new();
        for pattern in patterns {
            let Some((prefix, suffix)) = pattern.split_once('*') else {
                continue;
            };
            found.extend(
                self.files
                    .iter()
                    .filter(|f| within(f, dir) && strip_script(f).len() < f.len())
                    .filter(|f| {
                        let stem = strip_script(f);
                        stem.len() > prefix.len() + suffix.len()
                            && stem.starts_with(prefix)
                            && stem.ends_with(suffix)
                    })
                    .cloned(),
            );
        }
        found
    }

    fn has_manifest(&self, dir: &str, name: &str) -> bool {
        self.manifests
            .get(dir)
            .is_some_and(|m| m.iter().any(|n| n == name))
    }

    /// The file a manifest directory's importers reach by its name.
    fn manifest_entry(&self, dir: &str) -> Option<String> {
        if self.has_manifest(dir, "package.json")
            && let Some(entry) = self.package_entry(dir)
        {
            return Some(entry);
        }
        if self.has_manifest(dir, "Cargo.toml") {
            let src = join(dir, "src");
            if let Some(root) = ["lib.rs", "main.rs"]
                .iter()
                .map(|f| join(&src, f))
                .find(|f| self.files.contains(f))
            {
                return Some(root);
            }
        }
        // A Python project with one top-level package, in its directory or `src/`.
        let packages: Vec<String> = [dir.to_string(), join(dir, "src")]
            .iter()
            .flat_map(|root| {
                self.files
                    .iter()
                    .filter(move |f| {
                        name_of(f) == "__init__.py" && parent(dir_of(f)) == Some(root.as_str())
                    })
                    .cloned()
            })
            .collect();
        match packages.as_slice() {
            [one]
                if self.has_manifest(dir, "pyproject.toml")
                    || self.has_manifest(dir, "setup.py") =>
            {
                Some(one.clone())
            }
            _ => None,
        }
    }

    // ----- Python -----

    /// `from a.b import c` reaches module `a/b/c` when there is one, else `c` in `a/b`.
    fn resolve_python(&self, importer: &str, import: &Import) -> Vec<(String, Vec<String>)> {
        let spec = &import.specifier;
        let level = spec.chars().take_while(|&c| c == '.').count();
        let path: Vec<&str> = spec[level..].split('.').filter(|s| !s.is_empty()).collect();
        let roots = if level > 0 {
            let mut dir = Some(dir_of(importer));
            for _ in 1..level {
                dir = dir.and_then(parent);
            }
            dir.map(|d| vec![d.to_string()]).unwrap_or_default()
        } else {
            self.python_roots.clone()
        };
        for root in roots {
            let module = self.python_module(&root, &path);
            if import.names.is_empty() {
                if let Some(m) = module {
                    return vec![(m, Vec::new())];
                }
                continue;
            }
            let mut found = Vec::new();
            for name in &import.names {
                let mut sub = path.clone();
                sub.push(name);
                match self.python_module(&root, &sub).filter(|_| name != "*") {
                    Some(m) => found.push((m, Vec::new())),
                    None => found.extend(module.clone().map(|m| (m, vec![name.clone()]))),
                }
            }
            if !found.is_empty() {
                return found;
            }
        }
        Vec::new()
    }

    fn python_module(&self, root: &str, path: &[&str]) -> Option<String> {
        let dir = join(root, &path.join("/"));
        let candidates = if path.is_empty() {
            vec![join(&dir, "__init__.py")]
        } else {
            vec![format!("{dir}.py"), join(&dir, "__init__.py")]
        };
        candidates.into_iter().find(|f| self.files.contains(f))
    }

    // ----- Rust -----

    /// A `use` path to the deepest module file it names; the next segment is the symbol.
    fn resolve_rust(&self, importer: &str, import: &Import) -> Option<(String, Vec<String>)> {
        let segments: Vec<&str> = import
            .specifier
            .split("::")
            .filter(|s| !s.is_empty())
            .chain(import.names.iter().map(String::as_str))
            .collect();
        let supers = segments.iter().take_while(|s| **s == "super").count();
        // Paths not starting at a crate root are relative to the importer's module (2018).
        let (mut dir, skip, relative) = match *segments.first()? {
            "crate" => (self.crate_dir(importer)?, 1, false),
            "self" => (module_dir(importer), 1, false),
            "super" => {
                let mut dir = module_dir(importer);
                for _ in 0..supers {
                    dir = parent(&dir)?.to_string();
                }
                (dir, supers, false)
            }
            first => match self.crate_named(first) {
                Some(dir) => (dir, 1, false),
                None => (module_dir(importer), 0, true),
            },
        };
        let mut target = self.module_file(&dir)?;
        let mut names = Vec::new();
        let mut descended = false;
        for segment in &segments[skip..] {
            let child = [
                join(&dir, &format!("{segment}.rs")),
                join(&dir, &format!("{segment}/mod.rs")),
            ]
            .into_iter()
            .find(|f| self.files.contains(f));
            match child.filter(|_| *segment != "*") {
                Some(file) => {
                    target = file;
                    dir = join(&dir, segment);
                    descended = true;
                }
                None => {
                    names.push(segment.to_string());
                    break;
                }
            }
        }
        // An unknown first segment that isn't a child module is an external crate.
        (!relative || descended).then_some((target, names))
    }

    /// The file declaring the module whose children live in `dir`.
    fn module_file(&self, dir: &str) -> Option<String> {
        [
            join(dir, "mod.rs"),
            format!("{dir}.rs"),
            join(dir, "lib.rs"),
            join(dir, "main.rs"),
        ]
        .into_iter()
        .find(|f| self.files.contains(f))
    }

    /// `src/` of the nearest crate containing `file`.
    fn crate_dir(&self, file: &str) -> Option<String> {
        let mut dir = Some(dir_of(file));
        while let Some(d) = dir {
            if self.has_manifest(d, "Cargo.toml") {
                return Some(join(d, "src"));
            }
            dir = parent(d);
        }
        None
    }

    /// `src/` of the workspace crate `name` (with `-` read as `_`).
    fn crate_named(&self, name: &str) -> Option<String> {
        self.graph
            .modules
            .iter()
            .filter(|m| m.root.starts_with(self.base))
            .filter(|m| {
                std::iter::once(&m.name)
                    .chain(&m.aliases)
                    .any(|n| n.replace('-', "_") == name)
            })
            .map(|m| modules::relative(self.base, &m.root))
            .find(|dir| self.has_manifest(dir, "Cargo.toml"))
            .map(|dir| join(&dir, "src"))
            .filter(|src| self.module_file(src).is_some())
    }
}

// ----- paths: `/`-separated, relative to the scanned directory, `""` for it -----

fn dir_of(file: &str) -> &str {
    file.rsplit_once('/').map_or("", |(dir, _)| dir)
}

fn name_of(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

fn parent(dir: &str) -> Option<&str> {
    (!dir.is_empty()).then(|| dir_of(dir))
}

fn join(dir: &str, name: &str) -> String {
    match (dir.is_empty(), name.is_empty()) {
        (true, _) => name.to_string(),
        (_, true) => dir.to_string(),
        _ => format!("{dir}/{name}"),
    }
}

/// `path` is strictly inside `dir`.
fn within(path: &str, dir: &str) -> bool {
    path != dir
        && (dir.is_empty()
            || path
                .strip_prefix(dir)
                .is_some_and(|rest| rest.starts_with('/')))
}

/// `a/b` + `../c/./d` → `a/c/d`; `None` when it leaves the scanned directory.
fn normalize(dir: &str, path: &str) -> Option<String> {
    let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

fn common_dir(a: &str, b: &str) -> String {
    let a = a.split('/').filter(|s| !s.is_empty());
    let b = b.split('/').filter(|s| !s.is_empty());
    a.zip(b)
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x)
        .collect::<Vec<_>>()
        .join("/")
}

/// `dir` and its ancestors, stopping before `stop`.
fn up_to<'d>(dir: &'d str, stop: &'d str) -> impl Iterator<Item = &'d str> {
    std::iter::successors(Some(dir), |d| parent(d)).take_while(move |d| *d != stop)
}

fn is_entry_name(name: &str) -> bool {
    matches!(name, "__init__.py" | "mod.rs" | "lib.rs" | "main.rs")
        || name
            .strip_prefix("index")
            .is_some_and(|ext| SCRIPT.contains(&ext))
}

fn strip_script(path: &str) -> &str {
    SCRIPT
        .iter()
        .find_map(|ext| path.strip_suffix(ext))
        .unwrap_or(path)
}

/// `a/index.d.ts` → `a/index`.
fn strip_declaration(path: &str) -> &str {
    [".d.ts", ".d.mts", ".d.cts"]
        .iter()
        .find_map(|ext| path.strip_suffix(ext))
        .unwrap_or(path)
}

/// `pkg/dist/a/index.js` → `pkg/src/a/index.js`: where built files usually come from.
fn dist_to_src(dir: &str, path: &str) -> Option<String> {
    let rest = path.strip_prefix(&join(dir, "dist/"))?;
    Some(join(dir, &format!("src/{rest}")))
}

/// Every string in a `package.json` value, e.g. each condition of an `exports` map.
fn all_strings<'v>(value: &'v Value, out: &mut Vec<&'v str>) {
    match value {
        Value::String(s) => out.push(s),
        Value::Object(map) => map.values().for_each(|v| all_strings(v, out)),
        Value::Array(list) => list.iter().for_each(|v| all_strings(v, out)),
        _ => {}
    }
}

fn is_private_python(file: &str) -> bool {
    let name = name_of(file);
    name.ends_with(".py") && name.starts_with('_') && !name.starts_with("__")
}

/// `module` (its Rust children's directory): `a/b.rs` → `a/b`, `a/mod.rs` → `a`.
fn module_dir(file: &str) -> String {
    let name = name_of(file);
    match name {
        "mod.rs" | "lib.rs" | "main.rs" => dir_of(file).to_string(),
        _ => join(dir_of(file), name.strip_suffix(".rs").unwrap_or(name)),
    }
}

/// The first path in a `package.json` `exports` condition tree.
fn first_string(value: &Value) -> Option<&str> {
    match value {
        Value::String(s) => Some(s),
        Value::Object(map) => map.values().find_map(first_string),
        _ => None,
    }
}

// ----- `--base` -----

/// One imported symbol's change between base and head.
#[derive(Debug)]
pub struct Change {
    pub importer: String,
    pub symbol: String,
    /// Target file and bypass at each side; `None` when that side doesn't import it.
    pub before: Option<(String, usize)>,
    pub after: Option<(String, usize)>,
    /// Why a removal counts: `lower depth`, `moved` or `importer deleted`.
    pub reason: &'static str,
    /// The importer grew by at least the symbol's base line span: likely pasted in.
    pub grew: bool,
}

#[derive(Debug)]
pub struct SurfaceChange {
    pub dir: String,
    /// The boundary didn't exist at base, so its surface growth is expected.
    pub new_module: bool,
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

#[derive(Debug)]
pub struct Diff {
    pub improved: Vec<Change>,
    pub regressed: Vec<Change>,
    /// Bypassing imports gone while the symbol still sits in its base file: copied or inlined.
    pub removed_without_move: Vec<Change>,
    pub surface: Vec<SurfaceChange>,
    /// `(file, symbol)`.
    pub leaks_added: Vec<(String, String)>,
    pub leaks_removed: Vec<(String, String)>,
}

/// Matches imported symbols by `(importer, name)`. A bypassing symbol at base improves when
/// head imports it shallower, its file no longer declares it, or its importer is gone; it
/// disappearing otherwise earns nothing. Head symbols bypassing deeper than at base regress.
pub fn diff(base: &Coupling, head: &Coupling) -> Diff {
    let (before, after) = (base.symbol_map(), head.symbol_map());
    let side = |s: Option<&(&str, usize)>| s.map(|(t, d)| (t.to_string(), *d));
    let change = |key: &(&str, &str), reason, grew| Change {
        importer: key.0.to_string(),
        symbol: key.1.to_string(),
        before: side(before.get(key)),
        after: side(after.get(key)),
        reason,
        grew,
    };
    let declares = |c: &Coupling, file: &str, name: &str| {
        c.declared.get(file).is_some_and(|d| d.contains_key(name))
    };
    let mut improved = Vec::new();
    let mut removed_without_move = Vec::new();
    for (key, &(target, depth)) in before.iter().filter(|(_, (_, d))| *d > 0) {
        let (importer, name) = *key;
        match after.get(key) {
            Some(&(_, now)) if now < depth => improved.push(change(key, "lower depth", false)),
            Some(_) => {}
            None if !head.lines.contains_key(importer) => {
                improved.push(change(key, "importer deleted", false))
            }
            // Gone from its file, or the file is gone (a nameless symbol is its file).
            None if !head.lines.contains_key(target)
                || (declares(base, target, name) && !declares(head, target, name)) =>
            {
                improved.push(change(key, "moved", false))
            }
            None => {
                let span = base.declared.get(target).and_then(|d| d.get(name));
                let grew = span.is_some_and(|span| {
                    let lines = |c: &Coupling| c.lines.get(importer).copied().unwrap_or_default();
                    lines(head) >= lines(base) + span
                });
                removed_without_move.push(change(key, "", grew));
            }
        }
    }
    let mut regressed: Vec<Change> = after
        .iter()
        .filter(|(key, (_, now))| *now > 0 && before.get(*key).is_none_or(|(_, d)| d < now))
        .map(|(key, _)| change(key, "", false))
        .collect();
    for list in [&mut improved, &mut regressed, &mut removed_without_move] {
        list.sort_by(|a, b| (&a.importer, &a.symbol).cmp(&(&b.importer, &b.symbol)));
    }

    let surfaces = |c: &Coupling| -> BTreeMap<String, BTreeSet<String>> {
        c.boundaries
            .iter()
            .map(|b| (b.dir.clone(), b.surface.iter().cloned().collect()))
            .collect()
    };
    let (was, now) = (surfaces(base), surfaces(head));
    let empty = BTreeSet::new();
    let dirs: BTreeSet<&String> = was.keys().chain(now.keys()).collect();
    let surface = dirs
        .into_iter()
        .filter_map(|dir| {
            let (a, b) = (
                was.get(dir).unwrap_or(&empty),
                now.get(dir).unwrap_or(&empty),
            );
            let added: Vec<String> = b.difference(a).cloned().collect();
            let removed: Vec<String> = a.difference(b).cloned().collect();
            (!added.is_empty() || !removed.is_empty()).then(|| SurfaceChange {
                dir: dir.clone(),
                new_module: !was.contains_key(dir),
                added,
                removed,
            })
        })
        .collect();

    let leak_keys = |c: &Coupling| -> BTreeSet<(String, String)> {
        c.leaks
            .iter()
            .map(|l| (l.file.clone(), l.symbol.clone()))
            .collect()
    };
    let (was, now) = (leak_keys(base), leak_keys(head));
    Diff {
        improved,
        regressed,
        removed_without_move,
        surface,
        leaks_added: now.difference(&was).cloned().collect(),
        leaks_removed: was.difference(&now).cloned().collect(),
    }
}

// ----- output -----

pub struct Output<'a> {
    pub coupling: &'a Coupling,
    /// `--base` ref, merge base commit, the scan there and the diff.
    pub base: Option<(&'a str, &'a str, &'a Coupling, &'a Diff)>,
    pub folders: &'a [String],
    pub top: usize,
    pub all_edges: bool,
}

/// `.` for the scanned directory itself.
fn shown(dir: &str) -> &str {
    if dir.is_empty() { "." } else { dir }
}

/// `--folder` as given, relative to the scanned directory: `./src/` → `src`.
pub fn folder_key(folder: &str) -> String {
    normalize("", folder).unwrap_or_default()
}

pub fn render_text(o: &Output) -> String {
    let c = o.coupling;
    let t = &c.totals;
    let mut out = String::new();
    writeln!(
        out,
        "score {:.3} · bypassRate {:.3} ({} of {} cross-boundary symbols) · unusedSurface {:.3} ({} of {} entry exports) · hiding {:.3} ({} of {} declarations)",
        c.score(""),
        t.bypass_rate(),
        t.bypass_symbols,
        t.cross_symbols,
        t.unused_surface_rate(),
        t.unused_surface,
        t.surface,
        hiding_of(t.hidden, t.declared),
        t.hidden,
        t.declared
    )
    .ok();
    writeln!(
        out,
        "{} files, {} edges ({} cross-boundary, {} bypass, depth {}), {} symbols, {} leaks; bypassCost {}, meanBypassDepth {:.2}.",
        t.files,
        t.edges,
        t.cross_edges,
        t.bypass_edges,
        t.bypass_depth,
        t.symbols,
        t.leaks,
        t.bypass_cost,
        t.mean_bypass_depth()
    )
    .ok();
    for folder in o.folders {
        let key = folder_key(folder);
        let (bypass, unused, hiding) = c.ratios(&key);
        writeln!(
            out,
            "folder {folder}: score {:.3} · bypassRate {bypass:.3} · unusedSurface {unused:.3} · hiding {hiding:.3}",
            c.score(&key)
        )
        .ok();
    }
    if let Some((reference, commit, base, diff)) = o.base {
        render_diff(&mut out, c, reference, commit, base, diff);
        return out;
    }

    let boundaries = ranked(c);
    section(
        &mut out,
        "Boundaries, worst first",
        &boundaries,
        o.top,
        |b| {
            format!(
                "{:.2}  {}{}  surface {} (unused {}), hiding {:.2} ({} of {} hidden), bypass in {} of {}, leaks {}",
                b.score(),
                shown(&b.dir),
                if b.no_consumers() {
                    " (no consumers)"
                } else {
                    ""
                },
                b.surface.len(),
                b.unused.len(),
                b.hiding(),
                b.hidden,
                b.declared,
                b.bypass_in,
                b.incoming,
                b.leaks
            )
        },
    );
    let edges: Vec<&Edge> = c
        .edges
        .iter()
        .filter(|e| e.bypass > 0 || (o.all_edges && e.downs > 0))
        .collect();
    let title = if o.all_edges {
        "Cross-boundary edges"
    } else {
        "Bypass edges"
    };
    section(&mut out, title, &edges, o.top, |e| {
        let via = e
            .via
            .as_ref()
            .map(|v| format!(" (via {v})"))
            .unwrap_or_default();
        format!(
            "{} -> {}{via}  ups={} downs={} bypass={}",
            e.importer, e.target, e.ups, e.downs, e.bypass
        )
    });
    let leaks: Vec<&Leak> = c.leaks.iter().collect();
    section(&mut out, "Leaks", &leaks, o.top, |l| {
        format!("{}#{} <- {}", l.file, l.symbol, l.importers.join(", "))
    });
    out
}

fn render_diff(
    out: &mut String,
    c: &Coupling,
    reference: &str,
    commit: &str,
    base: &Coupling,
    d: &Diff,
) {
    let short = &commit[..commit.len().min(10)];
    let (was, now) = (&base.totals, &c.totals);
    writeln!(
        out,
        "\nSince {reference} ({short}): score {:.3} -> {:.3}, bypassRate {:.3} -> {:.3}, unusedSurface {:.3} -> {:.3}, hiding {:.3} -> {:.3}, bypassCost {} -> {}.",
        base.score(""),
        c.score(""),
        was.bypass_rate(),
        now.bypass_rate(),
        was.unused_surface_rate(),
        now.unused_surface_rate(),
        hiding_of(was.hidden, was.declared),
        hiding_of(now.hidden, now.declared),
        was.bypass_cost,
        now.bypass_cost
    )
    .ok();
    let side = |s: &Option<(String, usize)>| match s {
        Some((target, depth)) => format!("{target} bypass={depth}"),
        None => "gone".to_string(),
    };
    let line = |ch: &Change| {
        let reason = if ch.reason.is_empty() {
            String::new()
        } else {
            format!(" ({})", ch.reason)
        };
        let grew = if ch.grew { " (importer grew)" } else { "" };
        format!(
            "{}#{}  {} -> {}{reason}{grew}",
            ch.importer,
            ch.symbol,
            side(&ch.before),
            side(&ch.after)
        )
    };
    for (title, list) in [
        ("Improved", &d.improved),
        ("Regressed", &d.regressed),
        ("Removed without move", &d.removed_without_move),
    ] {
        let list: Vec<&Change> = list.iter().collect();
        section(out, title, &list, usize::MAX, |ch| line(ch));
    }
    let surface: Vec<&SurfaceChange> = d.surface.iter().collect();
    section(out, "Surface", &surface, usize::MAX, |s| {
        let new = if s.new_module { " (new module)" } else { "" };
        let mut parts = Vec::new();
        if !s.added.is_empty() {
            parts.push(format!("+{}", s.added.join(", +")));
        }
        if !s.removed.is_empty() {
            parts.push(format!("-{}", s.removed.join(", -")));
        }
        format!("{}{new}: {}", shown(&s.dir), parts.join(" "))
    });
    for (title, list) in [
        ("Leaks added", &d.leaks_added),
        ("Leaks removed", &d.leaks_removed),
    ] {
        let list: Vec<&(String, String)> = list.iter().collect();
        section(out, title, &list, usize::MAX, |(file, symbol)| {
            format!("{file}#{symbol}")
        });
    }
}

/// `\nTitle (n):` then up to `top` lines.
fn section<T>(out: &mut String, title: &str, items: &[T], top: usize, line: impl Fn(&T) -> String) {
    writeln!(out, "\n{title} ({}):", items.len()).ok();
    for item in items.iter().take(top) {
        writeln!(out, "  {}", line(item)).ok();
    }
    if items.len() > top {
        writeln!(
            out,
            "  … {} more; raise --top to see them.",
            items.len() - top
        )
        .ok();
    }
}

fn totals_json(c: &Coupling) -> Value {
    let t = &c.totals;
    json!({
        "score": c.score(""),
        "bypassRate": t.bypass_rate(),
        "unusedSurface": t.unused_surface_rate(),
        "hiding": hiding_of(t.hidden, t.declared),
        "declared": t.declared,
        "hidden": t.hidden,
        "files": t.files,
        "edges": t.edges,
        "crossBoundaryEdges": t.cross_edges,
        "bypassEdges": t.bypass_edges,
        "bypassDepth": t.bypass_depth,
        "symbols": t.symbols,
        "crossBoundarySymbols": t.cross_symbols,
        "bypassSymbols": t.bypass_symbols,
        "bypassCost": t.bypass_cost,
        "meanBypassDepth": t.mean_bypass_depth(),
        "surface": t.surface,
        "unusedSurfaceCount": t.unused_surface,
        "leaks": t.leaks,
    })
}

/// Everything, untruncated: `{ totals, folders, boundaries, edges, leaks, diff? }`.
/// Worst first; ties (many boundaries score 0) by how reached-into they are.
fn ranked(c: &Coupling) -> Vec<&Boundary> {
    let mut boundaries: Vec<&Boundary> = c.boundaries.iter().collect();
    boundaries.sort_by(|a, b| {
        a.score()
            .total_cmp(&b.score())
            .then((b.bypass_in + b.leaks).cmp(&(a.bypass_in + a.leaks)))
            .then(a.dir.cmp(&b.dir))
    });
    boundaries
}

pub fn to_json(o: &Output) -> Value {
    let c = o.coupling;
    let folders: serde_json::Map<String, Value> = o
        .folders
        .iter()
        .map(|f| {
            let key = folder_key(f);
            let (bypass, unused, hiding) = c.ratios(&key);
            let ratios = json!({ "score": c.score(&key), "bypassRate": bypass, "unusedSurface": unused, "hiding": hiding });
            (f.clone(), ratios)
        })
        .collect();
    let boundaries: Vec<Value> = ranked(c)
        .into_iter()
        .map(|b| {
            json!({
                "dir": b.dir,
                "score": b.score(),
                "entries": b.entries,
                "surface": b.surface,
                "unused": b.unused,
                "unusedSurface": b.unused_rate(),
                "declared": b.declared,
                "hidden": b.hidden,
                "hiding": b.hiding(),
                "incoming": b.incoming,
                "bypassIn": b.bypass_in,
                "bypassRate": b.bypass_rate(),
                "leaks": b.leaks,
                "noConsumers": b.no_consumers(),
            })
        })
        .collect();
    let edges: Vec<Value> = c
        .edges
        .iter()
        .filter(|e| e.bypass > 0 || (o.all_edges && e.downs > 0))
        .map(|e| {
            json!({
                "importer": e.importer,
                "target": e.target,
                "via": e.via,
                "names": e.names,
                "ups": e.ups,
                "downs": e.downs,
                "bypass": e.bypass,
            })
        })
        .collect();
    let leaks: Vec<Value> = c
        .leaks
        .iter()
        .map(|l| json!({ "file": l.file, "symbol": l.symbol, "boundary": l.boundary, "importers": l.importers }))
        .collect();
    let mut out = json!({
        "totals": totals_json(c),
        "folders": folders,
        "boundaries": boundaries,
        "edges": edges,
        "leaks": leaks,
    });
    if let Some((reference, commit, base, d)) = o.base {
        let side = |s: &Option<(String, usize)>| {
            s.as_ref().map(|(t, b)| json!({ "target": t, "bypass": b }))
        };
        let changes = |list: &[Change]| -> Vec<Value> {
            list.iter()
                .map(|ch| {
                    let mut v = json!({
                        "importer": ch.importer,
                        "symbol": ch.symbol,
                        "base": side(&ch.before),
                        "head": side(&ch.after),
                    });
                    if !ch.reason.is_empty() {
                        v["reason"] = json!(ch.reason);
                    }
                    if ch.grew {
                        v["grew"] = json!(true);
                    }
                    v
                })
                .collect()
        };
        let leak = |list: &[(String, String)]| -> Vec<Value> {
            list.iter()
                .map(|(f, s)| json!({ "file": f, "symbol": s }))
                .collect()
        };
        out["diff"] = json!({
            "base": reference,
            "mergeBase": commit,
            "totals": totals_json(base),
            "improved": changes(&d.improved),
            "regressed": changes(&d.regressed),
            "removedWithoutMove": changes(&d.removed_without_move),
            "surface": d.surface.iter().map(|s| json!({
                "dir": s.dir,
                "newModule": s.new_module,
                "added": s.added,
                "removed": s.removed,
            })).collect::<Vec<_>>(),
            "leaks": { "added": leak(&d.leaks_added), "removed": leak(&d.leaks_removed) },
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_inside_the_scanned_directory() {
        assert_eq!(normalize("a/b", "../c/./d").as_deref(), Some("a/c/d"));
        assert_eq!(normalize("a", "../../x"), None);
        assert_eq!(common_dir("a/b/c", "a/b/d/e"), "a/b");
        assert_eq!(common_dir("a", "b"), "");
        assert!(within("a/b.ts", "a") && within("a/b.ts", "") && !within("ab/c.ts", "a"));
        assert_eq!(
            up_to("a/b/c", "a").collect::<Vec<_>>(),
            vec!["a/b/c", "a/b"]
        );
        assert_eq!(module_dir("src/a/b.rs"), "src/a/b");
        assert_eq!(module_dir("src/a/mod.rs"), "src/a");
    }

    #[test]
    fn entry_and_private_names() {
        assert!(
            is_entry_name("index.tsx") && is_entry_name("__init__.py") && is_entry_name("lib.rs")
        );
        assert!(!is_entry_name("indexer.ts") && !is_entry_name("index.css"));
        assert!(is_private_python("pkg/_impl.py") && !is_private_python("pkg/__init__.py"));
    }
}
