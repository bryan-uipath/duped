//! End-to-end: `skills/duped/scripts/scan.mjs` over a temporary git repo. Skipped without node or git.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

const BODY: &str = "export function NAME(items: Item[], limit: number) {\n\
    const seen = new Set<string>();\n\
    const out: Item[] = [];\n\
    for (const item of items) {\n\
      if (seen.has(item.id) || item.hidden) continue;\n\
      seen.add(item.id);\n\
      out.push({ ...item, label: item.label.trim().toLowerCase() });\n\
      if (out.length >= limit) break;\n\
    }\n\
    return out.sort((a, b) => a.label.localeCompare(b.label));\n\
  }\n";

/// `main` has `sub/a/list.ts`; branch `feature` adds a renamed copy at `sub/b/café.ts`
/// and an unrelated copy outside `sub/`.
fn repo(test: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("duped-scan-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    write(&root, "sub/a/list.ts", &BODY.replace("NAME", "uniqueItems"));
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "base"]);
    git(&root, &["checkout", "-q", "-b", "feature"]);
    write(&root, "sub/b/café.ts", &BODY.replace("NAME", "dedupeRows"));
    write(&root, "other/copy.ts", &BODY.replace("NAME", "outsideCopy"));
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "copy"]);
    root
}

#[test]
fn diff_mode_keeps_pairs_touching_non_ascii_changed_files() {
    let Some(root) = available().then(|| repo("diff")) else {
        return;
    };
    let report = json(&scan(&[
        &root.join("sub"),
        "--base".as_ref(),
        "main".as_ref(),
    ]));
    assert_eq!(report["mode"], "diff");
    let pairs: Vec<(&str, &str)> = report["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| (g["a"].as_str().unwrap(), g["b"].as_str().unwrap()))
        .collect();
    // Scoped to `sub/`, so the copy under `other/` isn't scanned.
    assert_eq!(pairs, vec![("a/list.ts", "b/café.ts")]);
    assert_eq!(report["groups"][0]["changed"][0], "b/café.ts");
    assert_eq!(report["detectors"]["bodies"]["kept"], 1);
}

#[test]
fn all_mode_needs_no_base_and_rejects_bad_top() {
    let Some(root) = available().then(|| repo("all")) else {
        return;
    };
    let report = json(&scan(&[
        root.as_path(),
        "--all".as_ref(),
        "--top".as_ref(),
        "1".as_ref(),
    ]));
    assert_eq!(report["filePairs"], 3);
    assert_eq!(report["truncated"], true);
    assert_eq!(report["groups"].as_array().unwrap().len(), 1);

    let rejected = scan(&[
        root.as_path(),
        "--all".as_ref(),
        "--top".as_ref(),
        "abc".as_ref(),
    ]);
    assert_eq!(rejected.status.code(), Some(2));
}

fn scan(args: &[&Path]) -> Output {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/duped/scripts/scan.mjs");
    let bin = Path::new(env!("CARGO_BIN_EXE_duped"))
        .parent()
        .unwrap()
        .to_path_buf();
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let mut command = Command::new("node");
    command
        .arg(script)
        .arg("--repo")
        .args(args)
        .env("PATH", path);
    command.output().expect("node runs")
}

fn json(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn available() -> bool {
    let ok = |bin: &str| Command::new(bin).arg("--version").output().is_ok();
    ok("node") && ok("git")
}

fn write(root: &Path, file: &str, text: &str) {
    let path = root.join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
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
