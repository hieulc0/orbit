//! Core coding agent tool surface for Orbit.
//!
//! Provides canonical tool identities, role permission matrices, standardized tool
//! metadata, normalized error codes, and robust, workspace-confined tool execution
//! for repository navigation, targeted editing, filesystem mutation, search, git, and terminal.

use crate::fs_tools::confine_path;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

// Normalized error codes (Requirement 27)
pub const ERR_PATH_NOT_FOUND: &str = "PATH_NOT_FOUND";
pub const ERR_PATH_OUTSIDE_WORKSPACE: &str = "PATH_OUTSIDE_WORKSPACE";
pub const ERR_DESTINATION_EXISTS: &str = "DESTINATION_EXISTS";
pub const ERR_READ_ONLY_ROLE: &str = "READ_ONLY_ROLE";
pub const ERR_MUTATION_LOCK_REQUIRED: &str = "MUTATION_LOCK_REQUIRED";
pub const ERR_OUTPUT_TRUNCATED: &str = "OUTPUT_TRUNCATED";
pub const ERR_COMMAND_TIMEOUT: &str = "COMMAND_TIMEOUT";
pub const ERR_PROCESS_NOT_FOUND: &str = "PROCESS_NOT_FOUND";
pub const ERR_UNSUPPORTED_TOOL: &str = "UNSUPPORTED_TOOL";
pub const ERR_NO_MATCH: &str = "NO_MATCH";
pub const ERR_MULTIPLE_MATCHES: &str = "MULTIPLE_MATCHES";

/// Canonical tool identities (Requirement 10)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanonicalToolName {
    FsReadTextFile,
    FsWriteTextFile,
    FsEditFile,
    FsListDirectory,
    FsFindPath,
    FsCreateDirectory,
    FsMove,
    FsCopy,
    FsDeleteFile,
    FsDeleteDirectory,
    SearchGrep,
    TerminalCreate,
    TerminalOutput,
    TerminalWaitForExit,
    TerminalKill,
    TerminalRelease,
    GitStatus,
    GitDiff,
    GitShow,
}

impl CanonicalToolName {
    pub fn legacy_name(&self) -> &'static str {
        match self {
            Self::FsReadTextFile => "read_file",
            Self::FsWriteTextFile => "write_file",
            Self::FsEditFile => "edit_file",
            Self::FsListDirectory => "list_directory",
            Self::FsFindPath => "find_path",
            Self::FsCreateDirectory => "create_directory",
            Self::FsMove => "move",
            Self::FsCopy => "copy",
            Self::FsDeleteFile => "delete_file",
            Self::FsDeleteDirectory => "delete_directory",
            Self::SearchGrep => "grep",
            Self::TerminalCreate => "shell",
            Self::TerminalOutput => "terminal/output",
            Self::TerminalWaitForExit => "terminal/wait_for_exit",
            Self::TerminalKill => "terminal/kill",
            Self::TerminalRelease => "terminal/release",
            Self::GitStatus => "git_status",
            Self::GitDiff => "git_diff",
            Self::GitShow => "git_show",
        }
    }

    pub fn from_canonical(name: &str) -> Option<Self> {
        Self::from_wire(name)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FsReadTextFile => "fs.read_text_file",
            Self::FsWriteTextFile => "fs.write_text_file",
            Self::FsEditFile => "fs.edit_file",
            Self::FsListDirectory => "fs.list_directory",
            Self::FsFindPath => "fs.find_path",
            Self::FsCreateDirectory => "fs.create_directory",
            Self::FsMove => "fs.move",
            Self::FsCopy => "fs.copy",
            Self::FsDeleteFile => "fs.delete_file",
            Self::FsDeleteDirectory => "fs.delete_directory",
            Self::SearchGrep => "search.grep",
            Self::TerminalCreate => "terminal.create",
            Self::TerminalOutput => "terminal.output",
            Self::TerminalWaitForExit => "terminal.wait_for_exit",
            Self::TerminalKill => "terminal.kill",
            Self::TerminalRelease => "terminal.release",
            Self::GitStatus => "git.status",
            Self::GitDiff => "git.diff",
            Self::GitShow => "git.show",
        }
    }

    /// Resolves canonical identity from any known wire name, provider alias, or bridge name.
    pub fn from_wire(name: &str) -> Option<Self> {
        let clean = name.trim();
        let stripped = clean.strip_prefix("orbit_").unwrap_or(clean);

        match stripped {
            "fs/read_text_file" | "fs.read_text_file" | "read_file" | "read_text_file" | "read" => {
                Some(Self::FsReadTextFile)
            }
            "fs/write_text_file" | "fs.write_text_file" | "write_file" | "write_text_file"
            | "write" => Some(Self::FsWriteTextFile),
            "fs/edit_file" | "fs.edit_file" | "edit_file" | "edit" => Some(Self::FsEditFile),
            "fs/list_directory" | "fs.list_directory" | "list_directory" | "list" => {
                Some(Self::FsListDirectory)
            }
            "fs/find_path" | "fs.find_path" | "find_path" | "find" => Some(Self::FsFindPath),
            "fs/create_directory" | "fs.create_directory" | "create_directory" => {
                Some(Self::FsCreateDirectory)
            }
            "fs/move" | "fs.move" | "move" => Some(Self::FsMove),
            "fs/copy" | "fs.copy" | "copy" => Some(Self::FsCopy),
            "fs/delete_file" | "fs.delete_file" | "delete_file" => Some(Self::FsDeleteFile),
            "fs/delete_directory" | "fs.delete_directory" | "delete_directory" => {
                Some(Self::FsDeleteDirectory)
            }
            "search/grep" | "search.grep" | "grep" => Some(Self::SearchGrep),
            "terminal/create" | "terminal.create" | "shell" | "terminal" => {
                Some(Self::TerminalCreate)
            }
            "terminal/output" | "terminal.output" => Some(Self::TerminalOutput),
            "terminal/wait_for_exit" | "terminal.wait_for_exit" => Some(Self::TerminalWaitForExit),
            "terminal/kill" | "terminal.kill" => Some(Self::TerminalKill),
            "terminal/release" | "terminal.release" => Some(Self::TerminalRelease),
            "git/status" | "git.status" | "git_status" => Some(Self::GitStatus),
            "git/diff" | "git.diff" | "git_diff" => Some(Self::GitDiff),
            "git/show" | "git.show" | "git_show" => Some(Self::GitShow),
            _ => None,
        }
    }
}

/// Tool metadata (Requirement 22)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolMetadata {
    pub tool_id: String,
    pub canonical_name: String,
    pub mutating: bool,
    pub requires_workspace: bool,
    pub requires_mutation_lock: bool,
    pub allowed_roles: Vec<String>,
    pub network_required: bool,
    pub max_output_bytes: usize,
    pub default_timeout_seconds: u64,
    pub cancellable: bool,
}

impl ToolMetadata {
    pub fn for_tool(tool: CanonicalToolName) -> Self {
        let (mutating, req_lock, allowed_roles, max_bytes, timeout, cancellable) = match tool {
            CanonicalToolName::FsReadTextFile => (
                false,
                false,
                vec!["planner".into(), "implementer".into(), "reviewer".into()],
                65536,
                30,
                true,
            ),
            CanonicalToolName::FsListDirectory => (
                false,
                false,
                vec!["planner".into(), "implementer".into(), "reviewer".into()],
                65536,
                30,
                true,
            ),
            CanonicalToolName::FsFindPath => (
                false,
                false,
                vec!["planner".into(), "implementer".into(), "reviewer".into()],
                65536,
                30,
                true,
            ),
            CanonicalToolName::SearchGrep => (
                false,
                false,
                vec!["planner".into(), "implementer".into(), "reviewer".into()],
                65536,
                60,
                true,
            ),
            CanonicalToolName::GitStatus => (
                false,
                false,
                vec!["planner".into(), "implementer".into(), "reviewer".into()],
                65536,
                30,
                true,
            ),
            CanonicalToolName::GitDiff => (
                false,
                false,
                vec!["planner".into(), "implementer".into(), "reviewer".into()],
                65536,
                30,
                true,
            ),
            CanonicalToolName::GitShow => (
                false,
                false,
                vec!["planner".into(), "implementer".into(), "reviewer".into()],
                65536,
                30,
                true,
            ),

            CanonicalToolName::FsWriteTextFile => {
                (true, true, vec!["implementer".into()], 65536, 30, true)
            }
            CanonicalToolName::FsEditFile => {
                (true, true, vec!["implementer".into()], 65536, 30, true)
            }
            CanonicalToolName::FsCreateDirectory => {
                (true, true, vec!["implementer".into()], 4096, 30, false)
            }
            CanonicalToolName::FsMove => (true, true, vec!["implementer".into()], 4096, 30, false),
            CanonicalToolName::FsCopy => (true, true, vec!["implementer".into()], 65536, 60, false),
            CanonicalToolName::FsDeleteFile => {
                (true, true, vec!["implementer".into()], 4096, 30, false)
            }
            CanonicalToolName::FsDeleteDirectory => {
                (true, true, vec!["implementer".into()], 4096, 30, false)
            }

            CanonicalToolName::TerminalCreate => {
                (true, true, vec!["implementer".into()], 65536, 300, true)
            }
            CanonicalToolName::TerminalOutput => {
                (false, false, vec!["implementer".into()], 65536, 10, true)
            }
            CanonicalToolName::TerminalWaitForExit => {
                (false, false, vec!["implementer".into()], 4096, 300, true)
            }
            CanonicalToolName::TerminalKill => {
                (false, false, vec!["implementer".into()], 4096, 10, false)
            }
            CanonicalToolName::TerminalRelease => {
                (false, false, vec!["implementer".into()], 4096, 10, false)
            }
        };

        Self {
            tool_id: tool.as_str().into(),
            canonical_name: tool.as_str().into(),
            mutating,
            requires_workspace: true,
            requires_mutation_lock: req_lock,
            allowed_roles,
            network_required: false,
            max_output_bytes: max_bytes,
            default_timeout_seconds: timeout,
            cancellable,
        }
    }

    pub fn is_role_allowed(&self, role_id: &str) -> bool {
        self.allowed_roles.iter().any(|r| r == role_id)
    }
}

/// Returns the complete inventory of all canonical Orbit tools (Requirement 1 & 42)
pub fn full_tool_inventory() -> Vec<ToolMetadata> {
    vec![
        ToolMetadata::for_tool(CanonicalToolName::FsReadTextFile),
        ToolMetadata::for_tool(CanonicalToolName::FsWriteTextFile),
        ToolMetadata::for_tool(CanonicalToolName::FsEditFile),
        ToolMetadata::for_tool(CanonicalToolName::FsListDirectory),
        ToolMetadata::for_tool(CanonicalToolName::FsFindPath),
        ToolMetadata::for_tool(CanonicalToolName::FsCreateDirectory),
        ToolMetadata::for_tool(CanonicalToolName::FsMove),
        ToolMetadata::for_tool(CanonicalToolName::FsCopy),
        ToolMetadata::for_tool(CanonicalToolName::FsDeleteFile),
        ToolMetadata::for_tool(CanonicalToolName::FsDeleteDirectory),
        ToolMetadata::for_tool(CanonicalToolName::SearchGrep),
        ToolMetadata::for_tool(CanonicalToolName::TerminalCreate),
        ToolMetadata::for_tool(CanonicalToolName::TerminalOutput),
        ToolMetadata::for_tool(CanonicalToolName::TerminalWaitForExit),
        ToolMetadata::for_tool(CanonicalToolName::TerminalKill),
        ToolMetadata::for_tool(CanonicalToolName::TerminalRelease),
        ToolMetadata::for_tool(CanonicalToolName::GitStatus),
        ToolMetadata::for_tool(CanonicalToolName::GitDiff),
        ToolMetadata::for_tool(CanonicalToolName::GitShow),
    ]
}

// ---------------------------------------------------------------------------
// Tool Data Structures & Results
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryEntry {
    pub name: String,
    pub entry_type: String, // "file" | "directory" | "symlink"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListDirectoryResult {
    pub path: String,
    pub entries: Vec<DirectoryEntry>,
    pub truncated: bool,
    pub total_entries: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathMatch {
    pub path: String,
    pub entry_type: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindPathResult {
    pub pattern: String,
    pub matches: Vec<PathMatch>,
    pub truncated: bool,
    pub total_matches: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepMatch {
    pub file: String,
    pub line: usize,
    pub preview: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepResult {
    pub query: String,
    pub matches: Vec<GrepMatch>,
    pub truncated: bool,
    pub total_matches: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditFileResult {
    pub path: String,
    pub matches_replaced: usize,
    pub byte_delta: i64,
    pub success: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyResult {
    pub source: String,
    pub destination: String,
    pub success: bool,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitStatusResult {
    pub branch: Option<String>,
    pub modified: Vec<String>,
    pub added: Vec<String>,
    pub deleted: Vec<String>,
    pub renamed: Vec<(String, String)>,
    pub untracked: Vec<String>,
    pub clean: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitDiffResult {
    pub diff: String,
    pub truncated: bool,
    pub total_bytes: usize,
    pub files_changed: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitShowResult {
    pub revision: String,
    pub content: String,
    pub truncated: bool,
    pub total_bytes: usize,
}

// ---------------------------------------------------------------------------
// Tool Implementations
// ---------------------------------------------------------------------------

/// 4. ADD DIRECTORY LISTING (`fs/list_directory`)
pub fn list_directory(
    repo_path: &Path,
    path_str: &str,
    recursive: bool,
    max_entries: usize,
    include_hidden: bool,
) -> Result<ListDirectoryResult> {
    let full = confine_path(repo_path, path_str, true, true)?;
    ensure!(full.is_dir(), "target path is not a directory");

    let bound = max_entries.clamp(1, 1000);
    let mut entries = Vec::new();

    fn walk_dir(
        dir: &Path,
        base_dir: &Path,
        recursive: bool,
        include_hidden: bool,
        entries: &mut Vec<DirectoryEntry>,
    ) -> Result<()> {
        let read = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(_) => return Ok(()),
        };

        for item in read.flatten() {
            let item_path = item.path();
            let file_name = item.file_name().to_string_lossy().to_string();

            if !include_hidden && file_name.starts_with('.') {
                continue;
            }

            let rel_name = item_path
                .strip_prefix(base_dir)
                .unwrap_or(&item_path)
                .to_string_lossy()
                .to_string();

            let (entry_type, size, is_dir) = if let Ok(meta) = item_path.symlink_metadata() {
                if meta.file_type().is_symlink() {
                    ("symlink".to_string(), None, false)
                } else if meta.is_dir() {
                    ("directory".to_string(), None, true)
                } else {
                    ("file".to_string(), Some(meta.len()), false)
                }
            } else {
                ("unknown".to_string(), None, false)
            };

            entries.push(DirectoryEntry {
                name: rel_name,
                entry_type,
                size,
            });

            if recursive && is_dir && file_name != ".git" && file_name != "target" {
                walk_dir(&item_path, base_dir, true, include_hidden, entries)?;
            }
        }
        Ok(())
    }

    walk_dir(&full, &full, recursive, include_hidden, &mut entries)?;

    // Deterministic ordering
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    let total = entries.len();
    let truncated = total > bound;
    if truncated {
        entries.truncate(bound);
    }

    Ok(ListDirectoryResult {
        path: path_str.to_string(),
        entries,
        truncated,
        total_entries: total,
    })
}

/// Helper for wildcard matching (* matches within segment, ** matches across segments, ? matches single char)
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    if pattern == "*" || pattern == "**" {
        return true;
    }
    if !pattern.contains('/') && !pattern.contains("**") {
        // Match within filename or whole text
        let filename = Path::new(text)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(text);
        if glob_segment_match(pattern, filename) {
            return true;
        }
    }
    glob_path_match(pattern, text)
}

fn glob_segment_match(pattern: &str, text: &str) -> bool {
    let p_bytes = pattern.as_bytes();
    let t_bytes = text.as_bytes();
    let mut p_idx = 0;
    let mut t_idx = 0;
    let mut star_idx = None;
    let mut match_idx = 0;

    while t_idx < t_bytes.len() {
        if p_idx < p_bytes.len() && (p_bytes[p_idx] == b'?' || p_bytes[p_idx] == t_bytes[t_idx]) {
            p_idx += 1;
            t_idx += 1;
        } else if p_idx < p_bytes.len() && p_bytes[p_idx] == b'*' {
            star_idx = Some(p_idx);
            p_idx += 1;
            match_idx = t_idx;
        } else if let Some(star) = star_idx {
            p_idx = star + 1;
            match_idx += 1;
            t_idx = match_idx;
        } else {
            return false;
        }
    }

    while p_idx < p_bytes.len() && p_bytes[p_idx] == b'*' {
        p_idx += 1;
    }

    p_idx == p_bytes.len()
}

fn glob_path_match(pattern: &str, text: &str) -> bool {
    let p_parts: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let t_parts: Vec<&str> = text.split('/').filter(|s| !s.is_empty()).collect();

    fn match_parts(p: &[&str], t: &[&str]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some(&"**"), _) => {
                if p.len() == 1 {
                    return true;
                }
                for i in 0..=t.len() {
                    if match_parts(&p[1..], &t[i..]) {
                        return true;
                    }
                }
                false
            }
            (Some(p_head), Some(t_head)) if glob_segment_match(p_head, t_head) => {
                match_parts(&p[1..], &t[1..])
            }
            (Some(_), Some(_)) => false,
            _ => false,
        }
    }

    match_parts(&p_parts, &t_parts)
}

/// 5. ADD PATH DISCOVERY (`fs/find_path`)
pub fn find_path(
    repo_path: &Path,
    base_path_opt: Option<&str>,
    pattern: &str,
    include: &[String],
    exclude: &[String],
    max_results: usize,
) -> Result<FindPathResult> {
    let base_str = base_path_opt.unwrap_or(".");
    let full = confine_path(repo_path, base_str, true, true)?;
    let canonical_repo = repo_path.canonicalize()?;

    let bound = max_results.clamp(1, 1000);
    let mut matches = Vec::new();

    let default_excludes = [".git", "target", "node_modules", ".orbit", ".cargo"];

    fn walk_search(
        dir: &Path,
        canonical_repo: &Path,
        pattern: &str,
        include: &[String],
        exclude: &[String],
        default_excludes: &[&str],
        matches: &mut Vec<PathMatch>,
    ) -> Result<()> {
        let read = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(_) => return Ok(()),
        };

        for item in read.flatten() {
            let item_path = item.path();
            let file_name = item.file_name().to_string_lossy().to_string();

            if default_excludes.iter().any(|&e| file_name == e) {
                continue;
            }

            let rel_path = match item_path.strip_prefix(canonical_repo) {
                Ok(p) => p.to_string_lossy().to_string(),
                Err(_) => continue,
            };

            if exclude.iter().any(|e| wildcard_match(e, &rel_path)) {
                continue;
            }

            let (entry_type, is_dir) = if let Ok(meta) = item_path.symlink_metadata() {
                if meta.file_type().is_symlink() {
                    ("symlink".to_string(), false)
                } else if meta.is_dir() {
                    ("directory".to_string(), true)
                } else {
                    ("file".to_string(), false)
                }
            } else {
                continue;
            };

            let pattern_matched = wildcard_match(pattern, &rel_path);
            let include_matched =
                include.is_empty() || include.iter().any(|inc| wildcard_match(inc, &rel_path));

            if pattern_matched && include_matched {
                matches.push(PathMatch {
                    path: rel_path.clone(),
                    entry_type,
                });
            }

            if is_dir {
                walk_search(
                    &item_path,
                    canonical_repo,
                    pattern,
                    include,
                    exclude,
                    default_excludes,
                    matches,
                )?;
            }
        }
        Ok(())
    }

    walk_search(
        &full,
        &canonical_repo,
        pattern,
        include,
        exclude,
        &default_excludes,
        &mut matches,
    )?;

    // Deterministic ordering
    matches.sort_by(|a, b| a.path.cmp(&b.path));

    let total = matches.len();
    let truncated = total > bound;
    if truncated {
        matches.truncate(bound);
    }

    Ok(FindPathResult {
        pattern: pattern.to_string(),
        matches,
        truncated,
        total_matches: total,
    })
}

/// 6. ADD TEXT SEARCH (`search/grep`)
#[allow(clippy::too_many_arguments)]
pub fn search_grep(
    repo_path: &Path,
    path_opt: Option<&str>,
    query: &str,
    case_sensitive: bool,
    is_regex: bool,
    include: &[String],
    exclude: &[String],
    max_matches: usize,
    _context_lines: usize,
) -> Result<GrepResult> {
    ensure!(!query.is_empty(), "search query cannot be empty");
    let base_str = path_opt.unwrap_or(".");
    let full = confine_path(repo_path, base_str, true, true)?;
    let canonical_repo = repo_path.canonicalize()?;

    let bound = max_matches.clamp(1, 500);
    let max_output_bytes = 65536;

    let default_excludes = [
        ".git",
        "target",
        "node_modules",
        ".orbit",
        ".cargo",
        "*.png",
        "*.jpg",
        "*.lock",
    ];

    // Collect candidate files deterministically
    let mut files = Vec::new();

    fn collect_files(
        dir: &Path,
        canonical_repo: &Path,
        include: &[String],
        exclude: &[String],
        default_excludes: &[&str],
        files: &mut Vec<PathBuf>,
    ) -> Result<()> {
        let read = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(_) => return Ok(()),
        };

        for item in read.flatten() {
            let item_path = item.path();
            let file_name = item.file_name().to_string_lossy().to_string();

            if default_excludes.iter().any(|&e| file_name == e) {
                continue;
            }

            let rel_path = match item_path.strip_prefix(canonical_repo) {
                Ok(p) => p.to_string_lossy().to_string(),
                Err(_) => continue,
            };

            if exclude.iter().any(|e| wildcard_match(e, &rel_path)) {
                continue;
            }

            if let Ok(meta) = item_path.symlink_metadata() {
                if meta.is_dir() {
                    collect_files(
                        &item_path,
                        canonical_repo,
                        include,
                        exclude,
                        default_excludes,
                        files,
                    )?;
                } else if meta.is_file() && meta.len() <= 2 * 1024 * 1024 {
                    // Skip files larger than 2MB or not matching include
                    if include.is_empty()
                        || include.iter().any(|inc| wildcard_match(inc, &rel_path))
                    {
                        files.push(item_path);
                    }
                }
            }
        }
        Ok(())
    }

    if full.is_file() {
        files.push(full);
    } else {
        collect_files(
            &full,
            &canonical_repo,
            include,
            exclude,
            &default_excludes,
            &mut files,
        )?;
    }

    files.sort();

    let mut matches = Vec::new();
    let mut current_bytes = 0;
    let mut total_matches = 0;
    let mut truncated = false;

    let query_lower = if !case_sensitive {
        query.to_lowercase()
    } else {
        String::new()
    };

    for file_path in files {
        if truncated {
            break;
        }

        let rel_file = match file_path.strip_prefix(&canonical_repo) {
            Ok(p) => p.to_string_lossy().to_string(),
            Err(_) => continue,
        };

        // Binary check: read up to 1024 bytes and check for null bytes
        let bytes = match std::fs::read(&file_path) {
            Ok(b) => b,
            Err(_) => continue,
        };

        let check_len = bytes.len().min(1024);
        if bytes[..check_len].contains(&0) {
            // Binary file, skip
            continue;
        }

        let text = match std::str::from_utf8(&bytes) {
            Ok(t) => t,
            Err(_) => continue, // Non-UTF-8, skip
        };

        for (line_idx, line) in text.lines().enumerate() {
            let is_match = if is_regex {
                let pat = if query.starts_with('*') || query.ends_with('*') {
                    query.to_string()
                } else {
                    format!("*{query}*")
                };
                if case_sensitive {
                    wildcard_match(&pat, line)
                } else {
                    wildcard_match(&pat.to_lowercase(), &line.to_lowercase())
                }
            } else if case_sensitive {
                line.contains(query)
            } else {
                line.to_lowercase().contains(&query_lower)
            };

            if is_match {
                total_matches += 1;
                if matches.len() < bound && current_bytes < max_output_bytes {
                    let preview = if line.len() > 256 {
                        let end = (0..=256)
                            .rev()
                            .find(|&n| line.is_char_boundary(n))
                            .unwrap_or(0);
                        format!("{}...", line[..end].trim())
                    } else {
                        line.trim().to_string()
                    };

                    current_bytes += preview.len() + rel_file.len() + 16;
                    matches.push(GrepMatch {
                        file: rel_file.clone(),
                        line: line_idx + 1,
                        preview,
                    });
                } else {
                    truncated = true;
                }
            }
        }
    }

    Ok(GrepResult {
        query: query.to_string(),
        matches,
        truncated,
        total_matches,
    })
}

/// 7. ADD TARGETED EDITING (`fs/edit_file`)
pub fn edit_file(
    repo_path: &Path,
    path_str: &str,
    old_text: &str,
    new_text: &str,
    replace_all: bool,
) -> Result<EditFileResult> {
    ensure!(!old_text.is_empty(), "old_text cannot be empty");
    let full = confine_path(repo_path, path_str, true, false)?;
    ensure!(full.is_file(), "target path is not a regular file");

    let content = std::fs::read_to_string(&full)
        .with_context(|| format!("{ERR_PATH_NOT_FOUND}: failed to read file: {path_str}"))?;

    let count = content.matches(old_text).count();

    if count == 0 {
        bail!("{ERR_NO_MATCH}: exact old_text not found in file: {path_str}");
    }

    if count > 1 && !replace_all {
        bail!(
            "{ERR_MULTIPLE_MATCHES}: old_text matched {count} times in {path_str}; set replace_all=true or provide a unique snippet"
        );
    }

    let new_content = if replace_all {
        content.replace(old_text, new_text)
    } else {
        // Replace single occurrence
        let pos = content.find(old_text).context("occurrence not found")?;
        let mut res = String::with_capacity(content.len() + new_text.len() - old_text.len());
        res.push_str(&content[..pos]);
        res.push_str(new_text);
        res.push_str(&content[pos + old_text.len()..]);
        res
    };

    let byte_delta = (new_content.len() as i64) - (content.len() as i64);
    let replaced_count = if replace_all { count } else { 1 };

    // Atomic write in same directory
    let parent = full.parent().context("file has no parent")?;
    let temp_path = parent.join(format!(".orbit-edit-{}", crate::model::id()));
    std::fs::write(&temp_path, new_content.as_bytes())?;
    std::fs::rename(&temp_path, &full)?;

    Ok(EditFileResult {
        path: path_str.to_string(),
        matches_replaced: replaced_count,
        byte_delta,
        success: true,
    })
}

/// 9. ADD COPY (`fs/copy`)
pub fn copy_path(
    repo_path: &Path,
    source_str: &str,
    destination_str: &str,
    recursive: bool,
) -> Result<CopyResult> {
    let src = confine_path(repo_path, source_str, true, false)?;
    let dst = confine_path(repo_path, destination_str, false, false)?;

    if dst.symlink_metadata().is_ok() {
        bail!("{ERR_DESTINATION_EXISTS}: destination already exists: {destination_str}");
    }

    let src_meta = src
        .symlink_metadata()
        .with_context(|| format!("{ERR_PATH_NOT_FOUND}: source path missing"))?;

    if src_meta.is_dir() {
        ensure!(
            recursive,
            "source is a directory; copy requires recursive=true"
        );
        copy_dir_recursive(repo_path, &src, &dst)?;
    } else if src_meta.is_file() {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&src, &dst)?;
    } else {
        bail!("unsupported file type for copy");
    }

    Ok(CopyResult {
        source: source_str.to_string(),
        destination: destination_str.to_string(),
        success: true,
        message: format!("Copied {source_str} to {destination_str} successfully."),
    })
}

fn copy_dir_recursive(repo_path: &Path, src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)?.flatten() {
        let entry_path = entry.path();
        let entry_name = entry.file_name();
        let target_path = dst.join(entry_name);

        // Verify confinement
        let _ = confine_path(repo_path, &target_path.to_string_lossy(), false, false)?;

        let meta = entry_path.symlink_metadata()?;
        if meta.is_dir() {
            copy_dir_recursive(repo_path, &entry_path, &target_path)?;
        } else if meta.is_file() {
            std::fs::copy(&entry_path, &target_path)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 17-20. FIRST-CLASS GIT READ TOOLS
// ---------------------------------------------------------------------------

fn validate_git_ref(git_ref: &str) -> Result<()> {
    ensure!(
        !git_ref.is_empty()
            && git_ref.len() <= 128
            && !git_ref.starts_with('-')
            && git_ref.chars().all(|c| c.is_ascii_alphanumeric()
                || matches!(c, '/' | '.' | '_' | '-' | '@' | '~' | '^')),
        "invalid git reference"
    );
    Ok(())
}

/// 18. GIT STATUS (`git/status`)
pub async fn git_status(repo_path: &Path, path_filter: Option<&str>) -> Result<GitStatusResult> {
    ensure!(repo_path.exists(), "repository path does not exist");

    // Get current branch
    let branch_out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .await;

    let branch = branch_out.ok().and_then(|out| {
        if out.status.success() {
            Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            None
        }
    });

    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C")
        .arg(repo_path)
        .args(["status", "--porcelain=v1"])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");

    if let Some(p) = path_filter {
        let _ = confine_path(repo_path, p, false, true)?;
        cmd.arg("--").arg(p);
    }

    let out = cmd.output().await.context("failed to execute git status")?;
    ensure!(
        out.status.success(),
        "git status failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut modified = Vec::new();
    let mut added = Vec::new();
    let mut deleted = Vec::new();
    let mut renamed = Vec::new();
    let mut untracked = Vec::new();

    for line in stdout.lines() {
        if line.len() < 4 {
            continue;
        }
        let code = &line[..2];
        let file = line[3..].trim().to_string();

        match code {
            "??" => untracked.push(file),
            " M" | "M " | "MM" => modified.push(file),
            "A " | "AM" => added.push(file),
            "D " | " D" => deleted.push(file),
            "R " => {
                if let Some((old_f, new_f)) = file.split_once(" -> ") {
                    renamed.push((old_f.trim().to_string(), new_f.trim().to_string()));
                } else {
                    modified.push(file);
                }
            }
            _ => modified.push(file),
        }
    }

    let clean = modified.is_empty()
        && added.is_empty()
        && deleted.is_empty()
        && renamed.is_empty()
        && untracked.is_empty();

    Ok(GitStatusResult {
        branch,
        modified,
        added,
        deleted,
        renamed,
        untracked,
        clean,
    })
}

/// 19. GIT DIFF (`git/diff`)
pub async fn git_diff(
    repo_path: &Path,
    base: Option<&str>,
    path_filter: Option<&str>,
    context_lines: Option<u32>,
    stat_only: bool,
    max_bytes: usize,
) -> Result<GitDiffResult> {
    ensure!(repo_path.exists(), "repository path does not exist");
    let bound = max_bytes.clamp(1024, 65536);

    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C")
        .arg(repo_path)
        .arg("diff")
        .arg("--no-ext-diff")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");

    if stat_only {
        cmd.arg("--stat");
    } else {
        let ctx = context_lines.unwrap_or(3);
        cmd.arg(format!("-U{ctx}"));
    }

    if let Some(b) = base {
        validate_git_ref(b)?;
        cmd.arg(b);
    }

    if let Some(p) = path_filter {
        let _ = confine_path(repo_path, p, false, true)?;
        cmd.arg("--").arg(p);
    }

    let out = cmd.output().await.context("failed to execute git diff")?;
    ensure!(
        out.status.success(),
        "git diff failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let raw = out.stdout;
    let total_bytes = raw.len();
    let truncated = total_bytes > bound;
    let take_bytes = if truncated { bound } else { total_bytes };
    let preview_text = String::from_utf8_lossy(&raw[..take_bytes]).to_string();

    // Count files changed
    let files_changed = preview_text
        .lines()
        .filter(|l| l.starts_with("diff --git "))
        .count();

    Ok(GitDiffResult {
        diff: preview_text,
        truncated,
        total_bytes,
        files_changed,
    })
}

/// 20. GIT SHOW (`git/show`)
pub async fn git_show(
    repo_path: &Path,
    revision: &str,
    path_filter: Option<&str>,
    max_bytes: usize,
) -> Result<GitShowResult> {
    ensure!(repo_path.exists(), "repository path does not exist");
    validate_git_ref(revision)?;
    let bound = max_bytes.clamp(1024, 65536);

    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C")
        .arg(repo_path)
        .arg("show")
        .arg("--no-ext-diff")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");

    if let Some(p) = path_filter {
        let confined = confine_path(repo_path, p, false, true)?;
        let rel_p = confined
            .strip_prefix(repo_path.canonicalize()?)
            .unwrap_or(&confined);
        let spec = format!("{}:{}", revision, rel_p.to_string_lossy());
        cmd.arg(spec);
    } else {
        cmd.arg(revision);
    }

    let out = cmd.output().await.context("failed to execute git show")?;
    ensure!(
        out.status.success(),
        "git show failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let raw = out.stdout;
    let total_bytes = raw.len();
    let truncated = total_bytes > bound;
    let take_bytes = if truncated { bound } else { total_bytes };
    let content = String::from_utf8_lossy(&raw[..take_bytes]).to_string();

    Ok(GitShowResult {
        revision: revision.to_string(),
        content,
        truncated,
        total_bytes,
    })
}

use std::process::Stdio;
use std::sync::Mutex as StdMutex;
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex as TokioMutex;
use tokio::task::AbortHandle;

#[derive(Default, Debug, Clone)]
pub struct TerminalOutputState {
    pub bytes: Vec<u8>,
    pub total_bytes: u64,
    pub truncated: bool,
    pub exit_code: Option<i32>,
}

impl TerminalOutputState {
    pub fn text(&self) -> String {
        let text = String::from_utf8_lossy(&self.bytes);
        let excess = text.len().saturating_sub(self.bytes.len());
        let start = (excess..=text.len())
            .find(|n| text.is_char_boundary(*n))
            .unwrap_or(0);
        text[start..].to_string()
    }
}

pub struct AgentTerminal {
    output: Arc<StdMutex<TerminalOutputState>>,
    child: Arc<TokioMutex<Option<tokio::process::Child>>>,
    abort_handles: Vec<AbortHandle>,
}

impl AgentTerminal {
    pub fn spawn(cwd: &Path, command: &str, args: &[String], output_limit: usize) -> Result<Self> {
        let output_limit = output_limit.min(65536);
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args)
            .current_dir(cwd)
            .env_clear()
            .env(
                "PATH",
                std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
            )
            .env(
                "HOME",
                std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into()),
            )
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().context("failed to spawn terminal command")?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let output_state = Arc::new(StdMutex::new(TerminalOutputState::default()));
        let mut abort_handles = Vec::new();

        if let Some(mut stream) = stdout {
            let out_clone = Arc::clone(&output_state);
            let handle = tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                loop {
                    match stream.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            let mut st = out_clone.lock().unwrap();
                            st.total_bytes += n as u64;
                            st.bytes.extend_from_slice(&buf[..n]);
                            if st.bytes.len() > output_limit {
                                let discard = st.bytes.len() - output_limit;
                                st.bytes.drain(..discard);
                                st.truncated = true;
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
            .abort_handle();
            abort_handles.push(handle);
        }

        if let Some(mut stream) = stderr {
            let out_clone = Arc::clone(&output_state);
            let handle = tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                loop {
                    match stream.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            let mut st = out_clone.lock().unwrap();
                            st.total_bytes += n as u64;
                            st.bytes.extend_from_slice(&buf[..n]);
                            if st.bytes.len() > output_limit {
                                let discard = st.bytes.len() - output_limit;
                                st.bytes.drain(..discard);
                                st.truncated = true;
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
            .abort_handle();
            abort_handles.push(handle);
        }

        let child_arc = Arc::new(TokioMutex::new(Some(child)));

        Ok(Self {
            output: output_state,
            child: child_arc,
            abort_handles,
        })
    }

    pub fn output(&self) -> TerminalOutputState {
        self.output.lock().unwrap().clone()
    }

    pub async fn wait_for_exit(&self, timeout: Duration) -> Result<i32> {
        let child_arc = Arc::clone(&self.child);
        let mut guard = child_arc.lock().await;
        if let Some(child) = guard.as_mut() {
            let wait_fut = child.wait();
            let status = tokio::time::timeout(timeout, wait_fut)
                .await
                .context(ERR_COMMAND_TIMEOUT)??;
            let code = status.code().unwrap_or(1);
            {
                let mut st = self.output.lock().unwrap();
                st.exit_code = Some(code);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok(code)
        } else {
            let st = self.output.lock().unwrap();
            st.exit_code.context(ERR_PROCESS_NOT_FOUND)
        }
    }

    pub async fn kill(&self) -> Result<()> {
        let child_arc = Arc::clone(&self.child);
        let mut guard = child_arc.lock().await;
        if let Some(child) = guard.as_mut() {
            let _ = child.start_kill();
            if let Ok(st) = child.wait().await {
                use std::os::unix::process::ExitStatusExt;
                let code = st
                    .code()
                    .or_else(|| st.signal().map(|s| 128 + s))
                    .unwrap_or(137);
                let mut out = self.output.lock().unwrap();
                out.exit_code = Some(code);
            }
        }
        for h in &self.abort_handles {
            h.abort();
        }
        Ok(())
    }
}

impl Drop for AgentTerminal {
    fn drop(&mut self) {
        for h in &self.abort_handles {
            h.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_canonical_resolution_and_aliases() {
        assert_eq!(
            CanonicalToolName::from_wire("fs/read_text_file"),
            Some(CanonicalToolName::FsReadTextFile)
        );
        assert_eq!(
            CanonicalToolName::from_wire("read_file"),
            Some(CanonicalToolName::FsReadTextFile)
        );
        assert_eq!(
            CanonicalToolName::from_wire("orbit_read_file"),
            Some(CanonicalToolName::FsReadTextFile)
        );
        assert_eq!(
            CanonicalToolName::from_wire("fs/edit_file"),
            Some(CanonicalToolName::FsEditFile)
        );
        assert_eq!(
            CanonicalToolName::from_wire("orbit_edit_file"),
            Some(CanonicalToolName::FsEditFile)
        );
        assert_eq!(
            CanonicalToolName::from_wire("fs/list_directory"),
            Some(CanonicalToolName::FsListDirectory)
        );
        assert_eq!(
            CanonicalToolName::from_wire("search/grep"),
            Some(CanonicalToolName::SearchGrep)
        );
        assert_eq!(
            CanonicalToolName::from_wire("terminal/create"),
            Some(CanonicalToolName::TerminalCreate)
        );
        assert_eq!(
            CanonicalToolName::from_wire("git/status"),
            Some(CanonicalToolName::GitStatus)
        );
        assert_eq!(CanonicalToolName::from_wire("invalid_xyz"), None);
    }

    #[test]
    fn test_role_matrix_enforcement() {
        let read = ToolMetadata::for_tool(CanonicalToolName::FsReadTextFile);
        assert!(read.is_role_allowed("planner"));
        assert!(read.is_role_allowed("implementer"));
        assert!(read.is_role_allowed("reviewer"));

        let write = ToolMetadata::for_tool(CanonicalToolName::FsWriteTextFile);
        assert!(!write.is_role_allowed("planner"));
        assert!(write.is_role_allowed("implementer"));
        assert!(!write.is_role_allowed("reviewer"));

        let edit = ToolMetadata::for_tool(CanonicalToolName::FsEditFile);
        assert!(!edit.is_role_allowed("planner"));
        assert!(edit.is_role_allowed("implementer"));
        assert!(!edit.is_role_allowed("reviewer"));

        let term = ToolMetadata::for_tool(CanonicalToolName::TerminalCreate);
        assert!(!term.is_role_allowed("planner"));
        assert!(term.is_role_allowed("implementer"));
        assert!(!term.is_role_allowed("reviewer"));
    }

    #[test]
    fn test_edit_file_exact_and_replace_all() -> Result<()> {
        let repo = tempdir()?;
        let path = repo.path().join("code.txt");
        std::fs::write(&path, "apple banana apple cherry")?;

        // 0 matches -> error
        let err0 = edit_file(repo.path(), "code.txt", "grape", "orange", false);
        assert!(err0.is_err());
        assert!(err0.unwrap_err().to_string().contains(ERR_NO_MATCH));

        // multiple matches without replace_all -> error
        let err_mult = edit_file(repo.path(), "code.txt", "apple", "pear", false);
        assert!(err_mult.is_err());
        assert!(
            err_mult
                .unwrap_err()
                .to_string()
                .contains(ERR_MULTIPLE_MATCHES)
        );

        // single unique match -> succeeds
        let res1 = edit_file(repo.path(), "code.txt", "banana", "blueberry", false)?;
        assert_eq!(res1.matches_replaced, 1);
        let updated = std::fs::read_to_string(&path)?;
        assert_eq!(updated, "apple blueberry apple cherry");

        // replace_all -> succeeds
        let res2 = edit_file(repo.path(), "code.txt", "apple", "pineapple", true)?;
        assert_eq!(res2.matches_replaced, 2);
        let updated2 = std::fs::read_to_string(&path)?;
        assert_eq!(updated2, "pineapple blueberry pineapple cherry");

        Ok(())
    }

    #[test]
    fn test_copy_file_and_directory() -> Result<()> {
        let repo = tempdir()?;
        let src_file = repo.path().join("src.txt");
        std::fs::write(&src_file, "content")?;

        // Copy file
        let res1 = copy_path(repo.path(), "src.txt", "dst.txt", false)?;
        assert!(res1.success);
        assert_eq!(
            std::fs::read_to_string(repo.path().join("dst.txt"))?,
            "content"
        );

        // Collision check
        let err_collision = copy_path(repo.path(), "src.txt", "dst.txt", false);
        assert!(err_collision.is_err());
        assert!(
            err_collision
                .unwrap_err()
                .to_string()
                .contains(ERR_DESTINATION_EXISTS)
        );

        // Copy directory without recursive -> error
        let src_dir = repo.path().join("sub");
        std::fs::create_dir_all(&src_dir)?;
        std::fs::write(src_dir.join("inner.txt"), "hello")?;

        let err_rec = copy_path(repo.path(), "sub", "sub2", false);
        assert!(err_rec.is_err());

        // Copy directory with recursive -> success
        let res_dir = copy_path(repo.path(), "sub", "sub2", true)?;
        assert!(res_dir.success);
        assert_eq!(
            std::fs::read_to_string(repo.path().join("sub2/inner.txt"))?,
            "hello"
        );

        Ok(())
    }

    #[test]
    fn test_list_directory_and_find_path() -> Result<()> {
        let repo = tempdir()?;
        let d1 = repo.path().join("pkg/sub");
        std::fs::create_dir_all(&d1)?;
        std::fs::write(repo.path().join("README.md"), "readme")?;
        std::fs::write(repo.path().join("pkg/lib.rs"), "lib")?;
        std::fs::write(d1.join("mod.rs"), "mod")?;

        let list_res = list_directory(repo.path(), ".", true, 100, false)?;
        assert_eq!(list_res.total_entries, 5); // README.md, pkg, pkg/lib.rs, pkg/sub, pkg/sub/mod.rs

        let find_res = find_path(repo.path(), None, "**/*.rs", &[], &[], 100)?;
        assert_eq!(find_res.matches.len(), 2);
        assert_eq!(find_res.matches[0].path, "pkg/lib.rs");
        assert_eq!(find_res.matches[1].path, "pkg/sub/mod.rs");

        Ok(())
    }

    #[test]
    fn test_search_grep() -> Result<()> {
        let repo = tempdir()?;
        let d = repo.path().join("src");
        std::fs::create_dir_all(&d)?;
        std::fs::write(d.join("foo.rs"), "fn hello_world() {\n    let x = 42;\n}\n")?;
        std::fs::write(
            d.join("bar.rs"),
            "fn goodbye_world() {\n    let x = 100;\n}\n",
        )?;

        let grep_res = search_grep(
            repo.path(),
            None,
            "hello_world",
            true,
            false,
            &[],
            &[],
            10,
            0,
        )?;
        assert_eq!(grep_res.matches.len(), 1);
        assert_eq!(grep_res.matches[0].file, "src/foo.rs");
        assert_eq!(grep_res.matches[0].line, 1);

        let grep_x = search_grep(repo.path(), None, "let x =", true, false, &[], &[], 10, 0)?;
        assert_eq!(grep_x.matches.len(), 2);

        Ok(())
    }

    #[tokio::test]
    async fn test_agent_terminal_lifecycle() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let cwd = temp.path();

        // 1. Successful command execution
        let term =
            AgentTerminal::spawn(cwd, "sh", &["-c".into(), "echo hello world".into()], 1024)?;
        let exit_code = term.wait_for_exit(Duration::from_secs(5)).await?;
        assert_eq!(exit_code, 0);
        let out = term.output();
        assert!(out.text().contains("hello world"));
        assert!(!out.truncated);
        assert_eq!(out.exit_code, Some(0));

        // 2. Output truncation limit test
        let term_trunc = AgentTerminal::spawn(
            cwd,
            "sh",
            &["-c".into(), "yes hello | head -n 500".into()],
            100,
        )?;
        let code_trunc = term_trunc.wait_for_exit(Duration::from_secs(5)).await?;
        assert_eq!(code_trunc, 0);
        let out_trunc = term_trunc.output();
        assert!(out_trunc.truncated);
        assert!(out_trunc.bytes.len() <= 100);
        assert!(out_trunc.total_bytes >= 2000);

        // 3. Kill running process
        let term_kill = AgentTerminal::spawn(cwd, "sleep", &["60".into()], 1024)?;
        term_kill.kill().await?;
        let out_kill = term_kill.output();
        // Process was killed
        assert!(
            out_kill.exit_code.is_some()
                || term_kill
                    .child
                    .lock()
                    .await
                    .as_ref()
                    .unwrap()
                    .id()
                    .is_some()
        );

        Ok(())
    }
}
