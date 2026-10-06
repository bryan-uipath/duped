//! End-to-end: `duped types` over a small pnpm workspace, with module tags and `duped.toml`.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

/// `app → core → base`, `web → base`, and `evals`, which depends on nothing.
fn workspace(test: &str, config: Option<&str>) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("duped-modules-e2e-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let mut files = vec![
        ("pnpm-workspace.yaml", "packages:\n  - 'packages/*'\n"),
        ("package.json", r#"{"name":"mono","private":true}"#),
        ("packages/base/package.json", r#"{"name":"@m/base"}"#),
        (
            "packages/core/package.json",
            r#"{"name":"@m/core","dependencies":{"@m/base":"workspace:*"}}"#,
        ),
        (
            "packages/app/package.json",
            r#"{"name":"@m/app","devDependencies":{"@m/core":"workspace:*"}}"#,
        ),
        (
            "packages/web/package.json",
            r#"{"name":"@m/web","dependencies":{"@m/base":"workspace:*"}}"#,
        ),
        ("packages/evals/package.json", r#"{"name":"@m/evals"}"#),
        // app copies core's type: importable.
        (
            "packages/core/src/auth.ts",
            "export interface AuthContext { accessToken: string; tenantId: string; organizationId: string; baseUrl?: string }",
        ),
        (
            "packages/app/src/auth.ts",
            "export interface HostAuth { accessToken: string; tenantId: string; organizationId: string; baseUrl?: string }",
        ),
        // app and web both copy an edge shape; neither depends on the other, both on base.
        (
            "packages/app/src/edge.ts",
            "export interface AppEdge { id: string; source: string; target: string; sourceHandle?: string }",
        ),
        (
            "packages/web/src/edge.ts",
            "export interface WebEdge { id: string; source: string; target: string; sourceHandle?: string }",
        ),
        // web and evals: deliberate twins, said so in a file header.
        (
            "packages/web/src/anchor.ts",
            "/**\n * Structural twin of the annotation types in `@m/evals`.\n */\nexport interface Anchor { page: number; left: number; top: number; width: number }",
        ),
        (
            "packages/evals/src/anchor.ts",
            "export interface Anchor { page: number; left: number; top: number; width: number }",
        ),
        // Two copies inside one module.
        (
            "packages/base/src/a.ts",
            "export interface One { p: 1; q: 2; r: 3; s: 4 }",
        ),
        (
            "packages/base/src/b.ts",
            "export interface Two { p: 1; q: 2; r: 3; s: 4 }",
        ),
    ];
    if let Some(config) = config {
        files.push(("duped.toml", config));
    }
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

fn types_json(root: &Path, extra: &[&str]) -> Value {
    let scan = root.join("packages");
    let mut args = vec![
        "types",
        scan.to_str().unwrap(),
        "--root",
        root.to_str().unwrap(),
        "--json",
    ];
    args.extend(extra);
    serde_json::from_str(&duped(&args)).unwrap()
}

/// `(a, b, tag kind)` for every reported pair, e.g. `("AuthContext", "HostAuth", "importable")`.
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
fn tags_pairs_by_module_relationship_and_hides_the_rest() {
    let root = workspace("tags", None);
    let json = types_json(&root, &[]);
    assert_eq!(
        tags(&json),
        vec![
            ("AppEdge".into(), "WebEdge".into(), "move-down".into()),
            ("HostAuth".into(), "AuthContext".into(), "importable".into()),
        ]
    );
    let importable = json["pairs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["tag"]["kind"] == "importable")
        .unwrap();
    assert_eq!(
        (
            importable["tag"]["copy"].as_str(),
            importable["tag"]["owner"].as_str()
        ),
        (Some("@m/app"), Some("@m/core"))
    );
    let edge = json["pairs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["tag"]["kind"] == "move-down")
        .unwrap();
    assert_eq!(edge["tag"]["home"], "@m/base");
    assert_eq!(json["summary"]["hidden"]["same_module"], 1);
    assert_eq!(json["summary"]["hidden"]["acknowledged"], 1);
    // Importable groups come first.
    assert_eq!(json["clusters"][0]["tag"]["kind"], "importable");
    assert_eq!(json["pairs"][0]["a"]["module"], "@m/app");
}

#[test]
fn include_flags_show_hidden_pairs() {
    let root = workspace("include", None);
    let json = types_json(&root, &["--include-same-module", "--include-acknowledged"]);
    let all = tags(&json);
    assert!(
        all.contains(&("One".into(), "Two".into(), "same-module".into())),
        "{all:?}"
    );
    let twin = json["pairs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["a"]["name"] == "Anchor")
        .unwrap();
    assert_eq!(
        (twin["tag"]["kind"].as_str(), twin["acknowledged"].as_str()),
        (Some("boundary"), Some("doc: structural twin"))
    );
}

#[test]
fn config_acknowledges_overrides_and_sets_thresholds() {
    let config = r#"
[types]
min_fields = 4

[modules."@m/web"]
deps = ["@m/app"]

[[acknowledged]]
a = "@m/core:AuthContext"
b = "HostAuth"
reason = "app is a demo"
"#;
    let root = workspace("config", Some(config));
    let json = types_json(&root, &[]);
    // web → app now, so the edge pair is importable; the auth pair is acknowledged.
    assert_eq!(
        tags(&json),
        vec![("AppEdge".into(), "WebEdge".into(), "importable".into())]
    );
    assert_eq!(json["summary"]["hidden"]["acknowledged"], 2);

    let text = duped(&[
        "types",
        root.join("packages").to_str().unwrap(),
        "--root",
        root.to_str().unwrap(),
    ]);
    assert!(text.contains("Modules: 6 modules."), "{text}");
    assert!(
        text.contains("importable: @m/web can import @m/app"),
        "{text}"
    );
    assert!(text.contains("[@m/app] app/src/edge.ts:1"), "{text}");
    assert!(text.contains("Hidden pairs: 1 same-module (--include-same-module), 2 acknowledged (--include-acknowledged)."), "{text}");

    // A flag beats the config.
    let strict = types_json(&root, &["--min-fields", "5"]);
    assert!(strict["pairs"].as_array().unwrap().is_empty());
}

#[test]
fn bad_config_fails_before_writing_output() {
    let root = workspace("bad", Some("[modules.nope]\ndeps = []\n"));
    let out = root.join("out.txt");
    std::fs::write(&out, "keep").unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_duped"))
        .args([
            "types",
            root.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!status.status.success());
    assert!(String::from_utf8_lossy(&status.stderr).contains("[modules.nope]"));
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "keep");
}
