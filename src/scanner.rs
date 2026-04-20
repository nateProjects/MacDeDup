use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use blake3::Hasher;
use crossbeam_channel::Sender;
use rayon::prelude::*;
use walkdir::WalkDir;

use crate::types::{FileGroup, FileInfo, ScanMessage, ScanPhase};

const PREFIX_LEN: usize = 4_096; // 4 KB quick-hash
const READ_BUF: usize = 65_536;  // 64 KB read buffer for full hash

pub fn scan(paths: Vec<PathBuf>, min_size: u64, scan_library: bool, tx: Sender<ScanMessage>) {
    if let Err(e) = scan_inner(paths, min_size, scan_library, &tx) {
        tx.send(ScanMessage::Error(e.to_string())).ok();
    }
}

fn scan_inner(
    paths: Vec<PathBuf>,
    min_size: u64,
    scan_library: bool,
    tx: &Sender<ScanMessage>,
) -> anyhow::Result<()> {
    // ── Phase 1: Walk ─────────────────────────────────────────────────────────
    tx.send(ScanMessage::Phase(ScanPhase::Walking)).ok();

    let mut all_files: Vec<FileInfo> = Vec::new();
    let mut walked = 0u64;

    for root in &paths {
        for entry in WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| !is_excluded(e, scan_library))
        {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            if !entry.file_type().is_file() {
                continue;
            }

            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };

            let size = meta.len();
            if size < min_size {
                continue;
            }

            // Skip locked / immutable files — clonefile can't safely replace them.
            // UF_IMMUTABLE = 0x0002, SF_IMMUTABLE = 0x00020000
            let flags = bsd_flags(entry.path());
            if flags & 0x0002 != 0 || flags & 0x0002_0000 != 0 {
                continue;
            }

            all_files.push(FileInfo {
                path: entry.path().to_path_buf(),
                size,
                inode: meta.ino(),
                device: meta.dev(),
            });

            walked += 1;
            if walked % 200 == 0 {
                tx.send(ScanMessage::Progress { done: walked, total: 0 }).ok();
            }
        }
    }
    tx.send(ScanMessage::Progress {
        done: all_files.len() as u64,
        total: all_files.len() as u64,
    })
    .ok();

    // ── Phase 2: Group by size ────────────────────────────────────────────────
    tx.send(ScanMessage::Phase(ScanPhase::Grouping)).ok();

    let mut by_size: HashMap<u64, Vec<FileInfo>> = HashMap::new();
    for file in all_files {
        by_size.entry(file.size).or_default().push(file);
    }

    // Flatten to one list of candidates (files that share a size with ≥1 other).
    // Flattening lets rayon schedule every individual file independently in
    // phases 3 and 4, rather than one thread per size-group.
    let candidates: Vec<FileInfo> = by_size
        .into_values()
        .filter(|g| g.len() > 1 && !all_same_inode(g))
        .flatten()
        .collect();

    if candidates.is_empty() {
        tx.send(ScanMessage::Done(vec![])).ok();
        return Ok(());
    }

    // ── Phase 3: Prefix hash — all candidates fully in parallel ──────────────
    tx.send(ScanMessage::Phase(ScanPhase::PrefixHashing)).ok();

    let prefix_total = candidates.len() as u64;
    let prefix_done = Arc::new(AtomicU64::new(0));
    let tx_p = tx.clone();
    let pd = prefix_done.clone();

    let prefix_hashed: Vec<(FileInfo, [u8; 32])> = candidates
        .into_par_iter()
        .filter_map(|file| {
            let h = hash_prefix(&file.path)?;
            let done = pd.fetch_add(1, Ordering::Relaxed) + 1;
            if done % 100 == 0 {
                tx_p.send(ScanMessage::Progress { done, total: prefix_total }).ok();
            }
            Some((file, h))
        })
        .collect();

    // Regroup by (size, prefix_hash).  Size is kept in the key so files from
    // different size-groups can never collide even on an accidental hash match.
    let mut by_prefix: HashMap<(u64, [u8; 32]), Vec<FileInfo>> = HashMap::new();
    for (file, hash) in prefix_hashed {
        by_prefix.entry((file.size, hash)).or_default().push(file);
    }

    let prefix_candidates: Vec<FileInfo> = by_prefix
        .into_values()
        .filter(|g| g.len() > 1 && !all_same_inode(g))
        .flatten()
        .collect();

    if prefix_candidates.is_empty() {
        tx.send(ScanMessage::Done(vec![])).ok();
        return Ok(());
    }

    // ── Phase 4: Full hash — remaining candidates fully in parallel ───────────
    tx.send(ScanMessage::Phase(ScanPhase::FullHashing)).ok();

    let full_total = prefix_candidates.len() as u64;
    let full_done = Arc::new(AtomicU64::new(0));
    let tx_f = tx.clone();
    let fd = full_done.clone();

    let full_hashed: Vec<(FileInfo, [u8; 32])> = prefix_candidates
        .into_par_iter()
        .filter_map(|file| {
            let h = hash_full(&file.path)?;
            let done = fd.fetch_add(1, Ordering::Relaxed) + 1;
            if done % 20 == 0 {
                tx_f.send(ScanMessage::Progress { done, total: full_total }).ok();
            }
            Some((file, h))
        })
        .collect();

    // Regroup by (size, full_hash) — these are the definitive duplicate sets.
    let mut by_full: HashMap<(u64, [u8; 32]), Vec<FileInfo>> = HashMap::new();
    for (file, hash) in full_hashed {
        by_full.entry((file.size, hash)).or_default().push(file);
    }

    let mut groups: Vec<FileGroup> = by_full
        .into_values()
        .filter(|g| g.len() > 1 && !all_same_inode(g))
        .map(|mut files| {
            files.sort_by(|a, b| a.path.cmp(&b.path));
            let size = files[0].size;
            FileGroup { size, files, selected: true }
        })
        .collect();

    groups.sort_by(|a, b| b.savings().cmp(&a.savings()));

    tx.send(ScanMessage::Done(groups)).ok();
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Read BSD file flags (st_flags) via lstat — returns 0 on any error.
fn bsd_flags(path: &Path) -> u32 {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let Ok(cpath) = CString::new(path.as_os_str().as_bytes()) else {
        return 0;
    };
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::lstat(cpath.as_ptr(), &mut st) } == 0 {
        st.st_flags
    } else {
        0
    }
}

fn all_same_inode(files: &[FileInfo]) -> bool {
    let first = (files[0].inode, files[0].device);
    files.iter().all(|f| (f.inode, f.device) == first)
}

fn hash_prefix(path: &Path) -> Option<[u8; 32]> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut buf = [0u8; PREFIX_LEN];
    let n = reader.read(&mut buf).ok()?;
    let mut h = Hasher::new();
    h.update(&buf[..n]);
    Some(*h.finalize().as_bytes())
}

fn hash_full(path: &Path) -> Option<[u8; 32]> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut h = Hasher::new();
    let mut buf = vec![0u8; READ_BUF];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                h.update(&buf[..n]);
            }
            Err(_) => return None,
        }
    }
    Some(*h.finalize().as_bytes())
}

/// Returns true if walkdir should not descend into (or yield) this entry.
fn is_excluded(entry: &walkdir::DirEntry, scan_library: bool) -> bool {
    let path = entry.path();

    let name = match path.file_name() {
        Some(n) => n.to_string_lossy(),
        None => return false,
    };
    let name = name.as_ref();

    if entry.file_type().is_dir() && is_package_dir(name) {
        return true;
    }

    if path.parent().map_or(false, |p| p == Path::new("/")) {
        if matches!(name, "System" | "dev" | "private" | "cores" | "proc") {
            return true;
        }
    }

    if !scan_library && is_library_dir(path) {
        return true;
    }

    false
}

/// macOS package bundle extensions — directories Finder displays as single files.
fn is_package_dir(name: &str) -> bool {
    const PKGS: &[&str] = &[
        ".app",
        ".bundle",
        ".framework",
        ".plugin",
        ".kext",
        ".photoslibrary",
        ".imovielibrary",
        ".aplibrary",
        ".rtfd",
        ".xcodeproj",
        ".xcworkspace",
        ".playground",
        ".pages",
        ".numbers",
        ".key",
        ".sparsebundle",
    ];
    PKGS.iter().any(|ext| name.ends_with(ext))
}

/// Returns true if `path` is a Library directory that should be skipped by default.
fn is_library_dir(path: &Path) -> bool {
    if let Ok(home) = std::env::var("HOME") {
        if path == Path::new(&home).join("Library") {
            return true;
        }
    }
    if path.parent() == Some(Path::new("/"))
        && path.file_name().map_or(false, |n| n == "Library")
    {
        return true;
    }
    false
}
