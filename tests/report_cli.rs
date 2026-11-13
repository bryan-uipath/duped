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
fn diff_mode_keeps_only_findings_on_changed_lines() {
    if !has_git() {
        return;
    }
    let root = temp("lines");
    let both = format!(
        "{}{}",
        LIST.replace("NAME", "uniqueItems"),
        RETRY.replace("NAME", "fetchJson")
    );
    write(&root, "a.ts", &both);
    write(&root, "list.ts", &LIST.replace("NAME", "dedupeRows"));
    write(&root, "retry.ts", &RETRY.replace("NAME", "loadJson"));
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "base"]);
    // Only `fetchJson` changes; `uniqueItems`, in the same file, was copied before.
    write(&root, "a.ts", &both.replace("retries exhausted", "gave up"));

    let report = json(&root, &["--base", "main"]);
    assert_eq!(report["changedFiles"], 1);
    let pairs: Vec<(&str, &str)> = groups(&report)
        .iter()
        .map(|g| (g["a"].as_str().unwrap(), g["b"].as_str().unwrap()))
        .collect();
    assert_eq!(pairs, vec![("a.ts", "retry.ts")]);
    assert_eq!(report["detectors"]["bodies"]["total"], 2);
    assert_eq!(report["detectors"]["bodies"]["kept"], 1);

    // A deletion alone counts: it can be what makes two bodies match.
    write(&root, "a.ts", &both.replace("seen.add(item.id);\n", ""));
    let report = json(&root, &["--base", "main"]);
    assert_eq!(groups(&report).len(), 1);
    assert_eq!(report["groups"][0]["b"], "list.ts");
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
fn same_file_type_pairs_count_one_side() {
    // Two 6-line interfaces with the same fields in one file: one copy is removable, not both.
    let root = temp("same-file");
    let shape = |name: &str| {
        format!(
            "export interface {name} {{\n  id: string;\n  label: string;\n  owner: string;\n  status: string;\n}}\n"
        )
    };
    write(
        &root,
        "a/types.ts",
        &format!("{}{}", shape("Alpha"), shape("Beta")),
    );
    write(&root, "b/other.ts", "export const unrelated = 1;\n");
    let report = json(&root, &[]);
    let same: Vec<&Value> = groups(&report)
        .iter()
        .filter(|g| g["a"] == g["b"])
        .collect();
    assert_eq!(same.len(), 1, "{report}");
    assert_eq!(same[0]["removableLines"], 6);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn identical_copies_count_once_each() {
    // Three identical files: two pairs with the first, not three, and not y ~ z from `bodies`.
    let root = temp("copies");
    for dir in ["x", "y", "z"] {
        write(
            &root,
            &format!("{dir}/list.ts"),
            &LIST.replace("NAME", "uniqueItems"),
        );
    }
    let report = json(&root, &[]);
    let pairs: Vec<(&str, &str)> = groups(&report)
        .iter()
        .map(|g| (g["a"].as_str().unwrap(), g["b"].as_str().unwrap()))
        .collect();
    assert_eq!(
        pairs,
        vec![("x/list.ts", "y/list.ts"), ("x/list.ts", "z/list.ts")]
    );
    assert_eq!(report["removableLines"], 22);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn groups_rank_by_removable_lines_then_agreeing_analyses() {
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
    // `s/`: an identical 12-line file, with CRLF line ends in one copy (files only).
    let table: String = (0..12)
        .map(|i| format!("export const LIMIT_{i} = {i};\n"))
        .collect();
    write(&root, "s/one.ts", &table);
    write(&root, "s/two.ts", &table.replace('\n', "\r\n"));

    let report = json(&root, &[]);
    let removable: Vec<u64> = groups(&report)
        .iter()
        .map(|g| g["removableLines"].as_u64().unwrap())
        .collect();
    assert_eq!(removable, vec![22, 12, 11, 1]);
    assert_eq!(report["removableLines"], 46);
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
            ("p/one.ts", vec!["bodies"], 3),
            ("s/one.ts", vec!["files"], 1),
            ("r/one.ts", vec!["bodies"], 1),
            ("q/one.ts", vec!["names", "types"], 2),
        ]
    );
    // Within a group, by each analysis's rank: the file pair sums its functions' shingles.
    let kinds: Vec<&str> = report["groups"][0]["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, vec!["file", "function", "function"]);
    let order = &report["groups"][3]["findings"];
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
        text.starts_with(
            "4 file pairs and 0 helper groups, ~46 removable lines (bodies 4; files 1; helpers 0; imports 0; names 1; types 1).\n"
        ),
        "{text}"
    );
    assert!(text.contains(
        "\n4. ~1 removable lines, names + types: 2 findings\n   q/one.ts\n   q/two.ts\n"
    ));
    assert!(
        text.contains("   - files file: identical, 12 lines"),
        "{text}"
    );
    assert!(
        text.contains("   - bodies file: 2 function pairs"),
        "{text}"
    );
    assert!(text.contains("(uniqueItems:1 ~ dedupeRows:1)"), "{text}");
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn helper_groups_rank_apart_and_skip_copies_in_identical_files() {
    let root = temp("helpers");
    write(
        &root,
        "pnpm-workspace.yaml",
        "packages:\n  - 'packages/*'\n",
    );
    write(
        &root,
        "packages/core/package.json",
        r#"{"name":"@demo/core"}"#,
    );
    write(
        &root,
        "packages/web/package.json",
        r#"{"name":"@demo/web","dependencies":{"@demo/core":"workspace:*"}}"#,
    );
    let guard = "export function NAME(value: unknown): value is Record<string, unknown> {\n  return typeof value === 'object' && value !== null && !Array.isArray(value);\n}\n";
    write(
        &root,
        "packages/core/a.ts",
        &guard.replace("NAME", "isRecord"),
    );
    // `web/p.ts` is an identical copy of `core/p.ts`: its helper is counted with the file,
    // so the group is left in one module, with no cross-module home.
    let padded = format!(
        "{}{}",
        guard.replace("NAME", "isPlain"),
        (0..10)
            .map(|i| format!("export const N{i} = {i};\n"))
            .collect::<String>()
    );
    write(&root, "packages/core/p.ts", &padded);
    write(&root, "packages/web/p.ts", &padded);

    let report = json(&root, &[]);
    assert_eq!(report["helperGroups"], 1);
    assert_eq!(report["detectors"]["helpers"]["total"], 1);
    let group = &report["helpers"][0];
    let files: Vec<&str> = group["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["file"].as_str().unwrap())
        .collect();
    assert_eq!(files, vec!["packages/core/a.ts", "packages/core/p.ts"]);
    assert_eq!(group["removableLines"], 3);
    assert_eq!(group["modules"], serde_json::json!(["@demo/core"]));
    assert_eq!(group["home"], Value::Null);
    // The identical file's 13 lines, plus 1 of 2 helper copies.
    assert_eq!(report["removableLines"], 16);

    let text = text(&root);
    assert!(
        text.contains("1 file pairs and 1 helper groups, ~16 removable lines"),
        "{text}"
    );
    assert!(
        text.contains("\nIdentical helpers:\n\n1. 2 copies in @demo/core, ~3 removable lines: isPlain, isRecord\n   [@demo/core] isRecord  packages/core/a.ts:1\n"),
        "{text}"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn lists_helper_families_whose_copies_drifted() {
    let root = temp("families");
    let guard = "export const isRecord = (v: unknown) => typeof v === 'object' && v !== null && !Array.isArray(v);\n";
    write(&root, "x.ts", guard);
    write(&root, "y.ts", guard);
    write(&root, "z.ts", &guard.replace(" && !Array.isArray(v)", ""));

    let report = json(&root, &[]);
    assert_eq!(report["helperFamilies"], 1);
    let family = &report["families"][0];
    assert_eq!(family["name"], "isRecord");
    assert_eq!(family["definitions"], 3);
    let variants: Vec<usize> = family["variants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_array().unwrap().len())
        .collect();
    assert_eq!(variants, vec![2, 1]);
    assert_eq!(family["variants"][1][0]["file"], "z.ts");

    let text = text(&root);
    assert!(
        text.contains("\nSame name, different bodies:\n\n1. isRecord: 3 definitions, 2 bodies\n   2× isRecord  x.ts:1\n   1× isRecord  z.ts:1\n"),
        "{text}"
    );

    // With `--base`, the family is kept for its changed copy, which stands for its variant.
    if has_git() {
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "base"]);
        assert_eq!(json(&root, &["--base", "main"])["helperFamilies"], 0);
        write(&root, "y.ts", &guard.replace("isRecord =", "isRecord  ="));
        let report = json(&root, &["--base", "main"]);
        assert_eq!(report["helperFamilies"], 1);
        let copies = &report["families"][0]["variants"][0];
        assert_eq!(copies[0].get("changed"), None);
        assert_eq!(copies[1]["changed"], true);
        let text = String::from_utf8(duped(&root, &["--base", "main"]).stdout).unwrap();
        assert!(
            text.contains("   2× isRecord  y.ts:1 (changed)\n"),
            "{text}"
        );
    }
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn rolls_file_pairs_up_by_directory() {
    let root = temp("directories");
    write(&root, "a/list.ts", &LIST.replace("NAME", "uniqueItems"));
    write(&root, "b/rows.ts", &LIST.replace("NAME", "dedupeRows"));
    write(&root, "a/http.ts", &RETRY.replace("NAME", "fetchJson"));
    write(&root, "b/load.ts", &RETRY.replace("NAME", "loadJson"));
    // One pair between `a` and the root: not a roll-up.
    write(&root, "totals.ts", &TOTALS.replace("NAME", "sumLines"));
    write(
        &root,
        "a/totals.ts",
        &TOTALS.replace("NAME", "invoiceTotals"),
    );

    let report = json(&root, &[]);
    assert_eq!(report["filePairs"], 3);
    let lines: u64 = groups(&report)
        .iter()
        .filter(|g| g["b"].as_str().unwrap().starts_with("b/"))
        .map(|g| g["removableLines"].as_u64().unwrap())
        .sum();
    assert_eq!(
        report["directories"],
        serde_json::json!([{ "a": "a", "b": "b", "filePairs": 2, "removableLines": lines }])
    );
    let text = text(&root);
    assert!(
        text.contains(&format!(
            "\nDirectories sharing several file pairs:\n   ~{lines} removable lines, 2 file pairs: a ~ b\n\n1. "
        )),
        "{text}"
    );
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
        // A user's context setting must not widen the changed lines.
        .env("GIT_DIFF_OPTS", "-u99")
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
