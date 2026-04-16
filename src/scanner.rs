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
    // -----------------------------------------------------------------------
    // Phase 1: Walk all directories and collect FileInfo
    // -----------------------------------------------------------------------
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
    tx.send(ScanMessage::Progress { done: all_files.len() as u64, total: all_files.len() as u64 }).ok();

    // -----------------------------------------------------------------------
    // Phase 2: Group by size — only same-size files can be duplicates
    // -----------------------------------------------------------------------
    tx.send(ScanMessage::Phase(ScanPhase::Grouping)).ok();

    let mut by_size: HashMap<u64, Vec<FileInfo>> = HashMap::new();
    for file in all_files {
        by_size.entry(file.size).or_default().push(file);
    }

    // Keep groups with 2+ files, where they are not all the same inode
    let size_groups: Vec<Vec<FileInfo>> = by_size
        .into_values()
        .filter(|g| g.len() > 1 && !all_same_inode(g))
        .collect();

    if size_groups.is_empty() {
        tx.send(ScanMessage::Done(vec![])).ok();
        return Ok(());
    }

    // -----------------------------------------------------------------------
    // Phase 3: Prefix hash — read first 4 KB of each candidate
    // -----------------------------------------------------------------------
    tx.send(ScanMessage::Phase(ScanPhase::PrefixHashing)).ok();

    let prefix_total: u64 = size_groups.iter().map(|g| g.len() as u64).sum();
    let prefix_done = Arc::new(AtomicU64::new(0));
    let tx_p = tx.clone();
    let pd = prefix_done.clone();

    // Process each size-group in parallel; within a group, hash files sequentially.
    // flat_map splits one group into potentially many sub-groups by prefix hash.
    let prefix_groups: Vec<Vec<FileInfo>> = size_groups
        .into_par_iter()
        .flat_map(|group| {
            let mut by_hash: HashMap<[u8; 32], Vec<FileInfo>> = HashMap::new();
            for file in group {
                if let Some(h) = hash_prefix(&file.path) {
                    by_hash.entry(h).or_default().push(file);
                }
                let done = pd.fetch_add(1, Ordering::Relaxed) + 1;
                if done % 100 == 0 {
                    tx_p.send(ScanMessage::Progress { done, total: prefix_total }).ok();
                }
            }
            by_hash
                .into_values()
                .filter(|g| g.len() > 1 && !all_same_inode(g))
                .collect::<Vec<_>>()
        })
        .collect();

    if prefix_groups.is_empty() {
        tx.send(ScanMessage::Done(vec![])).ok();
        return Ok(());
    }

    // -----------------------------------------------------------------------
    // Phase 4: Full content hash — the definitive duplicate check
    // -----------------------------------------------------------------------
    tx.send(ScanMessage::Phase(ScanPhase::FullHashing)).ok();

    let full_total: u64 = prefix_groups.iter().map(|g| g.len() as u64).sum();
    let full_done = Arc::new(AtomicU64::new(0));
    let tx_f = tx.clone();
    let fd = full_done.clone();

    let mut groups: Vec<FileGroup> = prefix_groups
        .into_par_iter()
        .flat_map(|group| {
            let mut by_hash: HashMap<[u8; 32], Vec<FileInfo>> = HashMap::new();
            for file in group {
                if let Some(h) = hash_full(&file.path) {
                    by_hash.entry(h).or_default().push(file);
                }
                let done = fd.fetch_add(1, Ordering::Relaxed) + 1;
                if done % 20 == 0 {
                    tx_f.send(ScanMessage::Progress { done, total: full_total }).ok();
                }
            }
            by_hash
                .into_values()
                .filter(|g| g.len() > 1 && !all_same_inode(g))
                .map(|mut files| {
                    files.sort_by(|a, b| a.path.cmp(&b.path));
                    let size = files[0].size;
                    FileGroup { size, files, selected: true }
                })
                .collect::<Vec<_>>()
        })
        .collect();

    // Sort by descending savings so the biggest wins appear first.
    groups.sort_by(|a, b| b.savings().cmp(&a.savings()));

    tx.send(ScanMessage::Done(groups)).ok();
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Read BSD file flags (st_flags) from the OS without following symlinks.
/// Returns 0 on any error (conservative — don't skip on stat failure).
fn bsd_flags(path: &Path) -> u32 {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let Ok(cpath) = CString::new(path.as_os_str().as_bytes()) else {
        return 0;
    };
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // lstat so we don't follow symlinks
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
            Ok(n) => { h.update(&buf[..n]); }
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

    // Symlinks are already filtered by the `is_file()` check later;
    // we do not need to exclude them here.

    // Directories that macOS presents as single files (packages/bundles).
    // Recursing into them would surface internal implementation files.
    if entry.file_type().is_dir() && is_package_dir(name) {
        return true;
    }

    // Well-known dangerous top-level system directories.
    if path.parent().map_or(false, |p| p == Path::new("/")) {
        if matches!(name, "System" | "dev" | "private" | "cores" | "proc") {
            return true;
        }
    }

    // ~/Library and /Library: contain caches, iCloud stubs, and system data.
    // Scanning them risks triggering cloud downloads and touching system state.
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
    // ~/Library
    if let Ok(home) = std::env::var("HOME") {
        if path == Path::new(&home).join("Library") {
            return true;
        }
    }
    // /Library at the volume root
    if path.parent() == Some(Path::new("/"))
        && path.file_name().map_or(false, |n| n == "Library")
    {
        return true;
    }
    false
}
