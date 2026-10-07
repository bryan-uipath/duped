//! End-to-end: `duped names` over a small pnpm workspace holding a ported module.

use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

/// `host → base`, `ext → base`. `ext` ported `host`'s help panel: it renamed the panel, added an
/// `auth` parameter and kept `dropCachedClient` as is.
fn workspace(test: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("duped-names-e2e-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let files = [
        ("pnpm-workspace.yaml", "packages:\n  - 'packages/*'\n"),
        ("package.json", r#"{"name":"mono","private":true}"#),
        ("packages/base/package.json", r#"{"name":"@m/base"}"#),
        (
            "packages/host/package.json",
            r#"{"name":"@m/host","dependencies":{"@m/base":"workspace:*"}}"#,
        ),
        (
            "packages/ext/package.json",
            r#"{"name":"@m/ext","dependencies":{"@m/base":"workspace:*"}}"#,
        ),
        (
            "packages/host/src/HelpPanel.tsx",
            r#"import type { Client, Session } from '@m/base';
let client: Client | null = null;
export function dropCachedClient(): void { client = null; }
async function ensureClient(): Promise<Client> { return client!; }
export function HelpPanel({ readOnly = false }: { readOnly?: boolean }) { return null; }
function PanelBody({ session }: { session: Session }) { return null; }
"#,
        ),
        (
            "packages/ext/src/ExtHelpPanel.tsx",
            r#"import type { Client, Session } from '@m/base';
export interface ClientAuth { token: string; baseUrl: string }
let client: Client | null = null;
export function dropCachedClient(): void { client = null; }
async function ensureClient(auth: ClientAuth): Promise<Client> { return client!; }
export function ExtHelpPanel({ auth }: { auth: ClientAuth | null }) { return null; }
function PanelBody({ session, auth }: { session: Session; auth: ClientAuth | null }) { return null; }
"#,
        ),
        // One common name, plus anonymous default exports: not a pair.
        (
            "packages/host/src/view.tsx",
            "export function render(): void {}\nexport default function () {}\n",
        ),
        (
            "packages/ext/src/view.tsx",
            "export function render(): void {}\nexport default function () {}\n",
        ),
        // A copy inside one module: hidden by default.
        (
            "packages/base/src/a.ts",
            "export function parseThing(s: string): number { return 0; }\nexport function formatThing(n: number): string { return ''; }\n",
        ),
        (
            "packages/base/src/b.ts",
            "export function parseThing(s: string): number { return 0; }\nexport function formatThing(n: number): string { return ''; }\n",
        ),
    ];
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

fn json(args: &[&str]) -> Value {
    serde_json::from_str(&duped(args)).unwrap()
}

#[test]
fn finds_the_ported_module_and_tags_it() {
    let root = workspace("ported");
    let dir = root.to_str().unwrap();

    let report = json(&["names", dir, "--json"]);
    assert_eq!(report["summary"]["pairs"], 1, "{report}");
    assert_eq!(report["summary"]["hidden"]["same_module"], 1);
    let pair = &report["pairs"][0];
    assert_eq!(pair["a"]["file"], "packages/ext/src/ExtHelpPanel.tsx");
    assert_eq!(pair["b"]["file"], "packages/host/src/HelpPanel.tsx");
    assert_eq!(pair["tag"]["kind"], "move-down");
    assert_eq!(pair["tag"]["home"], "@m/base");
    let shared: Vec<(&str, bool, bool)> = pair["shared"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["name"].as_str().unwrap(),
                s["same"].as_bool().unwrap(),
                s["exported"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        shared,
        vec![
            ("dropCachedClient", true, true),
            ("ensureClient", false, false),
            ("PanelBody", false, false),
        ]
    );

    let text = duped(&["names", dir]);
    assert!(
        text.contains("1. score 4.25: 3 shared names (of 5 and 4) — move-down"),
        "{text}"
    );
    assert!(
        text.contains("[@m/host] packages/host/src/HelpPanel.tsx"),
        "{text}"
    );
    assert!(
        text.contains("= dropCachedClient (:4 :3, exported): () => void"),
        "{text}"
    );
    assert!(text.contains("Hidden pairs: 1 same-module"), "{text}");

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn flags_reach_the_analysis() {
    let root = workspace("flags");
    let dir = root.to_str().unwrap();
    let pairs = |extra: &[&str]| -> u64 {
        let mut args = vec!["names", dir, "--json"];
        args.extend_from_slice(extra);
        json(&args)["summary"]["pairs"].as_u64().unwrap()
    };
    assert_eq!(pairs(&[]), 1);
    assert_eq!(pairs(&["--include-same-module"]), 2);
    assert_eq!(pairs(&["--min-score", "5"]), 0);
    assert_eq!(pairs(&["--min-shared", "4"]), 0);
    // `render` alone is a pair once one shared name and a low score are enough.
    assert_eq!(pairs(&["--min-shared", "1", "--min-score", "0"]), 2);
    // Names in more than one file don't count at all.
    assert_eq!(pairs(&["--max-files", "1"]), 0);
    std::fs::remove_dir_all(&root).unwrap();
}
