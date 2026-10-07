#!/usr/bin/env node
// Runs every `duped` analysis over a directory and groups candidates by file pair.
// Diff mode keeps pairs with a side in a changed file; --all keeps everything.

import { spawnSync } from "node:child_process";
import { realpathSync } from "node:fs";
import path from "node:path";

const DETECTORS = ["bodies", "imports", "names", "types"];
const MAX_BUFFER = 256 * 1024 * 1024;

function main() {
  const options = parseArguments(process.argv.slice(2));
  if (!options.all && !options.base) fail("--base is required unless --all is used");
  requireDuped();

  const changed = options.all ? null : changedFiles(options.dir, options.base);
  const detectors = {};
  const kept = [];
  for (const name of DETECTORS) {
    const run = spawnSync(
      "duped",
      [name, options.dir, "--json", ...options.exclude.flatMap((e) => ["--exclude", e])],
      { encoding: "utf8", maxBuffer: MAX_BUFFER }
    );
    if (run.status !== 0) {
      detectors[name] = { status: "failed", error: (run.stderr || run.error?.message || "").trim() };
      continue;
    }
    let found;
    try {
      found = normalize[name](JSON.parse(run.stdout));
    } catch (error) {
      detectors[name] = { status: "failed", error: `unexpected output: ${error.message}` };
      continue;
    }
    const mine = changed ? found.filter((f) => changed.has(f.a.file) || changed.has(f.b.file)) : found;
    for (const f of mine) kept.push(f);
    // `total` is repo-wide; `kept` is what survives the diff filter.
    detectors[name] = { status: "ok", total: found.length, kept: mine.length };
  }

  const groups = groupByFilePair(kept, changed);
  console.log(
    JSON.stringify(
      {
        directory: options.dir,
        mode: changed ? "diff" : "all",
        base: options.base,
        changedFiles: changed?.size,
        detectors,
        filePairs: groups.length,
        truncated: groups.length > options.top,
        groups: groups.slice(0, options.top),
      },
      null,
      2
    )
  );
}

// One shape per finding: { detector, kind, tag, a: { file, line?, name? }, b, evidence }.
// `rank` orders findings within a group and is dropped from the output.
const normalize = {
  bodies: (r) => [
    ...r.pairs.map((p) => ({
      detector: "bodies",
      kind: "function",
      rank: p.shared,
      tag: p.tag,
      a: side(p.a),
      b: side(p.b),
      evidence: `${round(p.similarity)} similar, ${p.shared} shared shingles (${p.a.tokens}/${p.b.tokens} tokens)`,
    })),
    ...r.files.map((f) => ({
      detector: "bodies",
      kind: "file",
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
      rank: p.rare,
      tag: p.tag,
      a: { file: p.a.file },
      b: { file: p.b.file },
      evidence: `${round(p.score)} score, ${p.rare} rare shared imports: ${p.shared
        .slice(0, 6)
        .map((s) => s.item)
        .join(", ")}`,
    })),
  names: (r) =>
    r.pairs.map((p) => ({
      detector: "names",
      kind: "file",
      rank: p.score,
      tag: p.tag,
      a: { file: p.a.file },
      b: { file: p.b.file },
      evidence: p.shared
        .slice(0, 8)
        .map(
          (s) =>
            `${s.same === true ? "=" : s.same === false ? "~" : "?"} ${s.name}${s.exported ? " (exported)" : ""} (:${s.a.line} :${s.b.line})`
        )
        .join(", "),
    })),
  types: (r) =>
    r.pairs.map((p) => ({
      detector: "types",
      kind: "type",
      rank: p.similarity,
      tag: p.tag,
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
    .map((g) => ({
      a: g.a,
      b: g.b,
      changed: changedSet ? [g.a, g.b].filter((f) => changedSet.has(f)) : undefined,
      detectors: [...new Set(g.findings.map((f) => f.detector))].sort(),
      tag: g.findings.find((f) => f.tag)?.tag ?? null,
      findings: g.findings.sort((x, y) => y.rank - x.rank).map(({ rank, ...rest }) => rest),
    }))
    .sort((x, y) => y.detectors.length - x.detectors.length || y.findings.length - x.findings.length);
}

function side(s) {
  return { file: s.file, line: s.line, name: s.scope ? `${s.scope}.${s.name}` : s.name };
}

// Files the branch added or modified since its merge base, plus uncommitted and untracked
// ones, relative to `dir` (duped's paths are relative to the directory it scans).
function changedFiles(dir, base) {
  const top = git(["rev-parse", "--show-toplevel"], dir).trim();
  const mergeBase = git(["merge-base", base, "HEAD"], dir).trim();
  // -z: unquoted paths, so `café.ts` matches what duped reports.
  const listed = [
    git(["diff", "-z", "--name-only", "--diff-filter=AMR", mergeBase], top),
    git(["ls-files", "-z", "--others", "--exclude-standard"], top),
  ];
  // realpath: git reports the resolved toplevel (macOS `/tmp` is `/private/tmp`).
  const scanned = realpathSync(dir);
  const files = listed
    .join("\0")
    .split("\0")
    .filter(Boolean)
    .map((f) => path.relative(scanned, path.join(top, f)).split(path.sep).join("/"))
    .filter((f) => !f.startsWith("../"));
  return new Set(files);
}

function git(args, cwd) {
  const run = spawnSync("git", args, { cwd, encoding: "utf8", maxBuffer: MAX_BUFFER });
  if (run.status !== 0) fail(`git ${args.join(" ")}: ${(run.stderr || run.error?.message || "").trim()}`);
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
  const parsed = { dir: process.cwd(), all: false, top: 40, exclude: [] };
  for (let i = 0; i < args.length; i += 1) {
    const arg = args[i];
    const value = () => args[++i] ?? fail(`${arg} requires a value`);
    if (arg === "--all") parsed.all = true;
    else if (arg === "--repo") parsed.dir = path.resolve(value());
    else if (arg === "--base") parsed.base = value();
    else if (arg === "--exclude") parsed.exclude.push(value());
    else if (arg === "--top") {
      parsed.top = Number(value());
      if (!Number.isInteger(parsed.top) || parsed.top < 1) fail("--top needs a positive integer");
    } else if (arg === "--help" || arg === "-h") {
      console.log("scan.mjs [--repo DIR] (--base REF | --all) [--exclude GLOB]... [--top N]");
      process.exit(0);
    } else fail(`unknown argument: ${arg}`);
  }
  return parsed;
}

function fail(message) {
  process.stderr.write(`scan.mjs: ${message}\n`);
  process.exit(2);
}

main();
