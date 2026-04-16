use std::path::PathBuf;

/// Information about a single file on disk.
#[derive(Debug, Clone)]
pub struct FileInfo {
    pub path: PathBuf,
    pub size: u64,
    pub inode: u64,
    pub device: u64,
}

/// A group of files that have identical content.
#[derive(Debug, Clone)]
pub struct FileGroup {
    pub size: u64,
    /// files[0] is treated as the "source"; the rest are "targets".
    pub files: Vec<FileInfo>,
    /// Whether this group is selected for reclamation.
    pub selected: bool,
}

impl FileGroup {
    /// Bytes saved by converting all targets to clones of the source.
    pub fn savings(&self) -> u64 {
        self.size.saturating_mul(self.files.len().saturating_sub(1) as u64)
    }
}

// ---------------------------------------------------------------------------
// Scanner → UI message types
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ScanMessage {
    Phase(ScanPhase),
    /// (done, total) — total == 0 means unknown
    Progress { done: u64, total: u64 },
    Done(Vec<FileGroup>),
    Error(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ScanPhase {
    Walking,
    Grouping,
    PrefixHashing,
    FullHashing,
}

impl ScanPhase {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Walking => "Scanning files",
            Self::Grouping => "Grouping by size",
            Self::PrefixHashing => "Quick-hashing candidates",
            Self::FullHashing => "Computing full checksums",
        }
    }
}

// ---------------------------------------------------------------------------
// Reclaimer → UI message types
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ReclaimMessage {
    Progress { done: u64, total: u64, reclaimed: u64 },
    Done { total_reclaimed: u64 },
    Error(String),
}
