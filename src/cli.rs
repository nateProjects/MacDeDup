//! Plain-text CLI mode (`--no-tui`).

use std::io::{self, Write};
use std::path::PathBuf;
use std::thread;

use bytesize::ByteSize;
use crossbeam_channel::Receiver;

use crate::types::{FileGroup, ReclaimMessage, ScanMessage, ScanPhase};
use crate::{clone, scanner};

// Move to column 0 and erase to end of line.
const CLR: &str = "\r\x1b[K";

pub fn run(
    paths: Vec<PathBuf>,
    dry_run: bool,
    min_size: u64,
    scan_library: bool,
) -> anyhow::Result<()> {
    let (scan_tx, scan_rx) = crossbeam_channel::unbounded::<ScanMessage>();
    {
        let paths = paths.clone();
        thread::spawn(move || scanner::scan(paths, min_size, scan_library, scan_tx));
    }

    let groups = run_scan(scan_rx)?;

    if groups.is_empty() {
        out("No duplicate files found.");
        return Ok(());
    }

    let total_savings: u64 = groups.iter().map(|g| g.savings()).sum();
    out(&format!(
        "Found {} duplicate group(s) — potential savings: {}",
        groups.len(),
        ByteSize(total_savings)
    ));
    out("");

    for (i, g) in groups.iter().take(10).enumerate() {
        let name = g.files[0]
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        out(&format!(
            "  {:3}. {} ({} copies, saves {})",
            i + 1,
            name,
            g.files.len(),
            ByteSize(g.savings()),
        ));
    }
    if groups.len() > 10 {
        out(&format!("       … and {} more groups", groups.len() - 10));
    }
    out("");

    if dry_run {
        out("[DRY RUN] No files were modified.");
        return Ok(());
    }

    // Confirm
    print!("Proceed with reclamation? [y/N] ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    if !matches!(input.trim().to_lowercase().as_str(), "y" | "yes") {
        out("Aborted.");
        return Ok(());
    }

    let (reclaim_tx, reclaim_rx) = crossbeam_channel::unbounded::<ReclaimMessage>();
    {
        let groups = groups.clone();
        thread::spawn(move || clone::reclaim_groups(&groups, false, reclaim_tx));
    }

    run_reclaim(reclaim_rx)
}

// ---------------------------------------------------------------------------
// `out` — guaranteed column-0 output.
//
// After in-place progress (which uses \r without \n), the cursor may be
// sitting at a non-zero column on the "current" line. A plain \n only moves
// to the next row without resetting the column. Using \r\n guarantees the
// cursor lands at column 0 of the new row regardless of terminal settings.
// ---------------------------------------------------------------------------
fn out(line: &str) {
    print!("\r{line}\n");
    let _ = io::stdout().flush();
}

// ---------------------------------------------------------------------------

const BAR: usize = 30;

fn progress_bar(pct: u64) -> String {
    let filled = (pct as usize * BAR / 100).min(BAR);
    format!("[{}{}]", "█".repeat(filled), "░".repeat(BAR - filled))
}

fn run_scan(rx: Receiver<ScanMessage>) -> anyhow::Result<Vec<FileGroup>> {
    let mut phase = ScanPhase::Walking;
    let mut files_found = 0u64;

    loop {
        match rx.recv()? {
            ScanMessage::Phase(p) => {
                match &p {
                    ScanPhase::Walking => {}

                    ScanPhase::Grouping => {
                        // Close the walking line with \r\n so cursor lands at col 0.
                        print!("{CLR}Scanning:  {} files found.\r\n", fmt_n(files_found));
                        io::stdout().flush()?;
                    }

                    ScanPhase::PrefixHashing => {
                        // Grouping is instant — no output for it.
                        print!("Hashing:   quick pass  {}", progress_bar(0));
                        io::stdout().flush()?;
                    }

                    ScanPhase::FullHashing => {
                        print!("{CLR}Hashing:   quick pass  {} done.\r\n", progress_bar(100));
                        print!("Hashing:   full pass   {}", progress_bar(0));
                        io::stdout().flush()?;
                    }
                }
                phase = p;
            }

            ScanMessage::Progress { done, total } => match &phase {
                ScanPhase::Walking => {
                    files_found = done;
                    print!("{CLR}Scanning:  {} files…", fmt_n(done));
                    io::stdout().flush()?;
                }
                ScanPhase::Grouping => {}
                ScanPhase::PrefixHashing => {
                    if total > 0 {
                        let pct = (done * 100 / total).min(100);
                        print!("{CLR}Hashing:   quick pass  {}", progress_bar(pct));
                        io::stdout().flush()?;
                    }
                }
                ScanPhase::FullHashing => {
                    if total > 0 {
                        let pct = (done * 100 / total).min(100);
                        print!("{CLR}Hashing:   full pass   {}", progress_bar(pct));
                        io::stdout().flush()?;
                    }
                }
            },

            ScanMessage::Done(groups) => {
                // End whatever phase we were in, then blank line.
                match &phase {
                    ScanPhase::Walking | ScanPhase::Grouping => {
                        print!("{CLR}Scanning:  {} files found.\r\n", fmt_n(files_found));
                    }
                    ScanPhase::PrefixHashing => {
                        print!("{CLR}Hashing:   quick pass  {} done.\r\n", progress_bar(100));
                    }
                    ScanPhase::FullHashing => {
                        print!("{CLR}Hashing:   full pass   {} done.\r\n", progress_bar(100));
                    }
                }
                // Blank line before results — \r\n ensures col 0.
                print!("\r\n");
                io::stdout().flush()?;
                return Ok(groups);
            }

            ScanMessage::Error(e) => {
                print!("{CLR}\r\n");
                io::stdout().flush()?;
                return Err(anyhow::anyhow!(e));
            }
        }
    }
}

fn run_reclaim(rx: Receiver<ReclaimMessage>) -> anyhow::Result<()> {
    let mut errors: Vec<String> = Vec::new();

    loop {
        match rx.recv()? {
            ReclaimMessage::Progress { done, total, reclaimed } => {
                if total > 0 {
                    let pct = (done * 100 / total).min(100);
                    print!(
                        "{CLR}Reclaiming: {} {} — {}/{}",
                        progress_bar(pct),
                        ByteSize(reclaimed),
                        fmt_n(done),
                        fmt_n(total),
                    );
                    io::stdout().flush()?;
                }
            }
            ReclaimMessage::Done { total_reclaimed } => {
                print!("{CLR}Reclaiming: {} done.\r\n", progress_bar(100));
                out(&format!("✓  Reclaimed {}.", ByteSize(total_reclaimed)));
                if !errors.is_empty() {
                    out("");
                    out(&format!("{} error(s):", errors.len()));
                    for e in &errors {
                        out(&format!("   • {e}"));
                    }
                }
                return Ok(());
            }
            ReclaimMessage::Error(e) => {
                errors.push(e.clone());
                print!("{CLR}  ! {e}\r\n");
                io::stdout().flush()?;
            }
        }
    }
}

// ---------------------------------------------------------------------------

fn fmt_n(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}
