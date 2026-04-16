mod cli;
mod clone;
mod scanner;
mod tui;
mod types;

use std::path::PathBuf;

use clap::Parser;

/// Reclaim disk space on macOS by replacing duplicate files with
/// APFS space-saving clones — no files are removed.
#[derive(Parser)]
#[command(name = "MacDeDup", version)]
struct Args {
    /// One or more directories to scan
    #[arg(required = true)]
    paths: Vec<PathBuf>,

    /// Plain text output — no interactive TUI
    #[arg(long)]
    no_tui: bool,

    /// Show what would be done without modifying any files
    #[arg(long)]
    dry_run: bool,

    /// Ignore files smaller than this many bytes (default: 1)
    #[arg(long, default_value_t = 1)]
    min_size: u64,

    /// Include ~/Library in the scan (may trigger iCloud file downloads)
    #[arg(long)]
    scan_library: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    if args.no_tui {
        cli::run(args.paths, args.dry_run, args.min_size, args.scan_library)
    } else {
        tui::run(args.paths, args.dry_run, args.min_size, args.scan_library)
    }
}
