//! End-to-end: `duped report` over temporary directories, with and without git.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

const LIST: &str = "export function NAME(items: Item[], limit: number) {\n\
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

const RETRY: &str = "export async function NAME(url: string, attempts = 3) {\n\
    let delay = 250;\n\
    for (let i = 0; i < attempts; i++) {\n\
      const res = await fetch(url, { headers: { accept: 'application/json' } });\n\
      if (res.ok) return await res.json();\n\
      if (res.status < 500) throw new Error(`request failed: ${res.status}`);\n\
      await new Promise((r) => setTimeout(r, delay));\n\
      delay = Math.min(delay * 2, 4000);\n\
    }\n\
    throw new Error('retries exhausted');\n\
  }\n";

const TOTALS: &str = "export function NAME(lines: Line[], region: string) {\n\
    const rate = region === 'eu' ? 0.2 : region === 'us' ? 0.07 : 0;\n\
    let net = 0;\n\
    let tax = 0;\n\
    for (const line of lines) {\n\
      const amount = line.quantity * line.unitPrice - (line.discount ?? 0);\n\
      net += amount;\n\
      tax += Math.round(amount * rate * 100) / 100;\n\
    }\n\
    return { net, tax, gross: net + tax, count: lines.length };\n\
  }\n";

const ORDER: &str = "export interface Order { id: string; total: number; status: string; placedAt: Date }\n\
    export function orderTotal(order: Order): number { return order.total; }\n";

/// `main` has `sub/a/list.ts`; branch `feature` adds a renamed copy at `sub/b/café.ts` and an
/// unrelated copy outside `sub/`; an untracked copy sits at `sub/c/draft.ts`.
fn repo(test: &str) -> PathBuf {
    let root = temp(test);
    write(&root, "sub/a/list.ts", &LIST.replace("NAME", "uniqueItems"));
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "base"]);
    git(&root, &["checkout", "-q", "-b", "feature"]);
    write(&root, "sub/b/café.ts", &LIST.replace("NAME", "dedupeRows"));
    write(&root, "other/copy.ts", &LIST.replace("NAME", "outsideCopy"));
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "copy"]);
    write(&root, "sub/c/draft.ts", &LIST.replace("NAME", "draftRows"));
    root
}

#[test]
fn diff_mode_keeps_pairs_touching_changed_files_under_the_scanned_path() {
    if !has_git() {
        return;
    }
    let root = repo("diff");
    let report = json(&root.join("sub"), &["--base", "main"]);
    assert_eq!(report["mode"], "diff");
    assert_eq!(report["base"], "main");
    // `other/copy.ts` is outside `sub/`: neither scanned nor counted as changed.
    assert_eq!(report["changedFiles"], 2);
    let pairs: Vec<(&str, &str, Vec<&str>)> = groups(&report)
        .iter()
        .map(|g| {
            let changed = g["changed"].as_array().unwrap().iter();
            (
                g["a"].as_str().unwrap(),
                g["b"].as_str().unwrap(),
                changed.map(|c| c.as_str().unwrap()).collect(),
            )
        })
        .collect();
    assert_eq!(
        pairs,
        vec![
            ("a/list.ts", "b/café.ts", vec!["b/café.ts"]),
            ("a/list.ts", "c/draft.ts", vec!["c/draft.ts"]),
            ("b/café.ts", "c/draft.ts", vec!["b/café.ts", "c/draft.ts"]),
        ]
    );
    assert_eq!(report["detectors"]["bodies"]["kept"], 3);

    // Diffing against the branch itself leaves only the untracked file.
    let report = json(&root.join("sub"), &["--base", "HEAD"]);
    assert_eq!(report["changedFiles"], 1);
    assert_eq!(groups(&report).len(), 2);

    // A single-file PATH counts only itself, not its changed siblings.
    write(&root, "sub/b/notes.md", "draft");
    let report = json(&root.join("sub/b/café.ts"), &["--base", "main"]);
    assert_eq!(report["changedFiles"], 1);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn all_mode_needs_no_git_and_truncates_to_top() {
    let root = temp("all");
    for (file, name) in [("a.ts", "one"), ("b.ts", "two"), ("c.ts", "three")] {
        write(&root, file, &LIST.replace("NAME", name));
    }
    let report = json(&root, &["--top", "1"]);
    assert_eq!(report["mode"], "all");
    assert_eq!(report["filePairs"], 3);
    assert_eq!(report["truncated"], true);
    assert_eq!(groups(&report).len(), 1);
    assert!(report.get("base").is_none() && report.get("changedFiles").is_none());
    assert!(report["groups"][0].get("changed").is_none());
    assert_eq!(report["detectors"]["bodies"]["total"], 3);

    assert_eq!(duped(&root, &["--top", "abc"]).status.code(), Some(2));
    let outside = duped(&root, &["--base", "no-such-ref"]);
    assert!(!outside.status.success());
    assert!(String::from_utf8_lossy(&outside.stderr).contains("git"));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn groups_rank_by_agreeing_analyses_then_finding_count() {
    let root = temp("rank");
    // `p/`: two functions copied under new names (bodies only: 2 pairs and the file pair).
    let p = |a, b| format!("{}{}", RETRY.replace("NAME", a), TOTALS.replace("NAME", b));
    write(&root, "p/one.ts", &p("fetchJson", "invoiceTotals"));
    write(&root, "p/two.ts", &p("getWithRetry", "sumLines"));
    // `q/`: a type and a function redeclared as is (names and types, one finding each).
    write(&root, "q/one.ts", ORDER);
    write(&root, "q/two.ts", ORDER);
    // `r/`: one copied function (bodies, one finding).
    write(&root, "r/one.ts", &LIST.replace("NAME", "uniqueItems"));
    write(&root, "r/two.ts", &LIST.replace("NAME", "dedupeRows"));

    let report = json(&root, &[]);
    let summary: Vec<(&str, Vec<&str>, usize)> = groups(&report)
        .iter()
        .map(|g| {
            let detectors = g["detectors"].as_array().unwrap().iter();
            (
                g["a"].as_str().unwrap(),
                detectors.map(|d| d.as_str().unwrap()).collect(),
                g["findings"].as_array().unwrap().len(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            ("q/one.ts", vec!["names", "types"], 2),
            ("p/one.ts", vec!["bodies"], 3),
            ("r/one.ts", vec!["bodies"], 1),
        ]
    );
    // Within a group, by each analysis's rank: the file pair sums its functions' shingles.
    let kinds: Vec<&str> = report["groups"][1]["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, vec!["file", "function", "function"]);
    let order = &report["groups"][0]["findings"];
    assert_eq!(order[0]["detector"], "names");
    assert_eq!(
        order[0]["evidence"],
        "= Order (exported) (:1 :1), = orderTotal (exported) (:2 :2)"
    );
    assert_eq!(order[1]["a"]["name"], "Order");
    assert!(
        order[1]["evidence"]
            .as_str()
            .unwrap()
            .starts_with("exact, 1 field overlap")
    );

    let text = text(&root);
    assert!(
        text.starts_with("3 file pairs (bodies 4; imports 0; names 1; types 1).\n"),
        "{text}"
    );
    assert!(text.contains("\n1. names + types: 2 findings\n   q/one.ts\n   q/two.ts\n"));
    assert!(
        text.contains("   - bodies file: 2 function pairs"),
        "{text}"
    );
    assert!(text.contains("(uniqueItems:1 ~ dedupeRows:1)"), "{text}");
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_failed_analysis_is_reported_and_the_others_still_run() {
    let root = temp("failed");
    write(&root, "duped.toml", "[types]\nexclude_names = [\"[bad\"]\n");
    write(&root, "a.ts", &LIST.replace("NAME", "one"));
    write(&root, "b.ts", &LIST.replace("NAME", "two"));
    let report = json(&root, &[]);
    assert_eq!(report["detectors"]["types"]["status"], "failed");
    assert!(
        report["detectors"]["types"]["error"]
            .as_str()
            .unwrap()
            .contains("[bad"),
        "{report}"
    );
    assert_eq!(report["detectors"]["bodies"]["status"], "ok");
    assert_eq!(groups(&report).len(), 1);

    let text = text(&root);
    assert!(
        text.contains("types failed: invalid --exclude-name glob"),
        "{text}"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

fn duped(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_duped"))
        .arg("report")
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

fn text(dir: &Path) -> String {
    String::from_utf8(duped(dir, &[]).stdout).unwrap()
}

fn groups(report: &Value) -> &Vec<Value> {
    report["groups"].as_array().unwrap()
}

fn temp(test: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("duped-report-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn has_git() -> bool {
    Command::new("git").arg("--version").output().is_ok()
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
