//! End-to-end: `duped imports` over a small pnpm workspace with a ported component.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

/// `app → base`, `web → base`. `web` ports `app`'s chat panel under a new name.
fn workspace(test: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("duped-imports-e2e-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let chat = "import { useState } from 'react';\n\
        import { ChatWidget } from '@vendor/chat-widget';\n\
        import '@vendor/chat-widget/style.css';\n\
        import { useDebugStore, type DebugSession } from '@m/base/debug';\n\
        import { Spinner, AlertIcon } from '@vendor/icons';\n";
    let files = vec![
        ("pnpm-workspace.yaml", "packages:\n  - 'packages/*'\n".to_string()),
        ("package.json", r#"{"name":"mono","private":true}"#.into()),
        ("packages/base/package.json", r#"{"name":"@m/base"}"#.into()),
        (
            "packages/app/package.json",
            r#"{"name":"@m/app","dependencies":{"@m/base":"workspace:*"}}"#.into(),
        ),
        (
            "packages/web/package.json",
            r#"{"name":"@m/web","dependencies":{"@m/base":"workspace:*"}}"#.into(),
        ),
        (
            "packages/app/src/ChatPanel.tsx",
            format!("{chat}import {{ APP_NS }} from './i18n';\nexport function ChatPanel() {{}}\n"),
        ),
        (
            "packages/web/src/WebChatPanel.tsx",
            format!("{chat}import {{ WEB_NS }} from '../locale';\nexport function Panel() {{}}\n"),
        ),
        // A test copy is skipped unless `--include-tests`.
        ("packages/web/src/WebChatPanel.test.tsx", chat.to_string()),
        // Siblings in one module: hidden unless `--include-same-module`.
        (
            "packages/app/src/Grid.tsx",
            "import { Table } from '@vendor/table';\nimport { Pager } from '@vendor/pager';\nimport { Filter } from '@vendor/filter';\n".into(),
        ),
        (
            "packages/app/src/List.tsx",
            "import { Table } from '@vendor/table';\nimport { Pager } from '@vendor/pager';\nimport { Filter } from '@vendor/filter';\n".into(),
        ),
        // Parts of one compound component are one specifier: not enough on their own.
        (
            "packages/app/src/Confirm.tsx",
            "import { Dialog, DialogTitle, DialogBody, DialogFooter } from '@vendor/ui';\n".into(),
        ),
        (
            "packages/web/src/Remove.tsx",
            "import { Dialog, DialogTitle, DialogBody, DialogFooter } from '@vendor/ui';\n".into(),
        ),
    ];
    // Filler: `react` is in most files, so it is common and weighs almost nothing.
    let filler = (0..24).map(|i| {
        let package = if i % 2 == 0 { "app" } else { "web" };
        (
            format!("packages/{package}/src/filler{i}.tsx"),
            format!(
                "import {{ useState }} from 'react';\nimport {{ own{i} }} from '@vendor/own{i}';\n"
            ),
        )
    });
    for (path, source) in files
        .into_iter()
        .map(|(p, s)| (p.to_string(), s))
        .chain(filler)
    {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    root
}

fn imports(root: &Path, extra: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_duped"))
        .arg("imports")
        .arg(root)
        .args(extra)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn pair_files(report: &Value) -> Vec<(String, String)> {
    report["pairs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            let name = |side: &str| p[side]["file"].as_str().unwrap().to_string();
            (name("a"), name("b"))
        })
        .collect()
}

#[test]
fn finds_the_ported_panel_and_tags_it() {
    let root = workspace("finds");
    let report = imports(&root, &["--json"]);
    assert_eq!(
        pair_files(&report),
        vec![(
            "packages/app/src/ChatPanel.tsx".to_string(),
            "packages/web/src/WebChatPanel.tsx".to_string()
        )]
    );
    let pair = &report["pairs"][0];
    assert_eq!(pair["tag"]["kind"], "move-down");
    assert_eq!(pair["tag"]["home"], "@m/base");
    assert_eq!(pair["a"]["module"], "@m/app");
    // Chat widget, its stylesheet, the debug store and the icons; `react` is not rare.
    assert_eq!(pair["sources"], 4);
    assert_eq!(report["summary"]["hidden"]["same_module"], 1);

    let text = Command::new(env!("CARGO_BIN_EXE_duped"))
        .arg("imports")
        .arg(&root)
        .output()
        .unwrap();
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(text.contains("move-down: both depend on @m/base"), "{text}");
    assert!(
        text.contains("@vendor/chat-widget#ChatWidget (2)"),
        "{text}"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn flags_reveal_same_module_and_test_pairs() {
    let root = workspace("flags");
    let same = imports(&root, &["--json", "--include-same-module"]);
    assert!(pair_files(&same).contains(&(
        "packages/app/src/Grid.tsx".to_string(),
        "packages/app/src/List.tsx".to_string()
    )));
    let tests = imports(&root, &["--json", "--include-tests"]);
    assert!(pair_files(&tests).contains(&(
        "packages/app/src/ChatPanel.tsx".to_string(),
        "packages/web/src/WebChatPanel.test.tsx".to_string()
    )));
    let strict = imports(&root, &["--json", "--min-shared", "5"]);
    assert_eq!(strict["summary"]["pairs"], 0);
    std::fs::remove_dir_all(&root).unwrap();
}
