//! `duped report`: every analysis's pairs as findings, grouped by file pair, optionally kept
//! to the pairs a branch touches.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::analysis::bodies::{BodyReport, file_shared, function_at};
use crate::analysis::imports::{FileImports, ImportReport};
use crate::analysis::names::NameReport;
use crate::analysis::pair_notes;
use crate::analysis::types::TypeReport;
use crate::modules::{self, Tag};
use crate::record::{Record, qualified};

/// Everything `report` prints; the JSON keys are the skills' contract.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub directory: String,
    /// `diff` with `--base`, else `all`.
    pub mode: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed_files: Option<usize>,
    pub detectors: BTreeMap<&'static str, Status>,
    /// Groups before `--top`.
    pub file_pairs: usize,
    pub truncated: bool,
    pub groups: Vec<Group>,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum Status {
    /// `total` counts the analysis's pairs across the scan; `kept` those touching the change.
    Ok {
        total: usize,
        kept: usize,
    },
    Failed {
        error: String,
    },
}

/// Findings on one unordered file pair; `a` sorts first.
#[derive(Serialize)]
pub struct Group {
    pub a: String,
    pub b: String,
    /// With `--base`: the sides in changed files.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed: Option<Vec<String>>,
    /// Analyses that found the pair, sorted.
    pub detectors: Vec<&'static str>,
    /// The first tagged finding's tag.
    pub tag: Option<Tag>,
    /// Highest rank first.
    pub findings: Vec<Finding>,
}

/// One pair from one analysis.
#[derive(Serialize)]
pub struct Finding {
    pub detector: &'static str,
    /// `function`, `type` or `file`.
    pub kind: &'static str,
    pub tag: Option<Tag>,
    pub a: Side,
    pub b: Side,
    pub evidence: String,
    /// The analysis's own ranking score, e.g. shared shingles for `bodies`.
    #[serde(skip)]
    rank: f64,
}

#[derive(Serialize)]
pub struct Side {
    pub file: String,
    /// Declaration line and qualified name, for function and type findings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Report {
    /// Groups each analysis's findings, or records why it failed; `changed` keeps only pairs
    /// with a side in it.
    pub fn new(
        directory: String,
        base: Option<&str>,
        changed: Option<HashSet<String>>,
        analyses: Vec<(&'static str, Result<Vec<Finding>, String>)>,
        top: usize,
    ) -> Self {
        let mut detectors = BTreeMap::new();
        let mut kept = Vec::new();
        for (name, result) in analyses {
            let status = match result {
                Ok(found) => {
                    let total = found.len();
                    let touches = |f: &Finding| {
                        changed
                            .as_ref()
                            .is_none_or(|c| c.contains(&f.a.file) || c.contains(&f.b.file))
                    };
                    let before = kept.len();
                    kept.extend(found.into_iter().filter(touches));
                    Status::Ok {
                        total,
                        kept: kept.len() - before,
                    }
                }
                Err(error) => Status::Failed { error },
            };
            detectors.insert(name, status);
        }
        let mut groups = group_by_file_pair(kept, changed.as_ref());
        let file_pairs = groups.len();
        groups.truncate(top);
        Report {
            directory,
            mode: if changed.is_some() { "diff" } else { "all" },
            base: base.map(str::to_string),
            changed_files: changed.as_ref().map(HashSet::len),
            detectors,
            file_pairs,
            truncated: file_pairs > top,
            groups,
        }
    }
}

/// Runs one analysis; an error or panic fails only it, as a crashed subprocess would.
pub fn attempt(run: impl FnOnce() -> Result<Vec<Finding>>) -> Result<Vec<Finding>, String> {
    match catch_unwind(AssertUnwindSafe(run)) {
        Ok(result) => result.map_err(|err| format!("{err:#}")),
        Err(panic) => Err(panic
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panicked".to_string())),
    }
}

// ----- findings -----

/// Function pairs, then files sharing several functions.
pub fn bodies(records: &[Record], report: &BodyReport) -> Vec<Finding> {
    let pairs = report.pairs.iter().map(|p| {
        let tokens = |i| function_at(records, i).body.len();
        Finding {
            detector: "bodies",
            kind: "function",
            tag: p.tag.clone(),
            a: side(&records[p.a]),
            b: side(&records[p.b]),
            evidence: format!(
                "{} similar, {} shared shingles ({}/{} tokens)",
                round(p.similarity),
                p.shared,
                tokens(p.a),
                tokens(p.b)
            ),
            rank: p.shared as f64,
        }
    });
    let files = report.files.iter().map(|f| {
        let shared = file_shared(f, &report.pairs);
        Finding {
            detector: "bodies",
            kind: "file",
            tag: report.pairs[f.pairs[0]].tag.clone(),
            a: file_side(&f.a),
            b: file_side(&f.b),
            evidence: format!("{} function pairs, {shared} shared shingles", f.pairs.len()),
            rank: shared as f64,
        }
    });
    pairs.chain(files).collect()
}

pub fn imports(files: &[FileImports], report: &ImportReport) -> Vec<Finding> {
    report
        .pairs
        .iter()
        .map(|p| {
            let shown: Vec<&str> = p.shared.iter().take(6).map(|(i, _)| i.as_str()).collect();
            Finding {
                detector: "imports",
                kind: "file",
                tag: p.tag.clone(),
                a: file_side(&files[p.a].file),
                b: file_side(&files[p.b].file),
                evidence: format!(
                    "{} score, {} rare shared imports: {}",
                    round(p.score),
                    p.rare,
                    shown.join(", ")
                ),
                rank: p.rare as f64,
            }
        })
        .collect()
}

pub fn names(records: &[Record], report: &NameReport) -> Vec<Finding> {
    report
        .pairs
        .iter()
        .map(|p| {
            // `=` same signature or shape, `~` different, `?` unknown (an alias).
            let shown: Vec<String> = p
                .shared
                .iter()
                .take(8)
                .map(|s| {
                    let mark = match s.same {
                        Some(true) => "=",
                        Some(false) => "~",
                        None => "?",
                    };
                    let exported = if s.exported { " (exported)" } else { "" };
                    format!(
                        "{mark} {}{exported} (:{} :{})",
                        s.name,
                        records[s.a].location().start_line,
                        records[s.b].location().start_line
                    )
                })
                .collect();
            Finding {
                detector: "names",
                kind: "file",
                tag: p.tag.clone(),
                a: file_side(&p.a),
                b: file_side(&p.b),
                evidence: shown.join(", "),
                rank: p.score,
            }
        })
        .collect()
}

pub fn types(records: &[Record], report: &TypeReport) -> Vec<Finding> {
    report
        .pairs
        .iter()
        .map(|p| {
            let relationship = serde_json::to_value(p.relationship)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default();
            let shown: Vec<&str> = p.shared.iter().take(8).map(String::as_str).collect();
            Finding {
                detector: "types",
                kind: "type",
                tag: p.tag.clone(),
                a: side(&records[p.a]),
                b: side(&records[p.b]),
                evidence: format!(
                    "{relationship}, {} field overlap: {}",
                    round(p.similarity),
                    shown.join(", ")
                ),
                rank: p.similarity,
            }
        })
        .collect()
}

fn side(record: &Record) -> Side {
    Side {
        file: record.location().file.clone(),
        line: Some(record.location().start_line),
        name: Some(qualified(record.scope(), record.name())),
    }
}

fn file_side(file: &str) -> Side {
    Side {
        file: file.to_string(),
        line: None,
        name: None,
    }
}

/// Two decimals, printed without trailing zeros: `0.5`, `0.83`, `1`.
fn round(n: f64) -> f64 {
    (n * 100.0).round() / 100.0
}

/// More analyses agreeing on one file pair is the strongest signal of a ported module, so
/// groups sort by that, then by finding count; ties keep the analyses' order.
fn group_by_file_pair(findings: Vec<Finding>, changed: Option<&HashSet<String>>) -> Vec<Group> {
    let mut index: HashMap<(String, String), usize> = HashMap::new();
    let mut groups: Vec<Group> = Vec::new();
    for f in findings {
        let (a, b) = if f.a.file <= f.b.file {
            (&f.a.file, &f.b.file)
        } else {
            (&f.b.file, &f.a.file)
        };
        let key = (a.clone(), b.clone());
        let at = *index.entry(key).or_insert_with(|| {
            groups.push(Group {
                a: a.clone(),
                b: b.clone(),
                changed: None,
                detectors: Vec::new(),
                tag: None,
                findings: Vec::new(),
            });
            groups.len() - 1
        });
        let group = &mut groups[at];
        if group.tag.is_none() {
            group.tag = f.tag.clone();
        }
        if !group.detectors.contains(&f.detector) {
            group.detectors.push(f.detector);
        }
        group.findings.push(f);
    }
    for g in &mut groups {
        g.detectors.sort_unstable();
        g.findings.sort_by(|x, y| y.rank.total_cmp(&x.rank));
        g.changed = changed.map(|c| {
            [&g.a, &g.b]
                .into_iter()
                .filter(|f| c.contains(*f))
                .cloned()
                .collect()
        });
    }
    groups.sort_by(|x, y| {
        y.detectors
            .len()
            .cmp(&x.detectors.len())
            .then(y.findings.len().cmp(&x.findings.len()))
    });
    groups
}

// ----- changed files -----

/// Files added, modified or renamed since the merge base with `base`, committed or not, plus
/// untracked ones, under the canonical `scanned` path (a directory or one file), relative to
/// `dir`, the directory findings' paths are relative to.
pub fn changed_files(scanned: &Path, dir: &Path, base: &str) -> Result<HashSet<String>> {
    if base.starts_with('-') {
        bail!("--base `{base}` is not a ref");
    }
    let top = git(&["rev-parse", "--show-toplevel"], dir)?;
    // Canonical, like `dir`: macOS's `/tmp` is `/private/tmp`.
    let top = Path::new(top.trim())
        .canonicalize()
        .with_context(|| format!("cannot open git checkout {}", top.trim()))?;
    let merge_base = git(&["merge-base", base, "HEAD"], dir)?;
    // -z: unquoted paths, so `café.ts` matches the scanned path.
    let diff = git(
        &[
            "diff",
            "-z",
            "--name-only",
            "--diff-filter=AMR",
            merge_base.trim(),
            "--",
        ],
        &top,
    )?;
    let untracked = git(&["ls-files", "-z", "--others", "--exclude-standard"], &top)?;
    Ok(diff
        .split('\0')
        .chain(untracked.split('\0'))
        .filter(|f| !f.is_empty())
        .map(|f| top.join(f))
        .filter(|path| path.starts_with(scanned))
        .map(|path| modules::relative(dir, &path))
        .collect())
}

fn git(args: &[&str], cwd: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .context("cannot run git")?;
    if !output.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

// ----- output -----

/// Human-readable report: what each analysis found, then the ranked file pairs.
pub fn render_text(report: &Report) -> String {
    let mut out = String::new();
    let counts: Vec<String> = report
        .detectors
        .iter()
        .map(|(name, status)| match status {
            Status::Ok { total, kept } if report.base.is_some() => {
                format!("{name} {kept} of {total}")
            }
            Status::Ok { total, .. } => format!("{name} {total}"),
            Status::Failed { error } => format!("{name} failed: {error}"),
        })
        .collect();
    match (&report.base, report.changed_files) {
        (Some(base), Some(changed)) => writeln!(
            out,
            "{} file pairs touching {changed} files changed since {base} ({}).",
            report.file_pairs,
            counts.join("; ")
        ),
        _ => writeln!(
            out,
            "{} file pairs ({}).",
            report.file_pairs,
            counts.join("; ")
        ),
    }
    .ok();
    for (rank, group) in report.groups.iter().enumerate() {
        writeln!(
            out,
            "\n{}. {}: {} findings{}",
            rank + 1,
            group.detectors.join(" + "),
            group.findings.len(),
            pair_notes(&group.tag, &None)
        )
        .ok();
        for file in [&group.a, &group.b] {
            let changed = if group.changed.as_ref().is_some_and(|c| c.contains(file)) {
                " (changed)"
            } else {
                ""
            };
            writeln!(out, "   {file}{changed}").ok();
        }
        for f in &group.findings {
            // Sides in the group's order, e.g. ` (loadUser:12 ~ fetchUser:30)`.
            let (a, b) = if f.a.file == group.a {
                (&f.a, &f.b)
            } else {
                (&f.b, &f.a)
            };
            let members = match (&a.name, a.line, &b.name, b.line) {
                (Some(an), Some(al), Some(bn), Some(bl)) => format!(" ({an}:{al} ~ {bn}:{bl})"),
                _ => String::new(),
            };
            writeln!(
                out,
                "   - {} {}: {}{members}",
                f.detector, f.kind, f.evidence
            )
            .ok();
        }
    }
    if report.truncated {
        writeln!(
            out,
            "\n… {} more file pairs; raise --top to see them.",
            report.file_pairs - report.groups.len()
        )
        .ok();
    }
    out
}
