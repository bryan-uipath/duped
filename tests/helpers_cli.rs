//! End-to-end: `duped helpers` over a synthetic workspace where `isRecord` is copied into
//! three packages, one copy under a new name, and a fourth copy has drifted.

use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

const IS_RECORD: &str = "export function isRecord(value: unknown): value is Record<string, unknown> {\n  return typeof value === 'object' && value !== null && !Array.isArray(value);\n}\n";

/// `web → core`, `ext → core`.
fn workspace() -> PathBuf {
    let root = std::env::temp_dir().join(format!("duped-helpers-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let arrow = "export const isRec = (v: unknown) => typeof v === 'object' && v !== null && !Array.isArray(v);\n";
    let drifted = "export const isRecord = (value: unknown) => typeof value === 'object' && value !== null;\n";
    let files = [
        ("pnpm-workspace.yaml", "packages:\n  - 'packages/*'\n"),
        ("package.json", r#"{"name":"demo","private":true}"#),
        ("packages/core/package.json", r#"{"name":"@demo/core"}"#),
        (
            "packages/web/package.json",
            r#"{"name":"@demo/web","dependencies":{"@demo/core":"workspace:*"}}"#,
        ),
        (
            "packages/ext/package.json",
            r#"{"name":"@demo/ext","dependencies":{"@demo/core":"workspace:*"}}"#,
        ),
        ("packages/core/src/guards.ts", IS_RECORD),
        ("packages/web/src/form.ts", IS_RECORD),
        ("packages/ext/src/bridge.ts", arrow),
        ("packages/ext/src/legacy.ts", drifted),
        // Tests are skipped, as everywhere.
        ("packages/web/src/form.test.ts", IS_RECORD),
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

#[test]
fn groups_copies_suggests_a_home_and_lists_drift() {
    let root = workspace();
    let dir = root.to_str().unwrap();
    let json: Value = serde_json::from_str(&duped(&["helpers", dir, "--json"])).unwrap();
    let [group] = &json["groups"].as_array().unwrap()[..] else {
        panic!("{json}")
    };
    let members: Vec<(&str, &str)> = group["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["name"].as_str().unwrap(), m["module"].as_str().unwrap()))
        .collect();
    assert_eq!(
        members,
        vec![
            ("isRecord", "@demo/core"),
            ("isRec", "@demo/ext"),
            ("isRecord", "@demo/web"),
        ]
    );
    assert_eq!(group["removableLines"], 4);
    assert_eq!(group["home"]["module"], "@demo/core");
    assert_eq!(json["families"][0]["name"], "isRecord");
    assert_eq!(json["families"][0]["definitions"], 3);

    let text = duped(&["helpers", dir]);
    assert!(
        text.contains("1. 3 copies in 3 modules, ~4 removable lines: isRec, isRecord"),
        "{text}"
    );
    assert!(
        text.contains("home: @demo/core (every member can import it)"),
        "{text}"
    );
    assert!(
        text.contains("1. isRecord: 3 definitions, 2 bodies"),
        "{text}"
    );
    std::fs::remove_dir_all(&root).unwrap();
}
