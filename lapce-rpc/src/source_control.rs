use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct DiffInfo {
    pub head: String,
    pub branches: Vec<String>,
    pub tags: Vec<String>,
    pub diffs: Vec<FileDiff>,
}

/// Blame for a run of consecutive lines in the buffer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlameHunk {
    /// 0-based first line
    pub start_line: usize,
    pub lines: usize,
    /// `None` when the lines are not committed yet
    pub commit: Option<BlameCommit>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlameCommit {
    pub id: String,
    pub author: String,
    /// Unix seconds
    pub time: i64,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum FileDiff {
    Modified(PathBuf),
    Added(PathBuf),
    Deleted(PathBuf),
    Renamed(PathBuf, PathBuf),
}

impl FileDiff {
    pub fn path(&self) -> &PathBuf {
        match &self {
            FileDiff::Modified(p)
            | FileDiff::Added(p)
            | FileDiff::Deleted(p)
            | FileDiff::Renamed(_, p) => p,
        }
    }

    pub fn kind(&self) -> FileDiffKind {
        match self {
            FileDiff::Modified(_) => FileDiffKind::Modified,
            FileDiff::Added(_) => FileDiffKind::Added,
            FileDiff::Deleted(_) => FileDiffKind::Deleted,
            FileDiff::Renamed(_, _) => FileDiffKind::Renamed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileDiffKind {
    Modified,
    Added,
    Deleted,
    Renamed,
}
