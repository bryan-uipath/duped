mod analysis;
mod index;
mod lang;
mod record;
mod walk;

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use globset::{Glob, GlobSet, GlobSetBuilder};

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
}

#[derive(Args)]
struct TypesArgs {
    #[command(flatten)]
    scan: Scan,
    /// Only compare types with at least this many fields.
    #[arg(long, default_value_t = 4)]
    min_fields: usize,
    /// A pair needs at least this many shared field names.
    #[arg(long, default_value_t = 4)]
    min_shared: usize,
    /// Minimum similarity of field names (Jaccard, 0–1).
    #[arg(long, default_value_t = 0.7, value_parser = fraction)]
    threshold: f64,
    /// Skip types whose name (or `Scope.Name`) matches this glob (repeatable), in addition to `*Props`.
    #[arg(long = "exclude-name")]
    exclude_names: Vec<String>,
    /// Don't apply the default `*Props` name exclusion.
    #[arg(long)]
    no_default_excludes: bool,
    /// Field names on more than this fraction of types don't seed candidate pairs (they still score).
    #[arg(long, default_value_t = 0.1, value_parser = fraction)]
    common_field_fraction: f64,
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
    /// Skip paths under a root-relative glob, e.g. `examples` or `src/gen/*.ts` (repeatable).
    #[arg(long = "exclude")]
    excludes: Vec<String>,
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
    let (Command::Extract(scan) | Command::Index(scan) | Command::Types(TypesArgs { scan, .. })) =
        &cli.command;
    // Validate options before scanning or truncating `--out`.
    let type_options = match &cli.command {
        Command::Types(args) => Some(analysis::types::TypeOptions {
            min_fields: args.min_fields,
            min_shared: args.min_shared,
            threshold: args.threshold,
            exclude_names: name_globs(&args.exclude_names, !args.no_default_excludes)?,
            common_field_fraction: args.common_field_fraction,
        }),
        _ => None,
    };
    let records = scan_records(scan)?;
    let mut out = output(scan)?;
    match (&cli.command, type_options) {
        (Command::Extract(_), _) => {
            for record in &records {
                serde_json::to_writer(&mut out, record)?;
                out.write_all(b"\n")?;
            }
        }
        (Command::Index(_), _) => out.write_all(index::render(&records).as_bytes())?,
        (Command::Types(args), Some(options)) => {
            let report = analysis::types::find_duplicate_types(&records, &options);
            if args.json {
                serde_json::to_writer(&mut out, &analysis::types::to_json(&records, &report))?;
                out.write_all(b"\n")?;
            } else {
                let text =
                    analysis::types::render_text(&records, &report, args.top, args.pairs, &options);
                out.write_all(text.as_bytes())?;
            }
        }
        (Command::Types(_), None) => unreachable!("type options are built for `types`"),
    }
    out.flush()?;
    Ok(())
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

fn scan_records(scan: &Scan) -> Result<Vec<record::Record>> {
    let options = walk::WalkOptions {
        include_tests: scan.include_tests,
        excludes: scan.excludes.clone(),
    };
    let files = walk::discover(&scan.path, &options)?;
    let extraction = lang::extract_files(&files);
    if extraction.skipped > 0 {
        eprintln!("duped: skipped {} unreadable files", extraction.skipped);
    }
    Ok(extraction.records)
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
