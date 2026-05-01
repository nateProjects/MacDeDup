# MacDeDup — MacOS Space Saver

MacDeDup is a TUI / command-line app that reclaims disk space used by duplicate files — without removing any files.

It works by replacing duplicate files with APFS space-saving clones. Each clone looks and behaves exactly like the original (same name, permissions, timestamps, metadata), but all copies share a single copy of the data on disk. Changes to one file never affect the others.

Inspired by [fclones](https://github.com/pkolaczk/fclones) and [Hyperspace](https://apps.apple.com/app/hyperspace/id1537774279).

Note: Claude Code was used to help convert this from Python to Rust for more speedy scanning.

---

## Usage

```
MacDeDup <PATH> [PATH ...]          # interactive TUI (default)
MacDeDup --no-tui <PATH> [PATH ...]  # plain text output
MacDeDup --dry-run <PATH>            # preview savings without modifying anything
MacDeDup --scan-library <PATH>       # include ~/Library in the scan (see below)
MacDeDup --min-size <BYTES> <PATH>   # skip files smaller than N bytes (default: 1)
```

### TUI screens

1. **Scanning** — live progress through each detection phase
2. **Review** — browse duplicate groups, toggle which to reclaim (`Space`), then press `R`
3. **Reclaiming** — progress and live bytes-saved counter
4. **Done** — total space reclaimed

Key bindings in the review screen: `↑`/`↓`/`j`/`k` navigate, `Space` toggle, `A` select/deselect all, `R`/`Enter` reclaim, `Q` quit.

---

## How detection works

Files are compared in three progressively more expensive stages:

1. **Group by size** — only files of identical size can be duplicates
2. **Prefix hash** — blake3 of the first 4 KB; eliminates most non-duplicates cheaply
3. **Full content hash** — blake3 of the entire file; the definitive check

Stages 2 and 3 run in parallel across candidate groups via rayon.

---

## What gets skipped

The following are never scanned:

| Excluded | Reason |
|---|---|
| Symlinks | Not regular files |
| macOS package bundles (`.app`, `.framework`, `.photoslibrary`, `.pages`, `.xcodeproj`, etc.) | Directories Finder presents as single files — their internals should not be deduped independently |
| Immutable / locked files (`UF_IMMUTABLE`, `SF_IMMUTABLE`) | Cannot be safely replaced; same behaviour as Hyperspace |
| `~/Library` and `/Library` (by default) | Contains iCloud stubs, caches, and system state — reading stubs can trigger unwanted cloud downloads |
| Top-level system directories (`/System`, `/dev`, `/private`, `/cores`) | Read-only or unsafe to touch |

Pass `--scan-library` to include `~/Library` if you know what you are doing.

---

## What gets preserved on cloned files

When a target file is replaced with a clone of the source, the following metadata from the **target** is restored onto the clone:

- Permissions (mode bits)
- Ownership (uid / gid)
- Timestamps (atime, mtime)
- Extended attributes — including `com.apple.ResourceFork`, so resource forks are preserved
- BSD file flags (UF_HIDDEN, UF_NODUMP, UF_COMPRESSED, …)

The source file is never modified.

---

## Requirements

- macOS with an APFS volume (APFS clones do not work on HFS+ or external FAT/exFAT drives)

## Installation

**Homebrew (recommended):**

```
brew tap nateProjects/macdedup https://github.com/nateProjects/MacDeDup
brew install macdedup
```

**Build from source** (requires Rust 1.74+):

```
cargo build --release
cp target/release/MacDeDup /usr/local/bin/
```

**Or run from the project root** (re-run the `ln` after each rebuild):

```
ln -sf target/release/MacDeDup MacDeDup
./MacDeDup <PATH>
```

---

## Source Code

- [main.rs](src/main.rs) — entry point; parses CLI arguments and routes to TUI or plain-text mode
- [scanner.rs](src/scanner.rs) — walks directories, groups files by size, then narrows duplicates via prefix hash and full blake3 hash using rayon for parallelism
- [clone.rs](src/clone.rs) — replaces duplicate targets with APFS clones of the source, restoring all target metadata (permissions, ownership, timestamps, xattrs, BSD flags)
- [tui.rs](src/tui.rs) — interactive ratatui TUI: Scanning → Review → Reclaiming → Done state machine
- [cli.rs](src/cli.rs) — plain-text `--no-tui` mode with in-place progress bars
- [types.rs](src/types.rs) — shared types (`FileInfo`, `FileGroup`, `ScanMessage`, `ReclaimMessage`)
