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
    let (Command::Extract(scan) | Command::Index(scan)) = &cli.command;
    let records = scan_records(scan)?;
    let mut out = output(scan)?;
    match cli.command {
        Command::Extract(_) => {
            for record in &records {
                serde_json::to_writer(&mut out, record)?;
                out.write_all(b"\n")?;
            }
        }
        Command::Index(_) => out.write_all(index::render(&records).as_bytes())?,
    }
    out.flush()?;
    Ok(())
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
