//! End-to-end: `duped coupling` over temporary TypeScript, Python and Rust workspaces, and
//! `--base` / `--rev` over a git repository.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

/// `(importer, target, bypass)` of every reported edge.
fn edges(report: &Value) -> Vec<(String, String, u64)> {
    report["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            let s = |k: &str| e[k].as_str().unwrap().to_string();
            (s("importer"), s("target"), e["bypass"].as_u64().unwrap())
        })
        .collect()
}

fn boundary<'a>(report: &'a Value, dir: &str) -> &'a Value {
    report["boundaries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["dir"] == dir)
        .unwrap_or_else(|| panic!("no boundary {dir}: {report}"))
}

fn leaks(report: &Value) -> Vec<String> {
    report["leaks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            format!(
                "{}#{}",
                l["file"].as_str().unwrap(),
                l["symbol"].as_str().unwrap()
            )
        })
        .collect()
}

fn e(importer: &str, target: &str, bypass: u64) -> (String, String, u64) {
    (importer.to_string(), target.to_string(), bypass)
}

/// `app` imports `foo` through its entry, a subpath, a cross-package re-export and a
/// nested `internal/` boundary.
#[test]
fn typescript_workspace_scores_bypass_depth_and_leaks() {
    let root = temp("ts");
    write(
        &root,
        "pnpm-workspace.yaml",
        "packages:\n  - 'packages/*'\n",
    );
    write(&root, "package.json", r#"{"name":"mono","private":true}"#);
    write(
        &root,
        "packages/foo/package.json",
        r#"{"name":"@m/foo","main":"src/index.ts"}"#,
    );
    write(
        &root,
        "packages/foo/src/index.ts",
        "export { api } from './api';\nexport { util } from '@m/bar/util';\n",
    );
    write(
        &root,
        "packages/foo/src/api.ts",
        "export function api() {}\n",
    );
    write(&root, "packages/foo/src/internal/index.ts", "export {};\n");
    write(
        &root,
        "packages/foo/src/internal/deep.ts",
        "export function deep() {}\n",
    );
    write(&root, "packages/bar/package.json", r#"{"name":"@m/bar"}"#);
    write(
        &root,
        "packages/bar/src/index.ts",
        "export function bar() {}\n",
    );
    write(
        &root,
        "packages/bar/src/util.ts",
        "export function util() {}\n",
    );
    write(&root, "packages/app/package.json", r#"{"name":"@m/app"}"#);
    write(
        &root,
        "packages/app/src/main.ts",
        "import { api, util } from '@m/foo';\n\
         import { api as direct } from '@m/foo/api';\n\
         import { deep } from '../../foo/src/internal/deep.js';\n",
    );

    let report = json(&root, &[]);
    let main = "packages/app/src/main.ts";
    assert_eq!(
        edges(&report),
        vec![
            // foo, then `internal/`, past its entry.
            e(main, "packages/foo/src/internal/deep.ts", 2),
            // `util` resolves through foo's entry to a file in another package.
            e(main, "packages/bar/src/util.ts", 1),
            e(main, "packages/foo/src/api.ts", 1),
            e("packages/foo/src/index.ts", "packages/bar/src/util.ts", 1),
        ]
    );
    assert_eq!(report["edges"][1]["via"], "packages/foo/src/index.ts");
    // `api` through the entry is bypass 0, and `./api` is re-exported within foo.
    let all = json(&root, &["--all-edges"]);
    assert!(edges(&all).contains(&e(main, "packages/foo/src/index.ts", 0)));
    assert_eq!(
        leaks(&report),
        vec![
            "packages/bar/src/util.ts#util",
            "packages/foo/src/api.ts#api",
            "packages/foo/src/internal/deep.ts#deep",
        ]
    );

    let totals = &report["totals"];
    // Five cross-boundary symbols, four bypassing at depths 2, 1, 1, 1.
    assert_eq!(totals["crossBoundarySymbols"], 5);
    assert_eq!(totals["bypassSymbols"], 4);
    assert_eq!(totals["bypassRate"], 0.8);
    assert_eq!(totals["bypassCost"], 7);
    assert_eq!(totals["meanBypassDepth"], 1.25);

    let foo = boundary(&report, "packages/foo");
    assert_eq!(foo["entries"][0], "packages/foo/src/index.ts");
    assert_eq!(foo["surface"], serde_json::json!(["api", "util"]));
    assert_eq!(foo["unused"], serde_json::json!([]));
    // `api` is used from outside, so nothing foo declares is hidden; under 5 symbols, hiding is 1.
    assert_eq!(
        (foo["declared"].as_u64(), foo["hidden"].as_u64()),
        (Some(1), Some(0))
    );
    assert_eq!(foo["hiding"], 1.0);
    // 3 incoming symbols, 2 past the entry: score ∛(1/3 · 1 · 1).
    assert_eq!(
        (foo["incoming"].as_u64(), foo["bypassIn"].as_u64()),
        (Some(3), Some(2))
    );
    let score = foo["score"].as_f64().unwrap();
    assert!((score - (1.0f64 / 3.0).cbrt()).abs() < 1e-9, "{score}");

    let text = text(&root, &["--folder", "packages/foo"]);
    assert!(text.starts_with("score "), "{text}");
    assert!(
        text.contains("bypassRate 0.80 (4 of 5 cross-boundary symbols)"),
        "{text}"
    );
    // foo (0.69) and its `internal/` (0.00), one declaration each.
    assert!(text.contains("folder packages/foo: score 0.35"), "{text}");
    assert!(
        text.contains(
            "packages/app/src/main.ts -> packages/foo/src/internal/deep.ts  ups=1 downs=2 bypass=2"
        ),
        "{text}"
    );
    assert!(
        text.contains("packages/foo/src/internal/deep.ts#deep <- packages/app/src/main.ts"),
        "{text}"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

/// `svc` exposes subpaths through an `exports` pattern into `dist/`, `lib` a `main` in
/// `dist/`, and `pub` is published but imported by nothing here.
fn packages(test: &str) -> PathBuf {
    let root = temp(test);
    write(
        &root,
        "pnpm-workspace.yaml",
        "packages:\n  - 'packages/*'\n",
    );
    write(&root, "package.json", r#"{"name":"mono","private":true}"#);
    write(
        &root,
        "packages/svc/package.json",
        r#"{"name":"@m/svc","exports":{"./*":{"types":"./dist/svc/src/*/index.d.ts","import":"./dist/*/index.js"}}}"#,
    );
    write(
        &root,
        "packages/svc/src/debug/index.ts",
        "export { run } from './run';\n",
    );
    write(
        &root,
        "packages/svc/src/debug/run.ts",
        "export function run() {}\n",
    );
    write(
        &root,
        "packages/svc/src/util/index.ts",
        "export function util() {}\n",
    );
    write(
        &root,
        "packages/lib/package.json",
        r#"{"name":"@m/lib","main":"./dist/index.cjs","types":"./dist/index.d.ts"}"#,
    );
    write(
        &root,
        "packages/lib/src/index.ts",
        "export function lib() {}\n",
    );
    write(
        &root,
        "packages/lib/src/extra.ts",
        "export function extra() {}\n",
    );
    write(
        &root,
        "packages/pub/package.json",
        r#"{"name":"@m/pub","main":"index.ts"}"#,
    );
    write(
        &root,
        "packages/pub/index.ts",
        "export function published() {}\n",
    );
    write(&root, "packages/app/package.json", r#"{"name":"@m/app"}"#);
    write(
        &root,
        "packages/app/src/main.ts",
        "import { run } from '@m/svc/debug';\nimport { lib } from '@m/lib';\nimport { extra } from '@m/lib/extra';\n",
    );
    root
}

#[test]
fn package_exports_targets_are_entries() {
    let root = packages("exports");
    let report = json(&root, &[]);
    // `@m/svc/debug` and `@m/lib` resolve, via `dist/` → `src/`, to entries: bypass 0.
    assert_eq!(
        edges(&report),
        vec![e(
            "packages/app/src/main.ts",
            "packages/lib/src/extra.ts",
            1
        )]
    );
    let svc = boundary(&report, "packages/svc");
    assert_eq!(
        svc["entries"],
        serde_json::json!([
            "packages/svc/src/debug/index.ts",
            "packages/svc/src/util/index.ts"
        ])
    );
    assert_eq!(svc["surface"], serde_json::json!(["run", "util"]));
    assert_eq!(svc["unused"], serde_json::json!(["util"]));
    assert_eq!(
        boundary(&report, "packages/lib")["entries"][0],
        "packages/lib/src/index.ts"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn boundaries_nobody_imports_have_neutral_unused_surface() {
    let root = packages("consumers");
    let report = json(&root, &[]);
    let published = boundary(&report, "packages/pub");
    assert_eq!(published["noConsumers"], true);
    // The raw count stays; the score ignores it.
    assert_eq!(published["unusedSurface"], 1.0);
    assert_eq!(published["score"], 1.0);
    assert_eq!(boundary(&report, "packages/svc")["noConsumers"], false);
    let text = text(&root, &[]);
    assert!(
        text.contains("1.00  packages/pub (no consumers)  surface 1 (unused 1)"),
        "{text}"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn python_resolves_absolute_relative_and_private_modules() {
    let root = temp("py");
    write(
        &root,
        "libs/proj/pyproject.toml",
        "[project]\nname = \"proj\"\n",
    );
    write(
        &root,
        "libs/proj/src/proj/__init__.py",
        "from .core import run\n",
    );
    write(
        &root,
        "libs/proj/src/proj/core.py",
        "def run():\n    pass\n",
    );
    write(
        &root,
        "libs/proj/src/proj/_impl.py",
        "def helper():\n    pass\n",
    );
    write(&root, "libs/proj/src/proj/sub/__init__.py", "");
    write(
        &root,
        "libs/proj/src/proj/sub/mod.py",
        "def thing():\n    pass\n",
    );
    write(
        &root,
        "tools/cli.py",
        "from proj import run\nfrom proj.core import run as r\nfrom proj._impl import helper\nimport proj.sub.mod\n",
    );
    let report = json(&root, &[]);
    let cli = "tools/cli.py";
    assert_eq!(
        edges(&report),
        vec![
            // A `_private` module is one more boundary.
            e(cli, "libs/proj/src/proj/_impl.py", 2),
            e(cli, "libs/proj/src/proj/sub/mod.py", 2),
            e(cli, "libs/proj/src/proj/core.py", 1),
        ]
    );
    // The project's one package is its entry; `run` is surfaced by re-export.
    let proj = boundary(&report, "libs/proj");
    assert_eq!(proj["entries"][0], "libs/proj/src/proj/__init__.py");
    assert_eq!(proj["surface"], serde_json::json!(["run"]));
    assert_eq!(report["totals"]["crossBoundarySymbols"], 4);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn rust_resolves_crates_modules_and_globs() {
    let root = temp("rs");
    write(
        &root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/*\"]\n",
    );
    write(
        &root,
        "crates/core/Cargo.toml",
        "[package]\nname = \"core-lib\"\n",
    );
    write(
        &root,
        "crates/core/src/lib.rs",
        "pub mod api;\npub mod internal;\npub use api::Client;\n",
    );
    write(
        &root,
        "crates/core/src/api.rs",
        "pub struct Client;\npub(crate) fn hidden() {}\n",
    );
    write(&root, "crates/core/src/internal/mod.rs", "pub mod deep;\n");
    write(
        &root,
        "crates/core/src/internal/deep.rs",
        "pub fn deep() {}\n",
    );
    write(
        &root,
        "crates/app/Cargo.toml",
        "[package]\nname = \"app\"\n[dependencies]\ncore-lib = { path = \"../core\" }\n",
    );
    write(
        &root,
        "crates/app/src/main.rs",
        "mod cmd;\nuse core_lib::Client;\nuse core_lib::{api::Client as C2, internal::deep::deep};\nuse crate::cmd::*;\nuse std::fmt;\nfn main() {}\n",
    );
    write(&root, "crates/app/src/cmd.rs", "pub fn run() {}\n");
    let report = json(&root, &["--all-edges"]);
    let main = "crates/app/src/main.rs";
    assert_eq!(
        edges(&report),
        vec![
            e(main, "crates/core/src/internal/deep.rs", 2),
            e(main, "crates/core/src/api.rs", 1),
            // `pub use api::Client` keeps `Client` at the crate root.
            e(main, "crates/core/src/lib.rs", 0),
        ]
    );
    // The glob into a sibling module is an edge inside one boundary: counted, not listed.
    assert_eq!(report["totals"]["edges"], 5);
    let core = boundary(&report, "crates/core");
    assert_eq!(core["surface"], serde_json::json!(["Client"]));
    assert_eq!(leaks(&report).len(), 2);
    std::fs::remove_dir_all(&root).unwrap();
}

/// The `billing` / `orders` regression fixture, at `main`.
fn billing(test: &str) -> PathBuf {
    let root = temp(test);
    write(
        &root,
        "src/billing/index.ts",
        "export { charge } from './charge';\n",
    );
    write(
        &root,
        "src/billing/charge.ts",
        "export const charge = () => fmt(1);\n",
    );
    write(&root, "src/billing/internal/index.ts", "export {};\n");
    write(
        &root,
        "src/billing/internal/fmt.ts",
        "export const fmt = (n) => `$${n}`;\n",
    );
    write(
        &root,
        "src/orders/index.ts",
        "export { place } from './place';\n",
    );
    write(
        &root,
        "src/orders/place.ts",
        "import { charge } from '../billing/charge';\n\
         import { fmt } from '../billing/internal/fmt';\n\
         export const place = () => charge() + fmt(2);\n",
    );
    git(&root, &["init", "-q", "-b", "main"]);
    commit(&root, "base");
    root
}

/// Commits everything on a new branch and returns the coupling JSON against `main`.
fn branch(root: &Path, name: &str) -> Value {
    git(root, &["checkout", "-q", "-b", name]);
    commit(root, name);
    let report = json(root, &["--base", "main"]);
    git(root, &["checkout", "-q", "main"]);
    report
}

fn names(list: &Value) -> Vec<String> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|c| {
            format!(
                "{}#{}",
                c["importer"].as_str().unwrap(),
                c["symbol"].as_str().unwrap()
            )
        })
        .collect()
}

#[test]
fn base_delta_credits_real_fixes_only() {
    if !has_git() {
        return;
    }
    let root = billing("delta");
    let base = json(&root, &[]);
    assert_eq!(base["totals"]["bypassRate"], 1.0);
    assert_eq!(base["totals"]["unusedSurfaceCount"], 2);

    // Good: through billing's entry, and `fmt` moved behind a new module's entry.
    write(
        &root,
        "src/orders/place.ts",
        "import { charge } from '../billing';\nimport { fmt } from '../shared/format';\nexport const place = () => charge() + fmt(2);\n",
    );
    std::fs::create_dir_all(root.join("src/shared/format")).unwrap();
    let moved = ["src/billing/internal/fmt.ts", "src/shared/format/fmt.ts"].map(|f| root.join(f));
    std::fs::rename(&moved[0], &moved[1]).unwrap();
    write(
        &root,
        "src/shared/format/index.ts",
        "export { fmt } from './fmt';\n",
    );
    let good = branch(&root, "good");
    assert_eq!(good["totals"]["bypassRate"], 0.0);
    assert_eq!(good["diff"]["totals"]["bypassRate"], 1.0);
    assert_eq!(
        names(&good["diff"]["improved"]),
        vec!["src/orders/place.ts#charge", "src/orders/place.ts#fmt"]
    );
    assert_eq!(good["diff"]["removedWithoutMove"], serde_json::json!([]));
    let surface = &good["diff"]["surface"][0];
    assert_eq!(
        (surface["dir"].as_str(), surface["newModule"].as_bool()),
        (Some("src/shared/format"), Some(true))
    );

    // Bad A: `fmt` pasted into the importer. The import is gone, but `fmt` never moved.
    write(
        &root,
        "src/orders/place.ts",
        "import { charge } from '../billing/charge';\nconst fmt = (n) =>\n  `$${n}`;\nexport const place = () => charge() + fmt(2);\n",
    );
    let pasted = branch(&root, "paste");
    assert_eq!(pasted["totals"]["bypassRate"], 1.0);
    assert_eq!(pasted["diff"]["improved"], serde_json::json!([]));
    let removed = &pasted["diff"]["removedWithoutMove"];
    assert_eq!(names(removed), vec!["src/orders/place.ts#fmt"]);
    assert_eq!(removed[0]["grew"], true);
    assert_eq!(removed[0]["base"]["bypass"], 2);

    // Bad B: a barrel re-exporting every file. Importers use the entry, but the barrel itself
    // reaches past `internal/`'s entry, and `internal/` has nothing to hide.
    write(
        &root,
        "src/billing/index.ts",
        "export * from './charge';\nexport * from './internal';\nexport * from './internal/fmt';\n",
    );
    write(
        &root,
        "src/orders/place.ts",
        "import { charge, fmt } from '../billing';\nexport const place = () => charge() + fmt(2);\n",
    );
    let barrel = branch(&root, "barrel");
    assert_eq!(barrel["totals"]["bypassRate"], 0.25);
    assert_eq!(
        boundary(&barrel, "src/billing")["surface"],
        serde_json::json!(["charge", "fmt"])
    );
    assert_eq!(
        barrel["diff"]["surface"][0]["added"],
        serde_json::json!(["fmt"])
    );
    assert_eq!(
        names(&barrel["diff"]["regressed"]),
        vec!["src/billing/index.ts#*"]
    );

    // Bad C: `internal/` loses its entry but stays a boundary by name: nothing changes.
    std::fs::remove_file(root.join("src/billing/internal/index.ts")).unwrap();
    let unmarked = branch(&root, "unmarked");
    assert_eq!(unmarked["totals"]["bypassRate"], 1.0);
    assert_eq!(unmarked["totals"]["bypassCost"], 5);
    assert_eq!(unmarked["diff"]["improved"], serde_json::json!([]));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn base_delta_flags_new_modules_and_new_bypass_edges() {
    if !has_git() {
        return;
    }
    let root = billing("surface");
    git(&root, &["checkout", "-q", "-b", "feature"]);
    // A new module with its own surface, and a new export on an existing one.
    write(
        &root,
        "src/notify/index.ts",
        "export { send } from './send';\n",
    );
    write(&root, "src/notify/send.ts", "export function send() {}\n");
    write(
        &root,
        "src/billing/index.ts",
        "export { charge } from './charge';\nexport { refund } from './refund';\n",
    );
    write(
        &root,
        "src/billing/refund.ts",
        "export function refund() {}\n",
    );
    let place = std::fs::read_to_string(root.join("src/orders/place.ts")).unwrap();
    write(
        &root,
        "src/orders/place.ts",
        &format!("import {{ send }} from '../notify/send';\n{place}"),
    );
    commit(&root, "feature");

    let report = json(&root, &["--base", "main"]);
    let diff = &report["diff"];
    assert_eq!(diff["base"], "main");
    assert_eq!(names(&diff["regressed"]), vec!["src/orders/place.ts#send"]);
    assert_eq!(diff["regressed"][0]["head"]["bypass"], 1);
    let surface: Vec<(&str, bool, &Value)> = diff["surface"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["dir"].as_str().unwrap(),
                s["newModule"].as_bool().unwrap(),
                &s["added"],
            )
        })
        .collect();
    assert_eq!(
        surface,
        vec![
            ("src/billing", false, &serde_json::json!(["refund"])),
            ("src/notify", true, &serde_json::json!(["send"])),
        ]
    );
    assert_eq!(diff["leaks"]["added"][0]["symbol"], "send");
    // Head totals are absolute: three bypassing symbols now.
    assert_eq!(report["totals"]["bypassSymbols"], 3);

    let text = text(&root, &["--base", "main"]);
    assert!(
        text.contains(
            "Regressed (1):\n  src/orders/place.ts#send  gone -> src/notify/send.ts bypass=1"
        ),
        "{text}"
    );
    assert!(text.contains("  src/notify (new module): +send"), "{text}");
    assert!(text.contains("  src/billing: +refund"), "{text}");

    // `--rev` scans a commit instead of the working tree; paths line up with head's.
    let at_main = json(&root, &["--rev", "main"]);
    assert_eq!(at_main["totals"], diff["totals"]);
    let combined = json(&root, &["--rev", "main", "--base", "main"]);
    assert_eq!(combined["diff"]["regressed"], serde_json::json!([]));
    // Worktrees are removed afterwards.
    let worktrees = Command::new("git")
        .args(["worktree", "list"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&worktrees.stdout).lines().count(),
        1
    );
    assert!(!duped(&root, &["--rev", "no-such-ref"]).status.success());
    std::fs::remove_dir_all(&root).unwrap();
}

fn duped(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_duped"))
        .arg("coupling")
        .arg(dir)
        .args(args)
        .output()
        .unwrap()
}

fn json(dir: &Path, args: &[&str]) -> Value {
    let output = duped(dir, &[args, &["--json"]].concat());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn text(dir: &Path, args: &[&str]) -> String {
    let output = duped(dir, args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn temp(test: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("duped-coupling-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root.canonicalize().unwrap()
}

fn has_git() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

fn write(root: &Path, file: &str, text: &str) {
    let path = root.join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn commit(root: &Path, message: &str) {
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", message]);
}

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(root)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}
