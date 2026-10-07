//! Regression suite for `duped types`: the findings that justify the tool must keep being
//! reported, with the right tags, and nothing else may appear.
//!
//! The fixture at `tests/fixtures/workspace` is a synthetic pnpm workspace; see its README.
//! An optional second test checks a real repository against an expectations file kept
//! outside this repo; it runs only when `DUPED_REGRESSION_ROOT` and `DUPED_REGRESSION_EXPECT`
//! are set.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use serde::Deserialize;
use serde_json::{Value, json};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/workspace");

/// Every pair `duped types` reports on the fixture by default, keyed `module:Name`.
/// `relationship` is from `a`'s side, e.g. `subset` when `a` has fewer fields.
const EXPECTED: &[Expect] = &[
    // Exact copies the copying module can import today.
    Expect::importable("@acme/api:HostAuth", "@acme/core:AuthContext", "exact"),
    Expect::importable(
        "@acme/core:Binding",
        "@acme/schema:PackagingBinding",
        "exact",
    ),
    // Drift: copies that have fallen behind the original.
    Expect::importable("@acme/api:SessionInfo", "@acme/core:Session", "subset"),
    Expect::importable("@acme/api:JobStatus", "@acme/core:RunStatus", "subset"),
    // The edge family: one shape in four modules, owned by the base.
    Expect::importable(
        "@acme/core:LayoutEdge",
        "@acme/schema:SerializedEdge",
        "exact",
    ),
    Expect::importable("@acme/api:ApiEdge", "@acme/schema:SerializedEdge", "exact"),
    Expect::importable("@acme/web:WebEdge", "@acme/schema:SerializedEdge", "exact"),
    Expect::importable("@acme/api:ApiEdge", "@acme/core:LayoutEdge", "exact"),
    Expect::move_down("@acme/core:LayoutEdge", "@acme/web:WebEdge", "@acme/schema"),
    Expect::move_down("@acme/api:ApiEdge", "@acme/web:WebEdge", "@acme/schema"),
    // Siblings sharing a shape neither owns; both depend on the base.
    Expect::move_down("@acme/core:Paging", "@acme/web:PageInfo", "@acme/schema"),
    // ui reaches core only through api.
    Expect::indirect("@acme/ui:Incident", "@acme/core:Incident"),
    // No dependency path, and no comment saying it's deliberate.
    Expect::boundary("@acme/web:RunSummary", "@acme/evals:RunSummary"),
];

/// Pairs hidden by default, shown with `--include-acknowledged` or `--include-same-module`.
const HIDDEN: &[Hidden] = &[
    Hidden {
        a: "@acme/web:Anchor",
        b: "@acme/evals:Anchor",
        flag: "--include-acknowledged",
        tag: "boundary",
        acknowledged: Some("doc: structural twin"),
    },
    Hidden {
        a: "@acme/core:Metrics",
        b: "@acme/web:Metrics",
        flag: "--include-acknowledged",
        tag: "move-down",
        acknowledged: Some("web reports its own metrics on purpose"),
    },
    Hidden {
        a: "@acme/api:ApiError",
        b: "@acme/api:RequestError",
        flag: "--include-same-module",
        tag: "same-module",
        acknowledged: None,
    },
];

/// Planted noise: each pair exists in the fixture, but only appears with the given flags.
const NOISE: &[(&[&str], &str, &str)] = &[
    (
        &["--include-tests"],
        "@acme/web:TestAuth",
        "@acme/core:AuthContext",
    ),
    (
        &["--include-tests"],
        "@acme/api:MockAuth",
        "@acme/core:AuthContext",
    ),
    (
        &["--no-default-excludes"],
        "@acme/ui:ButtonProps",
        "@acme/web:ButtonProps",
    ),
    (
        &["--min-fields", "3", "--min-shared", "3"],
        "@acme/core:Point",
        "@acme/web:Point",
    ),
];

#[test]
fn reports_every_expected_finding_and_nothing_else() {
    let report = types(FIXTURE, &[]);
    let pairs = pairs_by_key(&report);

    let mut missing = Vec::new();
    for expect in EXPECTED {
        let Some(pair) = single(&pairs, expect.a, expect.b) else {
            missing.push(format!("{} <> {}", expect.a, expect.b));
            continue;
        };
        let context = format!("{} <> {}", expect.a, expect.b);
        assert_eq!(pair["tag"], expect.tag(), "{context}: tag");
        assert_eq!(
            relationship_from(pair, expect.a),
            expect.relationship,
            "{context}: relationship"
        );
        assert_eq!(pair["acknowledged"], Value::Null, "{context}: acknowledged");
    }
    assert!(
        missing.is_empty(),
        "expected findings not reported: {missing:#?}"
    );

    let unexpected: Vec<_> = pairs
        .keys()
        .filter(|k| !EXPECTED.iter().any(|e| key(e.a, e.b) == **k))
        .collect();
    assert!(
        unexpected.is_empty(),
        "unexpected pairs (new false positives?): {unexpected:#?}"
    );

    let summary = &report["summary"];
    assert_eq!(summary["pairs"], EXPECTED.len());
    assert_eq!(summary["hidden"]["acknowledged"], 2);
    assert_eq!(summary["hidden"]["same_module"], 1);
}

#[test]
fn the_edge_family_is_one_cluster_owned_by_the_base() {
    let report = types(FIXTURE, &[]);
    let edges = [
        "@acme/api:ApiEdge",
        "@acme/core:LayoutEdge",
        "@acme/schema:SerializedEdge",
        "@acme/web:WebEdge",
    ];
    let cluster = report["clusters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["members"].as_array().unwrap().len() == edges.len())
        .expect("a four-member cluster");
    let mut members: Vec<String> = cluster["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(member_key)
        .collect();
    members.sort();
    assert_eq!(members, edges);
    assert_eq!(
        cluster["home"],
        json!({ "kind": "owner", "module": "@acme/schema" })
    );
}

#[test]
fn hidden_pairs_appear_with_their_flag() {
    for hidden in HIDDEN {
        let pairs = pairs_by_key(&types(FIXTURE, &[hidden.flag]));
        let pair = single(&pairs, hidden.a, hidden.b)
            .unwrap_or_else(|| panic!("{} <> {} missing with {}", hidden.a, hidden.b, hidden.flag));
        assert_eq!(
            pair["tag"]["kind"], hidden.tag,
            "{} <> {}",
            hidden.a, hidden.b
        );
        assert_eq!(
            pair["acknowledged"].as_str(),
            hidden.acknowledged,
            "{} <> {}",
            hidden.a,
            hidden.b
        );
    }
}

#[test]
fn planted_noise_exists_but_is_filtered_by_default() {
    let default = pairs_by_key(&types(FIXTURE, &[]));
    for (flags, a, b) in NOISE {
        assert!(
            !default.contains_key(&key(a, b)),
            "{a} <> {b} reported by default"
        );
        // Proves the fixture really contains the noise, so the default check means something.
        let with_flags = pairs_by_key(&types(FIXTURE, flags));
        assert!(
            with_flags.contains_key(&key(a, b)),
            "{a} <> {b} missing with {flags:?}"
        );
    }
}

/// Checks a real repository. Set `DUPED_REGRESSION_ROOT` to the repo and
/// `DUPED_REGRESSION_EXPECT` to its expectations file (see the README for the format).
#[test]
fn real_repository_expectations() {
    let (Ok(root), Ok(expect)) = (
        std::env::var("DUPED_REGRESSION_ROOT"),
        std::env::var("DUPED_REGRESSION_EXPECT"),
    ) else {
        eprintln!("skipped: set DUPED_REGRESSION_ROOT and DUPED_REGRESSION_EXPECT to run");
        return;
    };
    let text = std::fs::read_to_string(&expect).unwrap_or_else(|e| panic!("{expect}: {e}"));
    let file: ExpectationsFile = toml::from_str(&text).unwrap_or_else(|e| panic!("{expect}: {e}"));

    let scan = Path::new(&root).join(file.path.as_deref().unwrap_or("."));
    let mut args = vec![
        "--include-acknowledged",
        "--include-same-module",
        "--root",
        root.as_str(),
    ];
    args.extend(file.args.iter().map(String::as_str));
    let pairs = pairs_by_key(&types(scan.to_str().unwrap(), &args));

    let mut failures = Vec::new();
    for e in &file.expect {
        let context = format!("{} <> {}", e.a, e.b);
        // Same-named types in one module share a key; any one of them may match.
        let candidates = pairs
            .get(&key(&e.a, &e.b))
            .map(Vec::as_slice)
            .unwrap_or_default();
        let results: Vec<Vec<String>> = candidates.iter().map(|pair| mismatches(e, pair)).collect();
        match results.iter().find(|r| r.is_empty()) {
            Some(_) => {}
            None if results.is_empty() => failures.push(format!("{context}: not reported")),
            None => failures.extend(results[0].iter().map(|m| format!("{context}: {m}"))),
        }
    }
    // Known misses must stay missing, so fixing one is noticed and moved to `expect`.
    for m in &file.missing {
        if pairs.contains_key(&key(&m.a, &m.b)) {
            failures.push(format!(
                "{} <> {}: now reported ({}); move it to [[expect]]",
                m.a, m.b, m.reason
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// What differs between an expected pair and a reported one.
fn mismatches(e: &ExpectedPair, pair: &Value) -> Vec<String> {
    let tag = &pair["tag"];
    let checks = [
        ("tag", Some(e.tag.as_str()), tag["kind"].as_str()),
        ("copy", e.copy.as_deref(), tag["copy"].as_str()),
        ("owner", e.owner.as_deref(), tag["owner"].as_str()),
        ("home", e.home.as_deref(), tag["home"].as_str()),
        (
            "relationship",
            e.relationship.as_deref(),
            Some(relationship_from(pair, &e.a)),
        ),
    ];
    let mut out: Vec<String> = checks
        .into_iter()
        .filter(|(_, wanted, actual)| wanted.is_some() && wanted != actual)
        .map(|(what, wanted, actual)| format!("{what} is {actual:?}, expected {wanted:?}"))
        .collect();
    if e.acknowledged != pair["acknowledged"].is_string() {
        out.push(format!(
            "acknowledged is {}, expected {}",
            pair["acknowledged"], e.acknowledged
        ));
    }
    out
}

/// One expected pair on the fixture; for importable tags `a` is the copy and `b`'s module
/// owns the original.
struct Expect {
    a: &'static str,
    b: &'static str,
    relationship: &'static str,
    tag: Tag,
}

enum Tag {
    Importable,
    Indirect,
    MoveDown(&'static str),
    Boundary,
}

impl Expect {
    const fn importable(a: &'static str, b: &'static str, relationship: &'static str) -> Self {
        Expect {
            a,
            b,
            relationship,
            tag: Tag::Importable,
        }
    }
    const fn indirect(a: &'static str, b: &'static str) -> Self {
        Expect {
            a,
            b,
            relationship: "exact",
            tag: Tag::Indirect,
        }
    }
    const fn move_down(a: &'static str, b: &'static str, home: &'static str) -> Self {
        Expect {
            a,
            b,
            relationship: "exact",
            tag: Tag::MoveDown(home),
        }
    }
    const fn boundary(a: &'static str, b: &'static str) -> Self {
        Expect {
            a,
            b,
            relationship: "exact",
            tag: Tag::Boundary,
        }
    }

    fn tag(&self) -> Value {
        let module = |side: &str| side.split(':').next().unwrap().to_string();
        match self.tag {
            Tag::Importable => {
                json!({ "kind": "importable", "copy": module(self.a), "owner": module(self.b) })
            }
            Tag::Indirect => {
                json!({ "kind": "importable-indirect", "copy": module(self.a), "owner": module(self.b) })
            }
            Tag::MoveDown(home) => json!({ "kind": "move-down", "home": home }),
            Tag::Boundary => json!({ "kind": "boundary" }),
        }
    }
}

struct Hidden {
    a: &'static str,
    b: &'static str,
    flag: &'static str,
    tag: &'static str,
    acknowledged: Option<&'static str>,
}

/// An expectations file for a real repository.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectationsFile {
    /// Directory to scan, relative to the root [default: the root].
    path: Option<String>,
    /// Extra `duped types` arguments.
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    expect: Vec<ExpectedPair>,
    #[serde(default)]
    missing: Vec<KnownMiss>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedPair {
    a: String,
    b: String,
    tag: String,
    copy: Option<String>,
    owner: Option<String>,
    home: Option<String>,
    relationship: Option<String>,
    #[serde(default)]
    acknowledged: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KnownMiss {
    a: String,
    b: String,
    reason: String,
}

fn types(path: &str, args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_duped"))
        .args(["types", path, "--json"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// Reported pairs keyed by their two sides, order-independent.
fn pairs_by_key(report: &Value) -> BTreeMap<(String, String), Vec<Value>> {
    let mut pairs: BTreeMap<_, Vec<Value>> = BTreeMap::new();
    for p in report["pairs"].as_array().unwrap() {
        let k = key(&member_key(&p["a"]), &member_key(&p["b"]));
        pairs.entry(k).or_default().push(p.clone());
    }
    pairs
}

/// The one reported pair for `a` and `b`; the fixture never repeats a key.
fn single<'a>(
    pairs: &'a BTreeMap<(String, String), Vec<Value>>,
    a: &str,
    b: &str,
) -> Option<&'a Value> {
    let found = pairs.get(&key(a, b))?;
    assert_eq!(found.len(), 1, "{a} <> {b} reported {} times", found.len());
    found.first()
}

fn key(a: &str, b: &str) -> (String, String) {
    let (a, b) = (a.to_string(), b.to_string());
    if a <= b { (a, b) } else { (b, a) }
}

/// `module:Scope.Name` for a JSON pair member.
fn member_key(member: &Value) -> String {
    let name = member["name"].as_str().unwrap();
    let qualified = match member["scope"].as_str() {
        Some(scope) => format!("{scope}.{name}"),
        None => name.to_string(),
    };
    format!(
        "{}:{qualified}",
        member["module"].as_str().unwrap_or("(untagged)")
    )
}

/// The pair's relationship read from `side`'s point of view.
fn relationship_from<'a>(pair: &'a Value, side: &str) -> &'a str {
    let relationship = pair["relationship"].as_str().unwrap();
    if member_key(&pair["a"]) == side {
        return relationship;
    }
    match relationship {
        "subset" => "superset",
        "superset" => "subset",
        other => other,
    }
}
