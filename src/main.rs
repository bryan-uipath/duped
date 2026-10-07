mod analysis;
mod config;
mod index;
mod judge;
mod lang;
mod modules;
mod record;
mod rules;
mod walk;

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::analysis::imports::ImportOptions;
use crate::config::Config;
use crate::rules::Rules;

/// Find duplicated functions and types across a codebase.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write every top-level function and type as JSON Lines.
    Extract(Scan),
    /// Write a greppable markdown summary of every function and type.
    Index(Scan),
    /// Find types that share most of their properties, whatever they are called.
    Types(TypesArgs),
    /// Find functions whose bodies are near-copies, whatever they are called.
    Bodies(BodiesArgs),
    /// Find TypeScript/JavaScript files that import the same rare things, often ported copies.
    Imports(ImportsArgs),
}

#[derive(Args)]
struct BodiesArgs {
    #[command(flatten)]
    scan: Scan,
    /// Minimum similarity of normalised token shingles (Jaccard, 0–1).
    #[arg(long, value_parser = fraction, default_value_t = 0.5)]
    threshold: f64,
    /// Tokens per shingle.
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u16).range(1..))]
    shingle: u16,
    /// Skip bodies with fewer tokens.
    #[arg(long, default_value_t = 50)]
    min_tokens: usize,
    /// Also report pairs whose two functions are in the same file.
    #[arg(long)]
    include_same_file: bool,
    /// Also report pairs acknowledged as deliberate (in `duped.toml` or by a doc comment).
    #[arg(long)]
    include_acknowledged: bool,
    /// How many file pairs and function pairs to show.
    #[arg(long, default_value_t = 40)]
    top: usize,
    /// Emit every pair and file pair as JSON (ignores `--top`).
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ImportsArgs {
    #[command(flatten)]
    scan: Scan,
    /// A pair needs shared rare imports from at least this many specifiers.
    #[arg(long, default_value_t = 3)]
    min_shared: usize,
    /// Minimum IDF-weighted Jaccard similarity of import items (0–1).
    #[arg(long, value_parser = fraction, default_value_t = 0.5)]
    threshold: f64,
    /// Imports in more than this fraction of files (and more than 10) are not rare.
    #[arg(long, value_parser = fraction, default_value_t = 0.005)]
    rare_fraction: f64,
    /// Also report pairs whose two files are in the same module.
    #[arg(long)]
    include_same_module: bool,
    /// How many pairs to show.
    #[arg(long, default_value_t = 40)]
    top: usize,
    /// Emit every pair as JSON (ignores `--top`).
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct TypesArgs {
    #[command(flatten)]
    scan: Scan,
    /// Only compare types with at least this many fields [default: 4].
    #[arg(long)]
    min_fields: Option<usize>,
    /// A pair needs at least this many shared field names [default: 4].
    #[arg(long)]
    min_shared: Option<usize>,
    /// Minimum similarity of field names (Jaccard, 0–1) [default: 0.7].
    #[arg(long, value_parser = fraction)]
    threshold: Option<f64>,
    /// Skip types whose name (or `Scope.Name`) matches this glob (repeatable), in addition to `*Props`.
    #[arg(long = "exclude-name")]
    exclude_names: Vec<String>,
    /// Don't apply the default `*Props` name exclusion.
    #[arg(long)]
    no_default_excludes: bool,
    /// Field names on more than this fraction of types don't seed candidate pairs (they still score) [default: 0.1].
    #[arg(long, value_parser = fraction)]
    common_field_fraction: Option<f64>,
    /// Also report pairs whose two types are in the same module.
    #[arg(long)]
    include_same_module: bool,
    /// Also report pairs acknowledged as deliberate (in `duped.toml` or by a doc comment).
    #[arg(long)]
    include_acknowledged: bool,
    /// List ranked pairs instead of clusters.
    #[arg(long)]
    pairs: bool,
    /// How many clusters (or pairs) to show.
    #[arg(long, default_value_t = 40)]
    top: usize,
    /// Emit every pair and cluster as JSON (ignores `--top` and `--pairs`).
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct Scan {
    /// Directory (or single file) to scan.
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Write to this file instead of stdout.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Include test, mock and fixture files.
    #[arg(long)]
    include_tests: bool,
    /// Skip paths under a glob relative to the scanned directory, e.g. `examples` or `src/gen/*.ts` (repeatable).
    #[arg(long = "exclude")]
    excludes: Vec<String>,
    /// Read settings from this file instead of `<project root>/duped.toml`.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Project root holding `duped.toml` and the module manifests [default: found from the scanned path].
    #[arg(long)]
    root: Option<PathBuf>,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        // `duped extract | head` closes stdout early; that is not a failure.
        Err(err) if is_broken_pipe(&err) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let (Command::Extract(scan)
    | Command::Index(scan)
    | Command::Types(TypesArgs { scan, .. })
    | Command::Bodies(BodiesArgs { scan, .. })
    | Command::Imports(ImportsArgs { scan, .. })) = &cli.command;
    let base = scan_base(&scan.path)?;
    let root = match &scan.root {
        Some(root) => root
            .canonicalize()
            .with_context(|| format!("cannot open --root {}", root.display()))?,
        None => config::project_root(&base),
    };
    let config = config::load(&root, scan.config.as_deref())?;
    let rules = Rules::with_config(&config.rules)?;
    if let Command::Imports(args) = &cli.command {
        return imports(args, &config, rules, &root, &base);
    }
    // Validate options and modules before scanning or truncating `--out`.
    let types = match &cli.command {
        Command::Types(args) => Some(type_options(args, &config, rules.clone())?),
        _ => None,
    };
    let graph = match &cli.command {
        Command::Types(_) | Command::Bodies(_) => {
            Some(modules::ModuleGraph::discover(&root, &config.modules)?)
        }
        _ => None,
    };
    let records = scan_records(scan, &config, rules, &root, &base)?;
    let mut out = output(scan)?;
    match (&cli.command, types, graph) {
        (Command::Extract(_), ..) => {
            for record in &records {
                serde_json::to_writer(&mut out, record)?;
                out.write_all(b"\n")?;
            }
        }
        (Command::Index(_), ..) => out.write_all(index::render(&records).as_bytes())?,
        (Command::Types(args), Some(options), Some(graph)) => {
            let judge = judge::ProjectJudge::new(
                &graph,
                &records,
                base,
                &config.acknowledged,
                "types",
                |r| matches!(r, record::Record::Type(_)),
            );
            let report = analysis::types::find_duplicate_types(&records, &options, &judge);
            if args.json {
                let json = analysis::types::to_json(&records, &report, &judge);
                serde_json::to_writer(&mut out, &json)?;
                out.write_all(b"\n")?;
            } else {
                let text = analysis::types::render_text(
                    &records, &report, args.top, args.pairs, &options, &judge,
                );
                out.write_all(text.as_bytes())?;
            }
        }
        (Command::Bodies(args), _, Some(graph)) => {
            let options = analysis::bodies::BodyOptions {
                shingle: usize::from(args.shingle),
                threshold: args.threshold,
                min_tokens: args.min_tokens,
                include_same_file: args.include_same_file,
                include_acknowledged: args.include_acknowledged,
            };
            let judge = judge::ProjectJudge::new(
                &graph,
                &records,
                base,
                &config.acknowledged,
                "functions",
                |r| analysis::bodies::is_candidate(r, &options),
            );
            let report = analysis::bodies::find_duplicate_bodies(&records, &options, &judge);
            if args.json {
                let json = analysis::bodies::to_json(&records, &report, &judge);
                serde_json::to_writer(&mut out, &json)?;
                out.write_all(b"\n")?;
            } else {
                let text =
                    analysis::bodies::render_text(&records, &report, args.top, &options, &judge);
                out.write_all(text.as_bytes())?;
            }
        }
        _ => unreachable!("options and modules are built for `types` and `bodies`"),
    }
    out.flush()?;
    Ok(())
}

/// `duped imports`: parses only import statements, so it skips record extraction.
fn imports(
    args: &ImportsArgs,
    config: &Config,
    rules: Rules,
    root: &Path,
    base: &Path,
) -> Result<()> {
    let graph = modules::ModuleGraph::discover(root, &config.modules)?;
    let files = walk::discover(
        &args.scan.path,
        &walk_options(&args.scan, config, rules, root, base),
    )?;
    let (imports, skipped) = lang::extract_imports(&files);
    if skipped > 0 {
        eprintln!("duped: skipped {skipped} unreadable files");
    }
    let options = ImportOptions {
        min_shared: args.min_shared,
        threshold: args.threshold,
        rare_fraction: args.rare_fraction,
        include_same_module: args.include_same_module,
    };
    let files = analysis::imports::file_items(imports);
    let judge = judge::FileJudge::new(&graph, base, files.iter().map(|f| f.file.as_str()));
    let report = analysis::imports::find_shared_imports(&files, &options, &judge);
    let mut out = output(&args.scan)?;
    if args.json {
        serde_json::to_writer(
            &mut out,
            &analysis::imports::to_json(&files, &report, &judge),
        )?;
        out.write_all(b"\n")?;
    } else {
        let text = analysis::imports::render_text(&files, &report, args.top, &options, &judge);
        out.write_all(text.as_bytes())?;
    }
    out.flush()?;
    Ok(())
}

/// Flags win over `[types]` in `duped.toml`, which wins over the defaults; name globs add up.
fn type_options(
    args: &TypesArgs,
    config: &Config,
    rules: Rules,
) -> Result<analysis::types::TypeOptions> {
    let types = &config.types;
    let default_excludes = !args.no_default_excludes && types.default_excludes.unwrap_or(true);
    let names: Vec<String> = types
        .exclude_names
        .iter()
        .chain(&args.exclude_names)
        .cloned()
        .collect();
    Ok(analysis::types::TypeOptions {
        min_fields: args.min_fields.or(types.min_fields).unwrap_or(4),
        min_shared: args.min_shared.or(types.min_shared).unwrap_or(4),
        threshold: args.threshold.or(types.threshold).unwrap_or(0.7),
        exclude_names: name_globs(&names, default_excludes)?,
        common_field_fraction: args
            .common_field_fraction
            .or(types.common_field_fraction)
            .unwrap_or(0.1),
        include_same_module: args.include_same_module,
        include_acknowledged: args.include_acknowledged,
        rules,
    })
}

/// Type-name globs, plus `*Props` unless disabled; `*` matches any characters.
fn name_globs(patterns: &[String], defaults: bool) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    let defaults = defaults.then_some("*Props").into_iter();
    for pattern in defaults.chain(patterns.iter().map(String::as_str)) {
        builder.add(
            Glob::new(pattern)
                .with_context(|| format!("invalid --exclude-name glob `{pattern}`"))?,
        );
    }
    Ok(builder.build()?)
}

fn fraction(value: &str) -> Result<f64, String> {
    let parsed: f64 = value
        .parse()
        .map_err(|_| format!("`{value}` is not a number"))?;
    if (0.0..=1.0).contains(&parsed) {
        Ok(parsed)
    } else {
        Err(format!("`{value}` must be between 0 and 1"))
    }
}

/// The directory records' `file` paths are relative to: the scan path, or its parent for a file.
fn scan_base(path: &Path) -> Result<PathBuf> {
    let path = path
        .canonicalize()
        .with_context(|| format!("cannot open {}", path.display()))?;
    Ok(if path.is_file() {
        path.parent()
            .map_or_else(|| path.clone(), Path::to_path_buf)
    } else {
        path
    })
}

fn scan_records(
    scan: &Scan,
    config: &Config,
    rules: Rules,
    root: &Path,
    base: &Path,
) -> Result<Vec<record::Record>> {
    let files = walk::discover(&scan.path, &walk_options(scan, config, rules, root, base))?;
    let extraction = lang::extract_files(&files);
    if extraction.skipped > 0 {
        eprintln!("duped: skipped {} unreadable files", extraction.skipped);
    }
    Ok(extraction.records)
}

fn walk_options(
    scan: &Scan,
    config: &Config,
    rules: Rules,
    root: &Path,
    base: &Path,
) -> walk::WalkOptions {
    // `[scan] exclude` is relative to the project root, `--exclude` to the scanned path.
    // A scan outside the root (via `--root`) has no root-relative paths to match.
    let prefix = base
        .starts_with(root)
        .then(|| modules::relative(root, base));
    walk::WalkOptions {
        include_tests: scan.include_tests || config.scan.include_tests,
        excludes: scan.excludes.clone(),
        root_excludes: config.scan.exclude.clone(),
        root_prefix: prefix,
        rules,
    }
}

fn output(scan: &Scan) -> Result<Box<dyn Write>> {
    Ok(match &scan.out {
        Some(path) => Box::new(BufWriter::new(
            File::create(path).with_context(|| format!("cannot write {}", path.display()))?,
        )),
        None => Box::new(BufWriter::new(io::stdout().lock())),
    })
}

fn is_broken_pipe(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        let kind = cause
            .downcast_ref::<io::Error>()
            .map(io::Error::kind)
            .or_else(|| {
                cause
                    .downcast_ref::<serde_json::Error>()
                    .and_then(serde_json::Error::io_error_kind)
            });
        kind == Some(io::ErrorKind::BrokenPipe)
    })
}
