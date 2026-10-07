//! End-to-end: `duped fns` and destructured parameters over a small pnpm workspace.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

/// `app → core`, and `web`, which depends on nothing.
fn workspace(test: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("duped-fns-e2e-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let files = vec![
        ("pnpm-workspace.yaml", "packages:\n  - 'packages/*'\n".to_string()),
        ("package.json", r#"{"name":"mono","private":true}"#.into()),
        ("packages/core/package.json", r#"{"name":"@m/core"}"#.into()),
        (
            "packages/app/package.json",
            r#"{"name":"@m/app","dependencies":{"@m/core":"workspace:*"}}"#.into(),
        ),
        ("packages/web/package.json", r#"{"name":"@m/web"}"#.into()),
        // app ports core's component and adds a prop: importable.
        (
            "packages/core/src/panel.tsx",
            "export interface PanelProps { user: User; compact?: boolean; theme: Theme }\nexport function Panel({ user, compact = false, theme }: PanelProps) { return null; }".into(),
        ),
        (
            "packages/app/src/panel.tsx",
            "export function AppPanel({ user, locale, compact = false, theme }: { user: User; locale: Locale; compact?: boolean; theme: Theme }) { return null; }".into(),
        ),
        // Same signature, no dependency path: boundary.
        (
            "packages/core/src/edge.ts",
            "export function makeEdgeKey(source: string, target: string, handle?: string): string { return source; }".into(),
        ),
        (
            "packages/web/src/edge.ts",
            "export function edgeKey(source: string, target: string, handle?: string): string { return target; }".into(),
        ),
        // A hook around a fetcher: hidden as a wrapper.
        (
            "packages/core/src/report.ts",
            "export async function fetchReport(reportId: string, folderKey: string) { return null; }".into(),
        ),
        (
            "packages/app/src/report.ts",
            "import { fetchReport } from '@m/core';\nexport function useReport(reportId: string, folderKey: string) {\n  return fetchReport(reportId, folderKey);\n}".into(),
        ),
        // Two implementations of one interface: hidden.
        (
            "packages/app/src/disk.ts",
            "export class DiskStore implements Store { save(entryKey: string, payload: string) {} }".into(),
        ),
        (
            "packages/web/src/memory.ts",
            "export class MemoryStore implements Store { save(entryKey: string, payload: string) {} }".into(),
        ),
        // Two in one file and two in one module: hidden.
        (
            "packages/web/src/pair.ts",
            "export function first(alpha: number, beta: number) {}\nexport function second(alpha: number, beta: number) {}".into(),
        ),
        (
            "packages/web/src/one.ts",
            "export function one(gamma: number, delta: number) {}".into(),
        ),
        (
            "packages/web/src/two.ts",
            "export function two(gamma: number, delta: number) {}".into(),
        ),
    ];
    // Six comparators in two modules: `(a, b)` is a convention, not a copy.
    let comparators = (0..6).map(|i| {
        let package = if i < 3 { "core" } else { "web" };
        (
            format!("packages/{package}/src/sort{i}.ts"),
            format!("export function by{i}(a: Item, b: Item): number {{ return {i}; }}"),
        )
    });
    let files = files
        .into_iter()
        .map(|(path, source)| (path.to_string(), source))
        .chain(comparators);
    for (path, source) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    root
}

fn duped(args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_duped"))
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn fns_json(root: &Path, extra: &[&str]) -> Value {
    let mut args = vec!["fns", root.to_str().unwrap(), "--json"];
    args.extend(extra);
    serde_json::from_str(&duped(&args)).unwrap()
}

/// `(a, b, tag kind)` for every reported pair.
fn tags(json: &Value) -> Vec<(String, String, String)> {
    let mut out: Vec<_> = json["pairs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            let name = |side: &str| p[side]["name"].as_str().unwrap().to_string();
            (
                name("a"),
                name("b"),
                p["tag"]["kind"].as_str().unwrap_or("none").to_string(),
            )
        })
        .collect();
    out.sort();
    out
}

#[test]
fn finds_ported_functions_and_hides_by_design_pairs() {
    let root = workspace("finds");
    let json = fns_json(&root, &[]);
    assert_eq!(
        tags(&json),
        vec![
            ("AppPanel".into(), "Panel".into(), "importable".into()),
            ("makeEdgeKey".into(), "edgeKey".into(), "boundary".into()),
        ]
    );
    let panel = &json["pairs"][0];
    assert_eq!(
        panel["b"]["params"],
        serde_json::json!(["user", "compact", "theme"])
    );
    assert_eq!(panel["tag"]["copy"], "@m/app");
    let hidden = &json["summary"]["hidden"];
    assert_eq!(
        (
            &hidden["wrappers"],
            &hidden["implementations"],
            &hidden["same_module"]
        ),
        (&Value::from(1), &Value::from(1), &Value::from(1))
    );

    let text = duped(&["fns", root.to_str().unwrap()]);
    assert!(
        text.contains("fn Panel({ user, compact?, theme })"),
        "{text}"
    );
    assert!(
        text.contains("importable: @m/app can import @m/core"),
        "{text}"
    );

    // Each flag brings its hidden pairs back.
    let count = |extra: &[&str]| fns_json(&root, extra)["summary"]["pairs"].as_u64().unwrap();
    assert_eq!(count(&["--include-wrappers"]), 3);
    assert_eq!(count(&["--include-implementations"]), 3);
    assert_eq!(count(&["--include-same-module"]), 3);
    // 3 × 3 cross-module comparator pairs.
    assert_eq!(count(&["--max-sharing", "6"]), 11);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn extract_lists_destructured_fields() {
    let root = workspace("extract");
    let out = duped(&[
        "extract",
        root.join("packages/core/src/panel.tsx").to_str().unwrap(),
    ]);
    let panel: Value = out
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|r| r["name"] == "Panel")
        .unwrap();
    let param = &panel["params"][0];
    assert_eq!(param["name"], "{ user, compact = false, theme }");
    assert_eq!(param["type"], "PanelProps");
    assert_eq!(
        param["fields"],
        serde_json::json!([
            { "name": "user", "type": "User" },
            { "name": "compact", "type": "boolean", "optional": true },
            { "name": "theme", "type": "Theme" },
        ])
    );
    std::fs::remove_dir_all(&root).unwrap();
}
