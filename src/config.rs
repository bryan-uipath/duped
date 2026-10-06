//! `duped.toml`: per-repo settings, read from the project root or `--config`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::modules::{read_json, read_toml};

pub const FILE_NAME: &str = "duped.toml";

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub scan: ScanConfig,
    pub types: TypesConfig,
    /// Module overrides, keyed by module name.
    pub modules: BTreeMap<String, ModuleOverride>,
    /// Pairs that are duplicated on purpose.
    pub acknowledged: Vec<Acknowledged>,
    /// Rule edits, keyed by language name, e.g. `typescript`.
    pub rules: BTreeMap<String, LanguageRulesConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScanConfig {
    /// Root-relative globs, added to `--exclude`.
    pub exclude: Vec<String>,
    pub include_tests: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TypesConfig {
    pub min_fields: Option<usize>,
    pub min_shared: Option<usize>,
    pub threshold: Option<f64>,
    pub common_field_fraction: Option<f64>,
    /// Type-name globs, added to `--exclude-name`.
    pub exclude_names: Vec<String>,
    /// `false` drops the built-in `*Props` exclusion.
    pub default_excludes: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModuleOverride {
    /// Module root, relative to the project root; defines the module when none was detected.
    pub path: Option<String>,
    /// Replaces the detected internal dependencies, by module name.
    pub deps: Option<Vec<String>>,
    /// Drop the module; its files belong to the next enclosing module.
    pub ignore: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Acknowledged {
    /// A type name, `Scope.Name`, or either prefixed with `module:`.
    pub a: String,
    pub b: String,
    pub reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LanguageRulesConfig {
    pub conventional_members: Option<ListEdit>,
    pub test_files: Option<ListEdit>,
    pub test_dirs: Option<ListEdit>,
}

/// A list setting: an array replaces the defaults, `{ add, remove }` edits them.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ListEdit {
    Replace(Vec<String>),
    Edit(EditList),
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EditList {
    pub add: Vec<String>,
    pub remove: Vec<String>,
}

/// `explicit` (from `--config`) must exist; otherwise `<root>/duped.toml` is optional.
pub fn load(root: &Path, explicit: Option<&Path>) -> Result<Config> {
    let path = match explicit {
        Some(path) => path.to_path_buf(),
        None => {
            let path = root.join(FILE_NAME);
            if !path.is_file() {
                return Ok(Config::default());
            }
            path
        }
    };
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let config: Config =
        toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;
    for (key, value) in [
        ("threshold", config.types.threshold),
        ("common_field_fraction", config.types.common_field_fraction),
    ] {
        if value.is_some_and(|v| !(0.0..=1.0).contains(&v)) {
            bail!("{}: [types] {key} must be between 0 and 1", path.display());
        }
    }
    Ok(config)
}

/// The directory that holds `duped.toml` and the module manifests for a scan.
///
/// Searches from `start` (the canonical scanned directory) up to the enclosing git checkout,
/// never past it: the nearest directory with a `duped.toml`, else the nearest workspace root
/// (`pnpm-workspace.yaml`, `package.json` with `workspaces`, a Cargo `[workspace]`, a uv
/// workspace or a `.sln`), else the checkout itself. Outside a checkout it is `start`.
pub fn project_root(start: &Path) -> PathBuf {
    let Some(checkout) = start.ancestors().find(|dir| dir.join(".git").exists()) else {
        return start.to_path_buf();
    };
    let within = || {
        start
            .ancestors()
            .take_while(|dir| dir.starts_with(checkout))
    };
    within()
        .find(|dir| dir.join(FILE_NAME).is_file())
        .or_else(|| within().find(|dir| is_workspace_root(dir)))
        .unwrap_or(checkout)
        .to_path_buf()
}

fn is_workspace_root(dir: &Path) -> bool {
    let toml_has = |name: &str, path: &[&str]| {
        read_toml(&dir.join(name)).is_some_and(|table| lookup(&table, path).is_some())
    };
    dir.join("pnpm-workspace.yaml").is_file()
        || read_json(&dir.join("package.json")).is_some_and(|json| json.get("workspaces").is_some())
        || toml_has("Cargo.toml", &["workspace"])
        || toml_has("pyproject.toml", &["tool", "uv", "workspace"])
        || std::fs::read_dir(dir).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|e| e.path().extension().is_some_and(|ext| ext == "sln"))
        })
}

/// `table[a][b]…`, when every step is a table.
pub fn lookup<'t>(table: &'t toml::Table, path: &[&str]) -> Option<&'t toml::Value> {
    let (first, rest) = path.split_first()?;
    let value = table.get(*first)?;
    if rest.is_empty() {
        Some(value)
    } else {
        lookup(value.as_table()?, rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("duped-config-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn parses_every_section() {
        let dir = temp("parse");
        std::fs::write(
            dir.join(FILE_NAME),
            r#"
[scan]
exclude = ["examples"]
include_tests = true

[types]
min_fields = 5
threshold = 0.8
exclude_names = ["*Dto"]
default_excludes = false

[modules.core]
path = "libs/core"
deps = ["base"]

[modules.legacy]
ignore = true

[[acknowledged]]
a = "canvas:Row"
b = "Row"
reason = "canvas can't depend on evals"

[rules.typescript]
conventional_members = { add = ["dispose"] }
test_dirs = ["qa"]
"#,
        )
        .unwrap();
        let config = load(&dir, None).unwrap();
        assert_eq!(config.scan.exclude, vec!["examples"]);
        assert!(config.scan.include_tests);
        assert_eq!(
            (config.types.min_fields, config.types.threshold),
            (Some(5), Some(0.8))
        );
        assert_eq!(config.types.min_shared, None);
        assert_eq!(config.types.default_excludes, Some(false));
        assert_eq!(config.modules["core"].path.as_deref(), Some("libs/core"));
        assert!(config.modules["legacy"].ignore);
        assert_eq!(config.acknowledged[0].a, "canvas:Row");
        let ts = &config.rules["typescript"];
        assert!(
            matches!(&ts.conventional_members, Some(ListEdit::Edit(e)) if e.add == ["dispose"])
        );
        assert!(matches!(&ts.test_dirs, Some(ListEdit::Replace(v)) if v == &["qa"]));
    }

    #[test]
    fn missing_file_is_default_but_explicit_must_exist() {
        let dir = temp("missing");
        assert!(load(&dir, None).unwrap().acknowledged.is_empty());
        assert!(load(&dir, Some(&dir.join("nope.toml"))).is_err());
    }

    #[test]
    fn rejects_typos_and_bad_fractions() {
        let dir = temp("typo");
        std::fs::write(dir.join(FILE_NAME), "[types]\nthreshhold = 0.5\n").unwrap();
        assert!(load(&dir, None).is_err());
        std::fs::write(dir.join(FILE_NAME), "[types]\nthreshold = 2.0\n").unwrap();
        let err = format!("{:#}", load(&dir, None).unwrap_err());
        assert!(err.contains("between 0 and 1"), "{err}");
    }

    #[test]
    fn finds_project_root_within_the_checkout() {
        let dir = temp("root");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("ws/packages/a/src")).unwrap();
        std::fs::write(
            dir.join("ws/pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n",
        )
        .unwrap();
        let scan = dir.join("ws/packages/a/src");
        assert_eq!(project_root(&scan), dir.join("ws"));

        std::fs::write(dir.join("ws/packages/a/duped.toml"), "").unwrap();
        assert_eq!(project_root(&scan), dir.join("ws/packages/a"));

        // Without a workspace marker or config, the checkout is the root.
        let plain = temp("plain");
        std::fs::create_dir_all(plain.join(".git")).unwrap();
        std::fs::create_dir_all(plain.join("src")).unwrap();
        assert_eq!(project_root(&plain.join("src")), plain);
    }
}
