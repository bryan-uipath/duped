//! End-to-end: run the `duped types` binary over a small TypeScript tree.

use std::path::PathBuf;
use std::process::Command;

fn workspace() -> PathBuf {
    let root = std::env::temp_dir().join(format!("duped-types-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let files = [
        (
            "core/src/auth.ts",
            "export interface AuthContext { accessToken: string; tenantId: string; organizationId: string; tenantName?: string; organizationName?: string; baseUrl?: string }",
        ),
        (
            "host/src/auth.ts",
            "export interface HostAuth { accessToken: string; organizationId: string; organizationName?: string; tenantId: string; tenantName?: string; baseUrl?: string }",
        ),
        (
            "host/src/status.ts",
            "export type InstanceStatus = 'Pending' | 'Running' | 'Paused' | 'Completed' | 'Faulted';",
        ),
        (
            "core/src/status.ts",
            "export type Status = 'Pending' | 'Running' | 'Paused' | 'Pausing' | 'Completed' | 'Faulted';\nexport interface Unrelated { alpha: 1; beta: 2; gamma: 3; delta: 4 }",
        ),
        (
            "host/src/button.tsx",
            "export interface ButtonProps { accessToken: string; tenantId: string; organizationId: string; baseUrl?: string }",
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

#[test]
fn finds_renamed_copies_and_drifted_unions() {
    let root = workspace();
    let dir = root.to_str().unwrap();

    let json: serde_json::Value = serde_json::from_str(&duped(&["types", dir, "--json"])).unwrap();
    let pairs = json["pairs"].as_array().unwrap();
    let summary: Vec<(String, String, String)> = pairs
        .iter()
        .map(|p| {
            (
                p["a"]["name"].as_str().unwrap().to_string(),
                p["b"]["name"].as_str().unwrap().to_string(),
                p["relationship"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    // `ButtonProps` is excluded by default; `Unrelated` shares nothing.
    assert_eq!(
        summary,
        vec![
            ("AuthContext".into(), "HostAuth".into(), "exact".into()),
            ("Status".into(), "InstanceStatus".into(), "superset".into()),
        ]
    );
    assert_eq!(json["summary"]["clusters"], 2);

    let text = duped(&["types", dir]);
    assert!(text.contains("AuthContext = HostAuth"), "{text}");
    assert!(text.contains("Status ⊃ InstanceStatus"), "{text}");

    let with_props = duped(&[
        "types",
        dir,
        "--no-default-excludes",
        "--pairs",
        "--threshold",
        "0.6",
    ]);
    assert!(with_props.contains("ButtonProps"), "{with_props}");

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn rejects_out_of_range_thresholds() {
    let output = Command::new(env!("CARGO_BIN_EXE_duped"))
        .args(["types", ".", "--threshold", "1.5"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("between 0 and 1"));
}
