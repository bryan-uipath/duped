//! End-to-end: run the `duped types` binary over a small TypeScript tree.

use std::path::PathBuf;
use std::process::Command;

/// A fresh tree per test; tests run in parallel, so each needs its own directory.
fn workspace(test: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("duped-types-e2e-{test}-{}", std::process::id()));
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
    let root = workspace("finds");
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

#[test]
fn flags_reach_the_analysis() {
    let root = workspace("flags");
    let dir = root.to_str().unwrap();
    let pair_count = |extra: &[&str]| -> u64 {
        let mut args = vec!["types", dir, "--json"];
        args.extend_from_slice(extra);
        let json: serde_json::Value = serde_json::from_str(&duped(&args)).unwrap();
        json["summary"]["pairs"].as_u64().unwrap()
    };
    assert_eq!(pair_count(&[]), 2);
    assert_eq!(pair_count(&["--exclude-name", "Host*"]), 1);
    // The unions have 5 and 6 members, the auth shapes 6 fields each.
    assert_eq!(pair_count(&["--min-fields", "6"]), 1);
    assert_eq!(pair_count(&["--min-shared", "6"]), 1);
    assert_eq!(pair_count(&["--threshold", "0.9"]), 1);

    let truncated = duped(&["types", dir, "--top", "1"]);
    assert!(truncated.contains("… 1 more; raise --top"), "{truncated}");

    let out = root.join("report.txt");
    duped(&["types", dir, "--out", out.to_str().unwrap()]);
    assert!(
        std::fs::read_to_string(&out)
            .unwrap()
            .contains("AuthContext = HostAuth")
    );

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn invalid_name_glob_fails_before_touching_out() {
    let root = workspace("invalid");
    let out = root.join("report.txt");
    std::fs::write(&out, "previous report").unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_duped"))
        .args([
            "types",
            root.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
            "--exclude-name",
            "[",
        ])
        .output()
        .unwrap();
    assert!(!status.status.success());
    assert!(String::from_utf8_lossy(&status.stderr).contains("invalid --exclude-name glob"));
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "previous report");
    std::fs::remove_dir_all(&root).unwrap();
}
