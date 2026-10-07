#!/usr/bin/env node
// Runs every `duped` analysis over a repo and groups candidates by file pair.
// Diff mode keeps pairs with a side in a changed file; --all keeps everything.

import { spawnSync } from "node:child_process";
import path from "node:path";

const DETECTORS = ["bodies", "imports", "names", "types"];
const MAX_BUFFER = 256 * 1024 * 1024;

function main() {
  const options = parseArguments(process.argv.slice(2));
  const repo = git(["rev-parse", "--show-toplevel"], options.repo).trim();
  if (!options.all && !options.base) fail("--base is required unless --all is used");
  requireDuped();

  const changed = options.all ? null : changedFiles(repo, options.base);
  const detectors = {};
  const findings = [];
  for (const name of options.only) {
    const started = Date.now();
    const run = spawnSync("duped", [name, repo, "--json", ...options.exclude.flatMap((e) => ["--exclude", e])], { cwd: repo, encoding: "utf8", maxBuffer: MAX_BUFFER });
    if (run.status !== 0) {
      detectors[name] = { status: "failed", error: (run.stderr || run.error?.message || "").trim() };
      continue;
    }
    const report = JSON.parse(run.stdout);
    const found = normalize[name](report);
    detectors[name] = { status: "ok", ms: Date.now() - started, total: found.length, summary: report.summary };
    findings.push(...found);
  }

  const kept = changed ? findings.filter((f) => changed.has(f.a.file) || changed.has(f.b.file)) : findings;
  const groups = groupByFilePair(kept, changed).slice(0, options.top);
  process.stdout.write(
    `${JSON.stringify(
      {
        repository: repo,
        mode: changed ? "diff" : "all",
        ...(options.base ? { base: options.base } : {}),
        changedFiles: changed ? changed.size : undefined,
        detectors,
        filePairs: groups.length,
        groups,
      },
      null,
      2
    )}\n`
  );
}

// One shape per finding: { detector, kind, score, tag, a: { file, line?, name? }, b, evidence }.
const normalize = {
  bodies: (r) => [
    ...r.pairs.map((p) => ({
      detector: "bodies",
      kind: "function",
      score: round(p.similarity),
      rank: p.shared,
      tag: p.tag,
      acknowledged: p.acknowledged,
      a: side(p.a),
      b: side(p.b),
      evidence: `${round(p.similarity)} similar, ${p.shared} shared shingles (${p.a.tokens}/${p.b.tokens} tokens)`,
    })),
    ...r.files.map((f) => ({
      detector: "bodies",
      kind: "file",
      score: f.pairs.length,
      rank: f.shared,
      tag: f.tag,
      a: { file: f.a },
      b: { file: f.b },
      evidence: `${f.pairs.length} function pairs, ${f.shared} shared shingles`,
    })),
  ],
  imports: (r) =>
    r.pairs.map((p) => ({
      detector: "imports",
      kind: "file",
      score: round(p.score),
      rank: p.rare,
      tag: p.tag,
      a: { file: p.a.file },
      b: { file: p.b.file },
      evidence: `${p.rare} rare shared imports: ${p.shared
        .slice(0, 6)
        .map((s) => s.item)
        .join(", ")}`,
    })),
  names: (r) =>
    r.pairs.map((p) => ({
      detector: "names",
      kind: "file",
      score: round(p.score),
      rank: p.score,
      tag: p.tag,
      a: { file: p.a.file },
      b: { file: p.b.file },
      evidence: p.shared
        .slice(0, 8)
        .map((s) => `${s.same === true ? "=" : s.same === false ? "~" : "?"} ${s.name} (:${s.a.line} :${s.b.line})`)
        .join(", "),
    })),
  types: (r) =>
    r.pairs.map((p) => ({
      detector: "types",
      kind: "type",
      score: round(p.similarity),
      rank: p.similarity,
      tag: p.tag,
      acknowledged: p.acknowledged,
      a: side(p.a),
      b: side(p.b),
      evidence: `${p.relationship}, ${round(p.similarity)} field overlap: ${p.shared.slice(0, 8).join(", ")}`,
    })),
};

// More detectors agreeing on one file pair is the strongest signal of a ported module.
function groupByFilePair(list, changedSet) {
  const byPair = new Map();
  for (const f of list) {
    const [a, b] = [f.a.file, f.b.file].sort();
    const key = `${a}\0${b}`;
    if (!byPair.has(key)) byPair.set(key, { a, b, findings: [] });
    byPair.get(key).findings.push(f);
  }
  return [...byPair.values()]
    .map((g) => {
      const found = [...new Set(g.findings.map((f) => f.detector))].sort();
      return {
        a: g.a,
        b: g.b,
        changed: changedSet ? [g.a, g.b].filter((f) => changedSet.has(f)) : undefined,
        detectors: found,
        tag: g.findings.find((f) => f.tag)?.tag ?? null,
        findings: g.findings.sort((x, y) => y.rank - x.rank).map(({ rank, ...rest }) => rest),
      };
    })
    .sort((x, y) => y.detectors.length - x.detectors.length || y.findings.length - x.findings.length);
}

function side(s) {
  return { file: s.file, line: s.line, name: s.scope ? `${s.scope}.${s.name}` : s.name };
}

function changedFiles(root, base) {
  const mergeBase = git(["merge-base", base, "HEAD"], root).trim();
  const lists = [
    git(["diff", "--name-only", "--diff-filter=AMR", mergeBase], root),
    git(["ls-files", "--others", "--exclude-standard"], root),
  ];
  return new Set(lists.join("\n").split("\n").filter(Boolean));
}

function git(args, cwd) {
  const run = spawnSync("git", args, { cwd, encoding: "utf8", maxBuffer: MAX_BUFFER });
  if (run.status !== 0) fail(`git ${args.join(" ")}: ${run.stderr.trim()}`);
  return run.stdout;
}

function requireDuped() {
  const run = spawnSync("duped", ["--version"], { encoding: "utf8" });
  if (run.status !== 0) fail("`duped` not on PATH. Install: cargo install --locked --git https://github.com/bryan-uipath/duped");
}

function round(n) {
  return Math.round(n * 100) / 100;
}

function parseArguments(args) {
  const parsed = { repo: process.cwd(), all: false, only: DETECTORS, top: 40, exclude: [] };
  for (let i = 0; i < args.length; i += 1) {
    const arg = args[i];
    const value = () => args[++i] ?? fail(`${arg} requires a value`);
    if (arg === "--all") parsed.all = true;
    else if (arg === "--repo") parsed.repo = path.resolve(value());
    else if (arg === "--base") parsed.base = value();
    else if (arg === "--top") parsed.top = Number(value());
    else if (arg === "--only") parsed.only = value().split(",");
    else if (arg === "--exclude") parsed.exclude.push(value());
    else if (arg === "--help" || arg === "-h") {
      process.stdout.write("scan.mjs [--repo DIR] (--base REF | --all) [--only bodies,imports,names,types] [--exclude GLOB]... [--top N]\n");
      process.exit(0);
    } else fail(`unknown argument: ${arg}`);
  }
  const unknown = parsed.only.filter((d) => !DETECTORS.includes(d));
  if (unknown.length) fail(`unknown detector: ${unknown.join(", ")}`);
  return parsed;
}

function fail(message) {
  process.stderr.write(`scan.mjs: ${message}\n`);
  process.exit(2);
}

main();
