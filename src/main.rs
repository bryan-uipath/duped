use clap::Parser;

/// Find duplicated functions and types across a codebase.
#[derive(Parser)]
#[command(version, about)]
struct Cli {}

fn main() {
    Cli::parse();
}
