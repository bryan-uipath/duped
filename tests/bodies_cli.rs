//! End-to-end: `duped bodies` over a synthetic workspace where one package ports another's
//! panel: the component is renamed, an `auth` parameter is threaded through, and a
//! cancellation guard is added.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

/// `web → core`, `ext → core`; `ext` has a port of `web`'s report panel.
fn workspace(test: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("duped-bodies-e2e-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
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
        ("packages/web/src/ReportPanel.tsx", WEB_PANEL),
        ("packages/ext/src/ExtReportPanel.tsx", EXT_PANEL),
        ("packages/core/src/format.ts", FORMAT),
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

fn bodies_json(root: &Path, extra: &[&str]) -> Value {
    let mut args = vec!["bodies", root.to_str().unwrap(), "--json"];
    args.extend(extra);
    serde_json::from_str(&duped(&args)).unwrap()
}

/// `(a, b)` names of every reported pair, sorted.
fn pairs(json: &Value) -> Vec<(String, String)> {
    let mut out: Vec<_> = json["pairs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            let name = |side: &str| p[side]["name"].as_str().unwrap().to_string();
            (name("a"), name("b"))
        })
        .collect();
    out.sort();
    out
}

#[test]
fn finds_a_ported_module_and_its_functions() {
    let root = workspace("ported");
    let json = bodies_json(&root, &[]);
    assert_eq!(
        pairs(&json),
        vec![
            ("ExtReportPanel".into(), "ReportPanel".into()),
            ("ReportContent".into(), "ReportContent".into()),
            ("getOrCreateClient".into(), "getOrCreateClient".into()),
        ]
    );
    for pair in json["pairs"].as_array().unwrap() {
        assert_eq!(pair["tag"]["kind"], "move-down", "{pair}");
        assert!(pair["similarity"].as_f64().unwrap() < 1.0, "{pair}");
    }
    let files = json["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["a"], "packages/ext/src/ExtReportPanel.tsx");
    assert_eq!(files[0]["b"], "packages/web/src/ReportPanel.tsx");
    assert_eq!(files[0]["pairs"].as_array().unwrap().len(), 3);
    // `formatBytes` and `formatRate` are near-copies in one file.
    assert_eq!(json["summary"]["hidden"]["same_file"], 1);

    let text = duped(&["bodies", root.to_str().unwrap()]);
    assert!(text.contains("Files sharing several functions:"), "{text}");
    assert!(
        text.contains("move-down: both depend on @demo/core"),
        "{text}"
    );
    assert!(text.contains("ReportContent ~ ReportContent"), "{text}");

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn flags_reach_the_analysis() {
    let root = workspace("flags");
    let count = |extra: &[&str]| {
        bodies_json(&root, extra)["summary"]["pairs"]
            .as_u64()
            .unwrap()
    };
    assert_eq!(count(&[]), 3);
    assert_eq!(count(&["--include-same-file"]), 4);
    assert_eq!(count(&["--threshold", "0.99"]), 0);
    assert_eq!(count(&["--min-tokens", "100000"]), 0);
    assert!(count(&["--shingle", "2"]) >= 3);

    let truncated = duped(&["bodies", root.to_str().unwrap(), "--top", "1"]);
    assert!(
        truncated.contains("… 2 more function pairs; raise --top"),
        "{truncated}"
    );

    // A `duped.toml` acknowledgement hides the pair unless asked for.
    std::fs::write(
        root.join("duped.toml"),
        "[[acknowledged]]\na = \"@demo/ext:getOrCreateClient\"\nb = \"@demo/web:getOrCreateClient\"\nreason = \"host-specific auth\"\n",
    )
    .unwrap();
    let json = bodies_json(&root, &[]);
    assert_eq!(
        (
            &json["summary"]["pairs"],
            &json["summary"]["hidden"]["acknowledged"]
        ),
        (&2.into(), &1.into())
    );
    let json = bodies_json(&root, &["--include-acknowledged"]);
    assert!(
        json["pairs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["acknowledged"] == "host-specific auth")
    );

    let rejected = Command::new(env!("CARGO_BIN_EXE_duped"))
        .args(["bodies", ".", "--shingle", "0"])
        .output()
        .unwrap();
    assert!(!rejected.status.success());

    std::fs::remove_dir_all(&root).unwrap();
}

const WEB_PANEL: &str = r#"import { useEffect, useMemo, useState } from 'react';
import { useReportStore, type ReportSession } from '@demo/core';
import { getCredentials } from './credentials';

let clientInstance: Client | null = null;
let clientPromise: Promise<Client> | null = null;

export function resetClient(): void {
  clientInstance = null;
  clientPromise = null;
}

function getOrCreateClient(): Promise<Client> {
  // Reused for the whole session.
  if (clientInstance) return Promise.resolve(clientInstance);
  if (clientPromise) return clientPromise;

  const creds = getCredentials();
  if (!creds) {
    return Promise.reject(new Error('Missing credentials for the report client'));
  }

  const client = new Client({
    baseUrl: window.location.origin,
    account: creds.accountId,
    region: creds.region,
    token: creds.token,
  });

  clientPromise = client
    .connect()
    .then(() => {
      clientInstance = client;
      clientPromise = null;
      return client;
    })
    .catch((err) => {
      clientPromise = null;
      clientInstance = null;
      throw err;
    });

  return clientPromise;
}

export function ReportPanel({ compact = false }: { compact?: boolean }) {
  const sessionsById = useReportStore((s) => s.sessions);
  const sessions = useMemo(() => Object.values(sessionsById).sort((a, b) => a.order - b.order), [sessionsById]);
  const session = sessions.length > 0 ? sessions[sessions.length - 1] : null;

  if (!session) {
    return <Empty className="h-full" title="No reports" description="Reports appear when a run produces one" />;
  }

  return (
    <div className="h-full">
      <ReportContent session={session} compact={compact} />
    </div>
  );
}

function ReportContent({ session, compact = false }: { session: ReportSession; compact?: boolean }) {
  const [client, setClient] = useState<Client | null>(clientInstance);
  const updateSession = useReportStore((s) => s.updateSession);

  useEffect(() => {
    getOrCreateClient()
      .then(setClient)
      .catch((err) => {
        console.warn('[ReportPanel] client failed:', err);
        updateSession(session.id, {
          status: 'error',
          error: err instanceof Error ? err.message : 'Failed to connect',
        });
      });
  }, [session.id, updateSession]);

  if (session.status === 'error') {
    return <Empty className="h-full" title="Failed to load report" description={session.error || 'Could not load the report.'} />;
  }

  if (!client || !session.id) {
    return <Spinner className="h-8 w-8" label="Loading report..." />;
  }

  return (
    <div className="report-container flex h-full flex-col">
      <Viewer
        key={`${compact ? 'compact' : 'full'}-${session.id}`}
        client={client}
        reportId={session.id}
        compact={compact}
        pages={session.pages}
        options={{ zoom: true, print: false, share: true }}
      />
    </div>
  );
}
"#;

const EXT_PANEL: &str = r#"/** Ported from the web report panel; auth comes from the extension host. */
import { useEffect, useMemo, useState } from 'react';
import { useReportStore, type ReportSession } from '@demo/core';

export interface ClientAuth {
  baseUrl: string;
  accountId: string;
  region: string;
  token: string;
}

let clientInstance: Client | null = null;
let clientPromise: Promise<Client> | null = null;

export function resetClient(): void {
  clientInstance = null;
  clientPromise = null;
}

function getOrCreateClient(auth: ClientAuth): Promise<Client> {
  if (clientInstance) return Promise.resolve(clientInstance);
  if (clientPromise) return clientPromise;

  const client = new Client({
    baseUrl: auth.baseUrl,
    account: auth.accountId,
    region: auth.region,
    token: auth.token,
  });

  clientPromise = client
    .connect()
    .then(() => {
      clientInstance = client;
      clientPromise = null;
      return client;
    })
    .catch((err) => {
      clientPromise = null;
      clientInstance = null;
      throw err;
    });

  return clientPromise;
}

export function ExtReportPanel({ auth, compact = false }: { auth: ClientAuth | null; compact?: boolean }) {
  const sessionsById = useReportStore((s) => s.sessions);
  const sessions = useMemo(() => Object.values(sessionsById).sort((a, b) => a.order - b.order), [sessionsById]);
  const session = sessions.length > 0 ? sessions[sessions.length - 1] : null;

  if (!session) {
    return <Empty className="h-full" title="No reports" description="Reports show up once a run produces one" />;
  }

  return (
    <div className="h-full">
      <ReportContent session={session} auth={auth} compact={compact} />
    </div>
  );
}

function ReportContent({ session, auth, compact = false }: { session: ReportSession; auth: ClientAuth | null; compact?: boolean }) {
  const [client, setClient] = useState<Client | null>(clientInstance);
  const updateSession = useReportStore((s) => s.updateSession);

  useEffect(() => {
    if (!auth) return;
    let cancelled = false;
    getOrCreateClient(auth)
      .then((instance) => {
        if (!cancelled) setClient(instance);
      })
      .catch((err) => {
        if (cancelled) return;
        console.warn('[ExtReportPanel] client failed:', err);
        updateSession(session.id, {
          status: 'error',
          error: err instanceof Error ? err.message : 'Failed to connect',
        });
      });
    return () => {
      cancelled = true;
    };
  }, [auth, session.id, updateSession]);

  if (session.status === 'error') {
    return <Empty className="h-full" title="Failed to load report" description={session.error || 'Could not load the report.'} />;
  }

  if (!auth) {
    return <Empty className="h-full" title="Sign in to view reports" description="Reports need an active session." />;
  }

  if (!client) {
    return <Spinner className="h-8 w-8" label="Loading report..." />;
  }

  return (
    <div className="report-container flex h-full flex-col">
      <Viewer
        key={`${compact ? 'compact' : 'full'}-${session.id}`}
        client={client}
        reportId={session.id}
        compact={compact}
        pages={session.pages}
        options={{ zoom: true, print: false, share: true }}
      />
    </div>
  );
}
"#;

const FORMAT: &str = r#"export function formatBytes(value: number, digits = 1): string {
  if (!Number.isFinite(value) || value < 0) return '-';
  const units = ['B', 'KB', 'MB', 'GB'];
  let index = 0;
  while (value >= 1024 && index < units.length - 1) {
    value /= 1024;
    index += 1;
  }
  return `${value.toFixed(digits)} ${units[index]}`;
}

export function formatRate(value: number, digits = 1): string {
  if (!Number.isFinite(value) || value < 0) return '-';
  const units = ['B/s', 'KB/s', 'MB/s', 'GB/s'];
  let index = 0;
  while (value >= 1024 && index < units.length - 1) {
    value /= 1024;
    index += 1;
  }
  return `${value.toFixed(digits)} ${units[index]}`;
}
"#;
