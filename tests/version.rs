//! The install guard and plugin manifest name the crate version, so a stale binary reinstalls.

#[test]
fn docs_name_the_crate_version() {
    let version = env!("CARGO_PKG_VERSION");
    let guard = include_str!("../skills/duped/references/cli.md");
    assert!(guard.contains(&format!("\"duped {version}\"")));
    let plugin = include_str!("../.claude-plugin/plugin.json");
    assert!(plugin.contains(&format!("\"version\": \"{version}\"")));
}
