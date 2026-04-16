//! APFS space-saving clone operations.
//!
//! For each duplicate group we:
//!   1. Keep files[0] as the "source" (untouched).
//!   2. For every other file ("target"), create a temp APFS clone of the source.
//!   3. Restore the target's metadata (permissions, ownership, timestamps, xattrs)
//!      onto the clone.
//!   4. Atomically rename the clone over the target.
//!
//! The target file's *data* is now a copy-on-write clone that shares blocks with
//! the source, but it retains its own name, permissions, timestamps, and xattrs.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use crossbeam_channel::Sender;
use filetime::FileTime;

use crate::types::{FileGroup, ReclaimMessage};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn reclaim_groups(groups: &[FileGroup], dry_run: bool, tx: Sender<ReclaimMessage>) {
    let selected: Vec<&FileGroup> = groups.iter().filter(|g| g.selected).collect();
    let total = selected.len() as u64;
    let mut done = 0u64;
    let mut total_reclaimed = 0u64;

    for group in &selected {
        let source = &group.files[0].path;

        for target_info in &group.files[1..] {
            let target = &target_info.path;

            if dry_run {
                total_reclaimed += group.size;
            } else {
                match replace_with_clone(source, target) {
                    Ok(saved) => total_reclaimed += saved,
                    Err(e) => {
                        tx.send(ReclaimMessage::Error(format!(
                            "Failed {}: {}",
                            target.display(),
                            e
                        )))
                        .ok();
                    }
                }
            }
        }

        done += 1;
        tx.send(ReclaimMessage::Progress {
            done,
            total,
            reclaimed: total_reclaimed,
        })
        .ok();
    }

    tx.send(ReclaimMessage::Done { total_reclaimed }).ok();
}

// ---------------------------------------------------------------------------
// Core clone logic
// ---------------------------------------------------------------------------

fn replace_with_clone(source: &Path, target: &Path) -> Result<u64> {
    let src_meta = fs::metadata(source)
        .with_context(|| format!("Cannot stat source: {}", source.display()))?;
    let tgt_meta = fs::metadata(target)
        .with_context(|| format!("Cannot stat target: {}", target.display()))?;

    if src_meta.dev() != tgt_meta.dev() {
        bail!(
            "source and target are on different volumes (APFS clones must be on the same volume)"
        );
    }

    let file_size = tgt_meta.len();
    let temp = temp_path(target);

    // Create APFS clone of source at temp (destination must not exist)
    reflink::reflink(source, &temp)
        .map_err(|e| anyhow::anyhow!("{}", e))
        .with_context(|| {
            format!(
                "APFS clonefile failed: {} -> {}",
                source.display(),
                temp.display()
            )
        })?;

    // Restore target metadata onto clone, then atomically replace target
    if let Err(e) = apply_metadata_and_rename(&temp, target, &tgt_meta) {
        let _ = fs::remove_file(&temp); // best-effort cleanup
        return Err(e);
    }

    Ok(file_size)
}

fn apply_metadata_and_rename(temp: &Path, target: &Path, tgt_meta: &fs::Metadata) -> Result<()> {
    // 1. Replace xattrs.
    //    `clonefile` copies the source's xattrs (including `com.apple.ResourceFork`
    //    if the source has a resource fork) onto temp.  We swap all of those for
    //    the target's xattrs so the clone carries the target's resource fork and
    //    any other extended metadata.
    swap_xattrs(temp, target)?;

    // 2. Permissions
    fs::set_permissions(temp, tgt_meta.permissions()).context("set permissions")?;

    // 3. Ownership
    chown(temp, tgt_meta.uid(), tgt_meta.gid()).context("set ownership")?;

    // 4. Timestamps
    let atime = FileTime::from_last_access_time(tgt_meta);
    let mtime = FileTime::from_last_modification_time(tgt_meta);
    filetime::set_file_times(temp, atime, mtime).context("set timestamps")?;

    // 5. Atomic rename: temp → target
    fs::rename(temp, target)
        .with_context(|| format!("rename {} -> {}", temp.display(), target.display()))?;

    // 6. BSD file flags (UF_HIDDEN, UF_NODUMP, UF_COMPRESSED, …)
    //    Set after rename so that immutable flags don't block the rename itself.
    //    We skip UF_IMMUTABLE / SF_IMMUTABLE — those are checked & excluded during
    //    scanning, so we should never reach this point with a locked target.
    let flags = bsd_flags(target);
    if flags != 0 {
        // Best-effort: ignore failure (e.g. permission denied for SF_* flags).
        let _ = set_flags(target, flags);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn temp_path(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut p = target.to_path_buf();
    p.set_file_name(format!("{}.mdd_tmp_{}", name, std::process::id()));
    p
}

fn chown(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let cpath =
        CString::new(path.as_os_str().as_bytes()).map_err(|_| invalid_input("null byte in path"))?;
    let ret = unsafe { libc::chown(cpath.as_ptr(), uid, gid) };
    if ret == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn swap_xattrs(temp: &Path, target: &Path) -> Result<()> {
    // Collect target's xattrs first
    let wanted: Vec<(std::ffi::OsString, Vec<u8>)> = xattr::list(target)
        .unwrap_or_default()
        .filter_map(|name| {
            let val = xattr::get(target, &name).ok()??;
            Some((name, val))
        })
        .collect();

    // Remove whatever clonefile copied from source
    if let Ok(names) = xattr::list(temp) {
        for name in names {
            let _ = xattr::remove(temp, &name);
        }
    }

    // Apply target's xattrs
    for (name, val) in &wanted {
        let _ = xattr::set(temp, name, val);
    }

    Ok(())
}

/// Read BSD st_flags without following symlinks (lstat).
fn bsd_flags(path: &Path) -> u32 {
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

fn set_flags(path: &Path, flags: u32) -> io::Result<()> {
    let cpath =
        CString::new(path.as_os_str().as_bytes()).map_err(|_| invalid_input("null byte in path"))?;
    let ret = unsafe { libc::chflags(cpath.as_ptr(), flags as libc::c_uint) };
    if ret == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn invalid_input(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}
