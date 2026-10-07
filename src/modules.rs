//! Modules (packages, crates, projects) found from manifests, their internal dependency
//! graph, and what it says about a duplicate pair: can one side import the other?

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use serde::Serialize;

use crate::config::{ModuleOverride, lookup};

/// Files under no module.
pub const ROOT_MODULE: &str = "(root)";

/// Python environments, skipped on top of the source walk's own list.
const ENV_DIRS: &[&str] = &["venv", "site-packages"];

#[derive(Debug, Clone)]
pub struct Module {
    pub name: String,
    /// Canonical directory holding the manifest.
    pub root: PathBuf,
    /// Direct internal dependencies, of every kind (dev, peer, optional, build …).
    pub deps: Vec<usize>,
    /// Names of other manifests in the same directory, e.g. a crate beside a `package.json`.
    pub aliases: Vec<String>,
}

#[derive(Debug)]
pub struct ModuleGraph {
    pub modules: Vec<Module>,
    /// `reach[a][b]`: `a` depends on `b`, directly or transitively.
    reach: Vec<Vec<bool>>,
}

/// What the dependency graph says about a duplicate pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Tag {
    /// Both sides are in one module.
    SameModule { module: String },
    /// `copy` already depends on `owner`, so it can import the original.
    Importable { copy: String, owner: String },
    /// `copy` reaches `owner` only through other modules; it needs a direct dependency first.
    ImportableIndirect { copy: String, owner: String },
    /// Neither depends on the other, but both depend on `home`, which could hold one copy.
    MoveDown { home: String },
    /// No dependency path between the two.
    Boundary,
}

/// Where a cluster's one shared definition could live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Home {
    /// A member's module that every other member's module depends on.
    Owner { module: String },
    /// No member's module works; this common dependency of all of them could.
    MoveDown { module: String },
}

impl Tag {
    /// Sort order: most actionable first.
    pub fn rank(&self) -> u8 {
        match self {
            Tag::Importable { .. } => 0,
            Tag::ImportableIndirect { .. } => 1,
            Tag::MoveDown { .. } => 2,
            Tag::Boundary => 3,
            Tag::SameModule { .. } => 4,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Tag::SameModule { module } => format!("same-module: {module}"),
            Tag::Importable { copy, owner } => format!("importable: {copy} can import {owner}"),
            Tag::ImportableIndirect { copy, owner } => {
                format!("importable-indirect: {copy} reaches {owner} only transitively")
            }
            Tag::MoveDown { home } => format!("move-down: both depend on {home}"),
            Tag::Boundary => "boundary: no dependency path".to_string(),
        }
    }
}

impl Home {
    pub fn describe(&self) -> String {
        match self {
            Home::Owner { module } => format!("home: {module} (every member can import it)"),
            Home::MoveDown { module } => format!("home: move down to {module}"),
        }
    }
}

impl ModuleGraph {
    /// Modules under `root`, with `[modules.<name>]` overrides applied.
    pub fn discover(root: &Path, overrides: &BTreeMap<String, ModuleOverride>) -> Result<Self> {
        let workspaces = Workspaces::read(root);
        let mut found: Vec<Found> = manifests(root)
            .iter()
            .filter_map(|manifest| parse(manifest, root, &workspaces))
            .collect();
        add_implicit_cargo_members(&mut found, &workspaces);
        found.retain(|f| f.member);
        // Sorted by root, then by ecosystem and manifest, so results don't depend on walk order.
        found.sort_by(|a, b| {
            (&a.root, a.ecosystem, &a.manifest).cmp(&(&b.root, b.ecosystem, &b.manifest))
        });

        // Manifests sharing a directory are one module, named after the first.
        let mut modules: Vec<Module> = Vec::new();
        let mut module_of: Vec<usize> = Vec::with_capacity(found.len());
        for f in &found {
            match modules.last_mut() {
                Some(last) if last.root == f.root => last.aliases.push(f.name.clone()),
                _ => modules.push(Module {
                    name: f.name.clone(),
                    root: f.root.clone(),
                    deps: Vec::new(),
                    aliases: Vec::new(),
                }),
            }
            module_of.push(modules.len() - 1);
        }
        for (i, f) in found.iter().enumerate() {
            let own = module_of[i];
            let deps = &mut modules[own].deps;
            deps.extend(
                f.deps
                    .iter()
                    .filter_map(|dep| resolve(dep, &found))
                    .map(|d| module_of[d])
                    .filter(|&d| d != own),
            );
            deps.sort_unstable();
            deps.dedup();
        }
        apply_overrides(&mut modules, root, overrides)?;
        Ok(Self::new(modules))
    }

    pub fn new(modules: Vec<Module>) -> Self {
        let n = modules.len();
        let reach = (0..n)
            .map(|start| {
                let mut seen = vec![false; n];
                let mut stack = modules[start].deps.clone();
                while let Some(m) = stack.pop() {
                    if !seen[m] {
                        seen[m] = true;
                        stack.extend(&modules[m].deps);
                    }
                }
                seen[start] = false;
                seen
            })
            .collect();
        ModuleGraph { modules, reach }
    }

    /// The module with the longest root containing `path`; `None` is [`ROOT_MODULE`].
    pub fn module_of(&self, path: &Path) -> Option<usize> {
        self.modules
            .iter()
            .enumerate()
            .filter(|(_, m)| path.starts_with(&m.root))
            .max_by_key(|(_, m)| m.root.components().count())
            .map(|(i, _)| i)
    }

    pub fn name(&self, module: Option<usize>) -> &str {
        module.map_or(ROOT_MODULE, |m| &self.modules[m].name)
    }

    pub fn tag(&self, a: Option<usize>, b: Option<usize>) -> Tag {
        let name = |m: usize| self.modules[m].name.clone();
        // Files under no module carry no dependency information.
        if a == b && a.is_some() {
            return Tag::SameModule {
                module: self.name(a).to_string(),
            };
        }
        let (Some(a), Some(b)) = (a, b) else {
            return Tag::Boundary;
        };
        for (copy, owner) in [(a, b), (b, a)] {
            if self.modules[copy].deps.contains(&owner) {
                return Tag::Importable {
                    copy: name(copy),
                    owner: name(owner),
                };
            }
        }
        for (copy, owner) in [(a, b), (b, a)] {
            if self.reach[copy][owner] {
                return Tag::ImportableIndirect {
                    copy: name(copy),
                    owner: name(owner),
                };
            }
        }
        match self.lowest(|m| self.reach[a][m] && self.reach[b][m]) {
            Some(home) => Tag::MoveDown { home: name(home) },
            None => Tag::Boundary,
        }
    }

    /// Where a cluster spanning `members`' modules could keep one definition.
    pub fn home(&self, members: &[Option<usize>]) -> Option<Home> {
        let mut set: Vec<usize> = members.iter().copied().collect::<Option<Vec<_>>>()?;
        set.sort_unstable();
        set.dedup();
        if set.len() < 2 {
            return None;
        }
        let reaches = |from: usize, to: usize| from == to || self.reach[from][to];
        if let Some(owner) = self.lowest(|m| set.contains(&m) && set.iter().all(|&o| reaches(o, m)))
        {
            return Some(Home::Owner {
                module: self.modules[owner].name.clone(),
            });
        }
        self.lowest(|m| set.iter().all(|&o| self.reach[o][m]))
            .map(|m| Home::MoveDown {
                module: self.modules[m].name.clone(),
            })
    }

    /// The matching module with the fewest dependencies of its own, then by name.
    fn lowest(&self, matches: impl Fn(usize) -> bool) -> Option<usize> {
        (0..self.modules.len())
            .filter(|&m| matches(m))
            .min_by_key(|&m| {
                let deps = self.reach[m].iter().filter(|&&r| r).count();
                (deps, self.modules[m].name.clone())
            })
    }
}

// ----- discovery -----

/// Order decides which manifest names a shared directory's module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Ecosystem {
    Node,
    Cargo,
    Python,
    DotNet,
}

#[derive(Debug)]
enum Dep {
    /// A package name within one ecosystem.
    Name(Ecosystem, String),
    /// A module root directory, e.g. a Cargo `path` dependency.
    Dir(PathBuf),
    /// A manifest file, e.g. a `ProjectReference`.
    Manifest(PathBuf),
}

#[derive(Debug)]
struct Found {
    name: String,
    ecosystem: Ecosystem,
    root: PathBuf,
    manifest: PathBuf,
    deps: Vec<Dep>,
    /// Directory relative to the project root.
    rel: String,
    /// In its ecosystem's workspace, or no workspace is declared.
    member: bool,
}

/// Which manifests belong to a workspace declared at the project root.
#[derive(Default)]
struct Workspaces {
    node: Option<Members>,
    cargo: Option<Members>,
    python: Option<Members>,
    cargo_root: Option<toml::Table>,
}

struct Members {
    include: GlobSet,
    exclude: GlobSet,
}

impl Members {
    fn new(include: &[String], exclude: &[String]) -> Self {
        Members {
            include: globs(include),
            exclude: globs(exclude),
        }
    }

    /// `rel` is the manifest directory relative to the project root; the root itself is always in.
    fn contains(&self, rel: &str) -> bool {
        rel.is_empty() || (self.include.is_match(rel) && !self.exclude.is_match(rel))
    }
}

impl Workspaces {
    fn read(root: &Path) -> Self {
        let mut node_patterns: Vec<String> = Vec::new();
        let mut node_declared = false;
        if let Some(json) = read_json(&root.join("package.json")) {
            let list = json
                .get("workspaces")
                .map(|w| w.get("packages").unwrap_or(w));
            if let Some(list) = list.and_then(|l| l.as_array()) {
                node_declared = true;
                node_patterns.extend(list.iter().filter_map(|v| v.as_str().map(str::to_string)));
            }
        }
        if let Ok(text) = std::fs::read_to_string(root.join("pnpm-workspace.yaml")) {
            node_declared = true;
            node_patterns.extend(pnpm_packages(&text));
        }
        let node = node_declared.then(|| {
            let (exclude, include): (Vec<String>, Vec<String>) =
                node_patterns.into_iter().partition(|p| p.starts_with('!'));
            let exclude: Vec<String> = exclude.iter().map(|p| p[1..].to_string()).collect();
            Members::new(&include, &exclude)
        });

        let cargo_root = read_toml(&root.join("Cargo.toml"));
        let cargo = cargo_root
            .as_ref()
            .and_then(|t| lookup(t, &["workspace"]))
            .and_then(|w| w.as_table())
            .map(|w| {
                Members::new(
                    &string_list(w.get("members")),
                    &string_list(w.get("exclude")),
                )
            });
        let python = read_toml(&root.join("pyproject.toml"))
            .as_ref()
            .and_then(|t| lookup(t, &["tool", "uv", "workspace"]))
            .and_then(|w| w.as_table())
            .map(|w| {
                Members::new(
                    &string_list(w.get("members")),
                    &string_list(w.get("exclude")),
                )
            });
        Workspaces {
            node,
            cargo,
            python,
            cargo_root,
        }
    }
}

/// Every `package.json`, `Cargo.toml`, `pyproject.toml` and `*.csproj` under `root`.
fn manifests(root: &Path) -> Vec<PathBuf> {
    let walker = WalkBuilder::new(root)
        .require_git(false)
        .filter_entry(|entry| {
            let name = entry.file_name();
            !crate::walk::SKIPPED_DIRS
                .iter()
                .chain(ENV_DIRS)
                .any(|dir| name == *dir)
        })
        .build();
    walker
        .flatten()
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .map(|e| e.into_path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            matches!(name, "package.json" | "Cargo.toml" | "pyproject.toml")
                || name.ends_with(".csproj")
        })
        .collect()
}

fn parse(manifest: &Path, root: &Path, workspaces: &Workspaces) -> Option<Found> {
    let dir = manifest.parent()?.canonicalize().ok()?;
    let rel = relative(root, &dir);
    let file = manifest.file_name()?.to_str()?;
    let member = |members: &Option<Members>| members.as_ref().is_none_or(|m| m.contains(&rel));
    let found = |name: String, ecosystem, deps, member| Found {
        name,
        ecosystem,
        root: dir.clone(),
        manifest: manifest
            .canonicalize()
            .unwrap_or_else(|_| manifest.to_path_buf()),
        deps,
        rel: rel.clone(),
        member,
    };
    match file {
        "package.json" if member(&workspaces.node) => {
            let json = read_json(manifest)?;
            let name = json.get("name")?.as_str()?.to_string();
            let deps = [
                "dependencies",
                "devDependencies",
                "peerDependencies",
                "optionalDependencies",
            ]
            .iter()
            .filter_map(|key| json.get(*key)?.as_object())
            .flatten()
            .flat_map(|(key, spec)| {
                // `"alias": "workspace:core@*"` or `"npm:@x/core@^1"` names another package.
                let aliased = spec.as_str().and_then(aliased_package);
                std::iter::once(key.clone()).chain(aliased)
            })
            .map(|dep| Dep::Name(Ecosystem::Node, dep))
            .collect();
            Some(found(name, Ecosystem::Node, deps, true))
        }
        // Non-members are kept for now: a member's path dependency makes a crate a member too.
        "Cargo.toml" => {
            let table = read_toml(manifest)?;
            let name = lookup(&table, &["package", "name"])?.as_str()?.to_string();
            let deps = cargo_deps(&table, &dir, workspaces.cargo_root.as_ref(), root);
            Some(found(
                name,
                Ecosystem::Cargo,
                deps,
                member(&workspaces.cargo),
            ))
        }
        "pyproject.toml" if member(&workspaces.python) => {
            let table = read_toml(manifest)?;
            let name = lookup(&table, &["project", "name"])
                .or_else(|| lookup(&table, &["tool", "poetry", "name"]))?
                .as_str()?;
            Some(found(
                python_name(name),
                Ecosystem::Python,
                python_deps(&table),
                true,
            ))
        }
        _ if file.ends_with(".csproj") => {
            let name = file.trim_end_matches(".csproj").to_string();
            let text = std::fs::read_to_string(manifest).ok()?;
            let deps = project_references(&text)
                .into_iter()
                .filter_map(|r| dir.join(r.replace('\\', "/")).canonicalize().ok())
                .map(Dep::Manifest)
                .collect();
            Some(found(name, Ecosystem::DotNet, deps, true))
        }
        _ => None,
    }
}

/// Cargo makes every crate a member's path dependency points at, inside the workspace, a
/// member too, unless `exclude` lists it.
fn add_implicit_cargo_members(found: &mut [Found], workspaces: &Workspaces) {
    let Some(members) = &workspaces.cargo else {
        return;
    };
    loop {
        let targets: Vec<PathBuf> = found
            .iter()
            .filter(|f| f.member && f.ecosystem == Ecosystem::Cargo)
            .flat_map(|f| f.deps.iter())
            .filter_map(|dep| match dep {
                Dep::Dir(dir) => Some(dir.clone()),
                _ => None,
            })
            .collect();
        let mut changed = false;
        for f in found.iter_mut() {
            if !f.member
                && f.ecosystem == Ecosystem::Cargo
                && targets.contains(&f.root)
                && !members.exclude.is_match(&f.rel)
            {
                f.member = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

/// `workspace:core@*` → `core`, `npm:@x/core@^1` → `@x/core`; `workspace:*` names nothing.
fn aliased_package(spec: &str) -> Option<String> {
    let (rest, npm) = match spec.strip_prefix("workspace:") {
        Some(rest) => (rest, false),
        None => (spec.strip_prefix("npm:")?, true),
    };
    let scope_offset = usize::from(rest.starts_with('@'));
    let name = match rest[scope_offset..].find('@') {
        Some(at) => &rest[..at + scope_offset],
        None if npm => rest,
        None => return None,
    };
    (!name.is_empty()).then(|| name.to_string())
}

fn resolve(dep: &Dep, found: &[Found]) -> Option<usize> {
    found.iter().position(|f| match dep {
        Dep::Name(ecosystem, name) => f.ecosystem == *ecosystem && f.name == *name,
        Dep::Dir(dir) => f.root == *dir,
        Dep::Manifest(path) => f.manifest == *path,
    })
}

/// `[dependencies]`, `[dev-dependencies]` and `[build-dependencies]`, including per-target
/// tables. Path dependencies resolve by directory; `workspace = true` through the root.
fn cargo_deps(
    table: &toml::Table,
    dir: &Path,
    workspace_root: Option<&toml::Table>,
    root: &Path,
) -> Vec<Dep> {
    const KINDS: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];
    let mut tables: Vec<&toml::Table> = KINDS
        .iter()
        .filter_map(|k| table.get(*k)?.as_table())
        .collect();
    if let Some(targets) = table.get("target").and_then(|t| t.as_table()) {
        for target in targets.values().filter_map(|t| t.as_table()) {
            tables.extend(KINDS.iter().filter_map(|k| target.get(*k)?.as_table()));
        }
    }
    let shared = workspace_root.and_then(|t| lookup(t, &["workspace", "dependencies"]));
    let mut deps = Vec::new();
    for (key, spec) in tables.into_iter().flatten() {
        let (spec, base) = if spec.get("workspace").and_then(|w| w.as_bool()) == Some(true) {
            match shared.and_then(|s| s.get(key)) {
                Some(spec) => (spec, root),
                None => (spec, dir),
            }
        } else {
            (spec, dir)
        };
        if let Some(path) = spec.get("path").and_then(|p| p.as_str())
            && let Ok(path) = base.join(path).canonicalize()
        {
            deps.push(Dep::Dir(path));
            continue;
        }
        let package = spec.get("package").and_then(|p| p.as_str()).unwrap_or(key);
        deps.push(Dep::Name(Ecosystem::Cargo, package.to_string()));
    }
    deps
}

/// PEP 621 dependencies, optional dependencies and dependency groups, plus Poetry tables.
fn python_deps(table: &toml::Table) -> Vec<Dep> {
    let mut requirements: Vec<String> = string_list(lookup(table, &["project", "dependencies"]));
    for path in [
        &["project", "optional-dependencies"][..],
        &["dependency-groups"],
    ] {
        if let Some(groups) = lookup(table, path).and_then(|g| g.as_table()) {
            for group in groups.values() {
                requirements.extend(string_list(Some(group)));
            }
        }
    }
    let mut names: Vec<String> = requirements.iter().map(|r| requirement_name(r)).collect();
    let mut poetry_tables = vec![
        lookup(table, &["tool", "poetry", "dependencies"]),
        lookup(table, &["tool", "poetry", "dev-dependencies"]),
    ];
    if let Some(groups) = lookup(table, &["tool", "poetry", "group"]).and_then(|g| g.as_table()) {
        poetry_tables.extend(groups.values().map(|g| g.get("dependencies")));
    }
    for deps in poetry_tables
        .into_iter()
        .flatten()
        .filter_map(|d| d.as_table())
    {
        names.extend(deps.keys().filter(|k| *k != "python").cloned());
    }
    names
        .into_iter()
        .filter(|n| !n.is_empty())
        .map(|n| Dep::Name(Ecosystem::Python, python_name(&n)))
        .collect()
}

/// `requests[socks]>=2.0; python_version<'3.12'` → `requests`.
fn requirement_name(requirement: &str) -> String {
    requirement
        .trim()
        .chars()
        .take_while(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect()
}

/// PEP 503 normalisation: `My_Package.Core` → `my-package-core`.
fn python_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars() {
        if matches!(c, '-' | '_' | '.') {
            if !out.ends_with('-') {
                out.push('-');
            }
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// `Include` paths of `<ProjectReference Include="..\\Core\\Core.csproj" />`, outside XML comments.
fn project_references(xml: &str) -> Vec<String> {
    let mut text = String::with_capacity(xml.len());
    let mut rest = xml;
    while let Some(start) = rest.find("<!--") {
        text.push_str(&rest[..start]);
        rest = rest[start..]
            .find("-->")
            .map_or("", |end| &rest[start + end + 3..]);
    }
    text.push_str(rest);

    let mut out = Vec::new();
    for (start, _) in text.match_indices("<ProjectReference") {
        let tag = &text[start..];
        let tag = &tag[..tag.find('>').unwrap_or(tag.len())];
        let Some(at) = tag.find("Include") else {
            continue;
        };
        let value = tag[at + "Include".len()..].trim_start();
        let Some(value) = value.strip_prefix('=').map(str::trim_start) else {
            continue;
        };
        let Some(quote) = value.chars().next().filter(|c| matches!(c, '"' | '\'')) else {
            continue;
        };
        if let Some(end) = value[1..].find(quote) {
            out.push(value[1..1 + end].to_string());
        }
    }
    out
}

/// The `packages:` list of a `pnpm-workspace.yaml`, block or inline form.
fn pnpm_packages(yaml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_packages = false;
    for line in yaml.lines() {
        let content = line.split(" #").next().unwrap_or_default();
        if content.trim().is_empty() || content.trim_start().starts_with('#') {
            continue;
        }
        let top_level = !line.starts_with(char::is_whitespace) && !line.starts_with('-');
        if top_level {
            in_packages = false;
            if let Some(rest) = content.strip_prefix("packages:") {
                let rest = rest.trim();
                if let Some(inline) = rest.strip_prefix('[') {
                    let inline = inline.trim_end().trim_end_matches(']');
                    out.extend(
                        split_flow(inline)
                            .iter()
                            .map(|s| unquote(s))
                            .filter(|s| !s.is_empty()),
                    );
                } else {
                    in_packages = true;
                }
            }
            continue;
        }
        if in_packages && let Some(item) = content.trim().strip_prefix('-') {
            out.push(unquote(item));
        }
    }
    out
}

/// Split a YAML flow list on commas outside quotes and braces: `'a/{x,y}', b`.
fn split_flow(text: &str) -> Vec<&str> {
    let (mut parts, mut start, mut depth, mut quote) = (Vec::new(), 0, 0i32, None);
    for (i, c) in text.char_indices() {
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, '{' | '[') => depth += 1,
            (None, '}' | ']') => depth -= 1,
            (None, ',') if depth == 0 => {
                parts.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

fn apply_overrides(
    modules: &mut Vec<Module>,
    root: &Path,
    overrides: &BTreeMap<String, ModuleOverride>,
) -> Result<()> {
    let index_of = |modules: &[Module], name: &str| {
        modules
            .iter()
            .position(|m| m.name == name || m.aliases.iter().any(|a| a == name))
    };
    // Define and relocate first, so `deps` can name modules defined by other overrides.
    for (name, change) in overrides {
        match (index_of(modules, name), &change.path) {
            (Some(i), Some(path)) => modules[i].root = module_path(root, name, path)?,
            (None, Some(path)) => modules.push(Module {
                name: name.clone(),
                root: module_path(root, name, path)?,
                deps: Vec::new(),
                aliases: Vec::new(),
            }),
            (Some(_), None) => {}
            (None, None) if change.ignore => {}
            (None, None) => {
                bail!("[modules.{name}] matches no detected module; add `path` to define it")
            }
        }
    }
    for (name, change) in overrides {
        let (Some(index), Some(deps)) = (index_of(modules, name), &change.deps) else {
            continue;
        };
        let mut resolved = Vec::new();
        for dep in deps {
            let Some(d) = index_of(modules, dep) else {
                bail!("[modules.{name}] deps: no module named `{dep}`");
            };
            resolved.push(d);
        }
        modules[index].deps = resolved;
    }
    // Drop ignored modules last. Their dependents inherit their dependencies, so paths through
    // them survive, and the remaining indices are renumbered.
    let keep: Vec<bool> = modules
        .iter()
        .map(|m| {
            !overrides
                .iter()
                .any(|(name, o)| o.ignore && (m.name == *name || m.aliases.contains(name)))
        })
        .collect();
    let spliced: Vec<Vec<usize>> = (0..modules.len())
        .map(|i| {
            let mut out = Vec::new();
            let mut stack = modules[i].deps.clone();
            let mut seen = vec![false; modules.len()];
            while let Some(d) = stack.pop() {
                if std::mem::replace(&mut seen[d], true) || d == i {
                    continue;
                }
                if keep[d] {
                    out.push(d);
                } else {
                    stack.extend(&modules[d].deps);
                }
            }
            out.sort_unstable();
            out
        })
        .collect();
    for (module, deps) in modules.iter_mut().zip(spliced) {
        module.deps = deps;
    }
    let mut new_index = Vec::with_capacity(modules.len());
    let mut next = 0;
    for &k in &keep {
        new_index.push(k.then_some(next));
        next += usize::from(k);
    }
    let mut kept = Vec::new();
    for (i, mut module) in std::mem::take(modules).into_iter().enumerate() {
        if keep[i] {
            module.deps = module.deps.iter().filter_map(|&d| new_index[d]).collect();
            kept.push(module);
        }
    }
    *modules = kept;
    Ok(())
}

fn module_path(root: &Path, name: &str, path: &str) -> Result<PathBuf> {
    root.join(path)
        .canonicalize()
        .map_err(|err| anyhow::anyhow!("[modules.{name}] path `{path}`: {err}"))
}

/// `dir` relative to `root`, `/`-joined; empty when equal or outside.
pub(crate) fn relative(root: &Path, dir: &Path) -> String {
    dir.strip_prefix(root)
        .map(|rel| {
            rel.components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/")
        })
        .unwrap_or_default()
}

fn globs(patterns: &[String]) -> GlobSet {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let pattern = pattern.trim_start_matches("./").trim_end_matches('/');
        if let Ok(glob) = GlobBuilder::new(pattern).literal_separator(true).build() {
            builder.add(glob);
        }
    }
    builder.build().unwrap_or_else(|_| GlobSet::empty())
}

fn string_list(value: Option<&toml::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn read_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

pub(crate) fn read_toml(path: &Path) -> Option<toml::Table> {
    std::fs::read_to_string(path).ok()?.parse().ok()
}

fn unquote(text: &str) -> String {
    text.trim()
        .trim_matches(|c| c == '\'' || c == '"')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp tree from `(path, contents)` pairs; returns its canonical root.
    fn tree(test: &str, files: &[(&str, &str)]) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("duped-modules-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (path, contents) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        root.canonicalize().unwrap()
    }

    fn names(graph: &ModuleGraph) -> Vec<(&str, Vec<&str>)> {
        graph
            .modules
            .iter()
            .map(|m| {
                let deps = m
                    .deps
                    .iter()
                    .map(|&d| graph.modules[d].name.as_str())
                    .collect();
                (m.name.as_str(), deps)
            })
            .collect()
    }

    fn graph(root: &Path) -> ModuleGraph {
        ModuleGraph::discover(root, &BTreeMap::new()).unwrap()
    }

    #[test]
    fn pnpm_workspace_members_and_all_dependency_kinds() {
        let root = tree(
            "pnpm",
            &[
                (
                    "package.json",
                    r#"{"name":"mono","devDependencies":{"@x/app":"workspace:*"}}"#,
                ),
                (
                    "pnpm-workspace.yaml",
                    "packages:\n  - 'packages/*'\n  - \"!packages/skip\"  # not this one\n",
                ),
                ("packages/base/package.json", r#"{"name":"@x/base"}"#),
                (
                    "packages/core/package.json",
                    r#"{"name":"@x/core","peerDependencies":{"@x/base":"*"}}"#,
                ),
                (
                    "packages/app/package.json",
                    r#"{"name":"@x/app","dependencies":{"@x/core":"1","react":"18"},"devDependencies":{"@x/base":"1"}}"#,
                ),
                ("packages/skip/package.json", r#"{"name":"@x/skip"}"#),
                ("examples/demo/package.json", r#"{"name":"demo"}"#),
                (
                    "packages/core/node_modules/dep/package.json",
                    r#"{"name":"dep"}"#,
                ),
            ],
        );
        assert_eq!(
            names(&graph(&root)),
            vec![
                ("mono", vec!["@x/app"]),
                ("@x/app", vec!["@x/base", "@x/core"]),
                ("@x/base", vec![]),
                ("@x/core", vec!["@x/base"]),
            ]
        );
    }

    #[test]
    fn npm_workspaces_object_form() {
        let root = tree(
            "npm",
            &[
                (
                    "package.json",
                    r#"{"name":"r","workspaces":{"packages":["libs/**"]}}"#,
                ),
                ("libs/a/b/package.json", r#"{"name":"deep"}"#),
                ("other/package.json", r#"{"name":"outside"}"#),
            ],
        );
        let g = graph(&root);
        assert_eq!(
            names(&g).iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            vec!["r", "deep"]
        );
    }

    #[test]
    fn without_a_workspace_every_named_package_is_a_module() {
        let root = tree(
            "loose",
            &[
                ("a/package.json", r#"{"name":"a","dependencies":{"b":"1"}}"#),
                ("b/package.json", r#"{"name":"b"}"#),
                ("c/package.json", r#"{"private":true}"#),
            ],
        );
        assert_eq!(names(&graph(&root)), vec![("a", vec!["b"]), ("b", vec![])]);
    }

    #[test]
    fn cargo_workspace_path_renamed_and_workspace_deps() {
        let root = tree(
            "cargo",
            &[
                (
                    "Cargo.toml",
                    "[workspace]\nmembers = [\"crates/*\"]\nexclude = [\"crates/old\"]\n[workspace.dependencies]\ncore = { path = \"crates/core\" }\n",
                ),
                ("crates/core/Cargo.toml", "[package]\nname = \"core\"\n"),
                (
                    "crates/util/Cargo.toml",
                    "[package]\nname = \"util-lib\"\n[dependencies]\ncore = { workspace = true }\n",
                ),
                (
                    "crates/app/Cargo.toml",
                    "[package]\nname = \"app\"\n[dependencies]\nu = { package = \"util-lib\", version = \"1\" }\n[target.'cfg(unix)'.dev-dependencies]\nhelper = { path = \"../core\" }\n",
                ),
                ("crates/old/Cargo.toml", "[package]\nname = \"old\"\n"),
            ],
        );
        assert_eq!(
            names(&graph(&root)),
            vec![
                ("app", vec!["core", "util-lib"]),
                ("core", vec![]),
                ("util-lib", vec!["core"])
            ]
        );
    }

    #[test]
    fn pyproject_requirements_and_poetry() {
        let root = tree(
            "python",
            &[
                (
                    "svc/pyproject.toml",
                    "[project]\nname = \"My_Service\"\ndependencies = [\"shared-models>=1.0; python_version>'3.9'\", \"requests\"]\n[project.optional-dependencies]\ncli = [\"tools[extra]\"]\n",
                ),
                (
                    "models/pyproject.toml",
                    "[project]\nname = \"shared.models\"\n",
                ),
                (
                    "tools/pyproject.toml",
                    "[tool.poetry]\nname = \"tools\"\n[tool.poetry.group.dev.dependencies]\nshared_models = \"*\"\n",
                ),
            ],
        );
        assert_eq!(
            names(&graph(&root)),
            vec![
                ("shared-models", vec![]),
                ("my-service", vec!["shared-models", "tools"]),
                ("tools", vec!["shared-models"]),
            ]
        );
    }

    #[test]
    fn csproj_project_references() {
        let root = tree(
            "dotnet",
            &[
                ("src/Core/Core.csproj", "<Project></Project>"),
                (
                    "src/Web/Web.csproj",
                    "<Project><ItemGroup><ProjectReference Include=\"..\\Core\\Core.csproj\" /><ProjectReference Include='../Missing/Missing.csproj'/></ItemGroup></Project>",
                ),
            ],
        );
        assert_eq!(
            names(&graph(&root)),
            vec![("Core", vec![]), ("Web", vec!["Core"])]
        );
    }

    #[test]
    fn maps_files_to_the_longest_root() {
        let root = tree(
            "longest",
            &[
                ("package.json", r#"{"name":"outer"}"#),
                ("inner/package.json", r#"{"name":"inner"}"#),
            ],
        );
        let g = graph(&root);
        assert_eq!(g.name(g.module_of(&root.join("inner/src/a.ts"))), "inner");
        assert_eq!(g.name(g.module_of(&root.join("src/a.ts"))), "outer");
        assert_eq!(
            g.name(g.module_of(Path::new("/elsewhere/a.ts"))),
            ROOT_MODULE
        );
    }

    /// `app → core → base`, `svc → base`, `lone`.
    fn layered() -> ModuleGraph {
        let module = |name: &str, deps: &[usize]| Module {
            name: name.into(),
            root: PathBuf::from(format!("/{name}")),
            deps: deps.to_vec(),
            aliases: Vec::new(),
        };
        ModuleGraph::new(vec![
            module("base", &[]),
            module("core", &[0]),
            module("app", &[1]),
            module("svc", &[0]),
            module("lone", &[]),
        ])
    }

    #[test]
    fn tags_each_relationship() {
        let g = layered();
        let s = String::from;
        assert_eq!(
            g.tag(Some(1), Some(1)),
            Tag::SameModule { module: s("core") }
        );
        assert_eq!(
            g.tag(Some(0), Some(1)),
            Tag::Importable {
                copy: s("core"),
                owner: s("base")
            }
        );
        assert_eq!(
            g.tag(Some(2), Some(0)),
            Tag::ImportableIndirect {
                copy: s("app"),
                owner: s("base")
            }
        );
        assert_eq!(g.tag(Some(2), Some(3)), Tag::MoveDown { home: s("base") });
        assert_eq!(g.tag(Some(4), Some(0)), Tag::Boundary);
        assert_eq!(g.tag(None, Some(0)), Tag::Boundary);
        assert!(
            Tag::Importable {
                copy: s("a"),
                owner: s("b")
            }
            .rank()
                < Tag::Boundary.rank()
        );
    }

    #[test]
    fn cluster_home_prefers_a_member_then_a_common_base() {
        let g = layered();
        let s = String::from;
        assert_eq!(
            g.home(&[Some(2), Some(1), Some(0)]),
            Some(Home::Owner { module: s("base") })
        );
        assert_eq!(
            g.home(&[Some(2), Some(3)]),
            Some(Home::MoveDown { module: s("base") })
        );
        assert_eq!(g.home(&[Some(4), Some(1)]), None);
        assert_eq!(g.home(&[Some(1), Some(1)]), None);
        assert_eq!(g.home(&[None, Some(1)]), None);
    }

    #[test]
    fn overrides_define_relocate_rewire_and_ignore() {
        let root = tree(
            "overrides",
            &[
                ("a/package.json", r#"{"name":"a","dependencies":{"b":"1"}}"#),
                ("b/package.json", r#"{"name":"b"}"#),
                ("libs/shared/x.ts", ""),
            ],
        );
        let mut overrides = BTreeMap::new();
        overrides.insert(
            "shared".to_string(),
            ModuleOverride {
                path: Some("libs/shared".into()),
                ..Default::default()
            },
        );
        overrides.insert(
            "a".to_string(),
            ModuleOverride {
                deps: Some(vec!["shared".into()]),
                ..Default::default()
            },
        );
        overrides.insert(
            "b".to_string(),
            ModuleOverride {
                ignore: true,
                ..Default::default()
            },
        );
        let g = ModuleGraph::discover(&root, &overrides).unwrap();
        assert_eq!(names(&g), vec![("a", vec!["shared"]), ("shared", vec![])]);

        let mut bad = BTreeMap::new();
        bad.insert("nope".to_string(), ModuleOverride::default());
        let err = ModuleGraph::discover(&root, &bad).unwrap_err().to_string();
        assert!(err.contains("[modules.nope]"), "{err}");
    }

    #[test]
    fn manifests_sharing_a_directory_are_one_module() {
        let root = tree(
            "same-root",
            &[
                ("core/package.json", r#"{"name":"@x/core"}"#),
                ("core/Cargo.toml", "[package]\nname = \"core-rs\"\n"),
                (
                    "app/package.json",
                    r#"{"name":"@x/app","dependencies":{"@x/core":"1"}}"#,
                ),
                (
                    "cli/Cargo.toml",
                    "[package]\nname = \"cli\"\n[dependencies]\ncore-rs = \"1\"\n",
                ),
            ],
        );
        let g = graph(&root);
        assert_eq!(
            names(&g),
            vec![
                ("@x/app", vec!["@x/core"]),
                ("cli", vec!["@x/core"]),
                ("@x/core", vec![])
            ]
        );
        assert_eq!(g.modules[2].aliases, vec!["core-rs"]);
    }

    #[test]
    fn cargo_path_dependencies_are_implicit_members() {
        let root = tree(
            "cargo-implicit",
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"app\"\n[workspace]\n[dependencies]\nutil = { path = \"util\" }\n",
                ),
                (
                    "util/Cargo.toml",
                    "[package]\nname = \"util\"\n[dependencies]\nleaf = { path = \"../leaf\" }\n",
                ),
                ("leaf/Cargo.toml", "[package]\nname = \"leaf\"\n"),
                ("stray/Cargo.toml", "[package]\nname = \"stray\"\n"),
            ],
        );
        assert_eq!(
            names(&graph(&root)),
            vec![
                ("app", vec!["util"]),
                ("leaf", vec![]),
                ("util", vec!["leaf"])
            ]
        );
    }

    #[test]
    fn pnpm_flow_lists_aliases_and_bin_packages() {
        let root = tree(
            "pnpm-alias",
            &[
                (
                    "pnpm-workspace.yaml",
                    "packages: ['packages/{app,core}', \"bin/*\"]\n",
                ),
                ("packages/core/package.json", r#"{"name":"core"}"#),
                (
                    "packages/app/package.json",
                    r#"{"name":"app","dependencies":{"shared":"workspace:core@*"}}"#,
                ),
                (
                    "bin/tool/package.json",
                    r#"{"name":"tool","devDependencies":{"c":"npm:core@^1","scoped":"npm:@s/none@1"}}"#,
                ),
            ],
        );
        assert_eq!(
            names(&graph(&root)),
            vec![
                ("tool", vec!["core"]),
                ("app", vec!["core"]),
                ("core", vec![])
            ]
        );
        assert_eq!(aliased_package("workspace:*"), None);
        assert_eq!(
            aliased_package("workspace:@x/core@^"),
            Some("@x/core".into())
        );
        assert_eq!(aliased_package("npm:left-pad"), Some("left-pad".into()));
        assert_eq!(aliased_package("^1.0.0"), None);
    }

    #[test]
    fn csproj_comments_and_spaced_attributes() {
        let xml = "<Project><!-- <ProjectReference Include=\"Old.csproj\" /> -->\n<ProjectReference\n  Include = \"New.csproj\" /></Project>";
        assert_eq!(project_references(xml), vec!["New.csproj"]);
    }

    #[test]
    fn files_under_no_module_are_a_boundary_not_one_module() {
        assert_eq!(layered().tag(None, None), Tag::Boundary);
    }

    #[test]
    fn ignored_modules_keep_paths_through_them() {
        let root = tree(
            "splice",
            &[
                (
                    "app/package.json",
                    r#"{"name":"app","dependencies":{"legacy":"1"}}"#,
                ),
                (
                    "legacy/package.json",
                    r#"{"name":"legacy","dependencies":{"base":"1"}}"#,
                ),
                ("base/package.json", r#"{"name":"base"}"#),
            ],
        );
        let mut overrides = BTreeMap::new();
        overrides.insert(
            "legacy".to_string(),
            ModuleOverride {
                ignore: true,
                ..Default::default()
            },
        );
        let g = ModuleGraph::discover(&root, &overrides).unwrap();
        assert_eq!(names(&g), vec![("app", vec!["base"]), ("base", vec![])]);
    }

    #[test]
    fn parses_manifest_fragments() {
        assert_eq!(
            requirement_name("requests[socks]>=2; python_version<'3'"),
            "requests"
        );
        assert_eq!(python_name("My_Package.Core"), "my-package-core");
        assert_eq!(
            pnpm_packages("packages: ['a/*', \"b\"]\nother: 1\n"),
            vec!["a/*", "b"]
        );
        assert_eq!(
            project_references("<ProjectReference Include=\"a.csproj\"><x/></ProjectReference>"),
            vec!["a.csproj"]
        );
    }
}
