//! Small read_dir-based directory walking helpers, replacing `glob` patterns
//! that were built from `Path::display()` (backslashes in Windows paths are
//! treated as escape characters by the glob crate, silently matching nothing).

use std::path::{Path, PathBuf};

/// Symlink-cycle guard for the recursive walk.
const MAX_DEPTH: usize = 16;

/// All `*.jsonl` files under `dir`, recursively (the old `**/*.jsonl`).
/// Unreadable directories are skipped silently, like a failed glob was.
pub fn jsonl_files_recursive(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(dir, &mut out, 0);
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out, depth + 1);
        } else if has_ext(&path, "jsonl") {
            out.push(path);
        }
    }
}

/// The `*.jsonl` files directly inside `dir` (the old `*.jsonl`).
pub fn jsonl_files_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && has_ext(p, "jsonl"))
        .collect()
}

/// The immediate subdirectories of `dir` (the old `*/` pattern segment).
pub fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect()
}

fn has_ext(path: &Path, ext: &str) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some(ext)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ccpark-walk-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn recursive_walk_finds_nested_jsonl() {
        let root = tmp_root("rec");
        std::fs::create_dir_all(root.join("p1/sid/subagents")).unwrap();
        std::fs::write(root.join("p1/main.jsonl"), "").unwrap();
        std::fs::write(root.join("p1/sid/subagents/agent-a.jsonl"), "").unwrap();
        std::fs::write(root.join("p1/readme.txt"), "").unwrap();

        let mut found = jsonl_files_recursive(&root);
        found.sort();
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|p| has_ext(p, "jsonl")));
    }

    #[test]
    fn flat_listing_ignores_dirs_and_other_exts() {
        let root = tmp_root("flat");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a.jsonl"), "").unwrap();
        std::fs::write(root.join("sub/b.jsonl"), "").unwrap();
        std::fs::write(root.join("c.json"), "").unwrap();

        let found = jsonl_files_in(&root);
        assert_eq!(found.len(), 1);
        assert!(found[0].ends_with("a.jsonl"));
    }

    #[test]
    fn subdirs_lists_only_directories() {
        let root = tmp_root("subs");
        std::fs::create_dir_all(root.join("d1")).unwrap();
        std::fs::create_dir_all(root.join("d2")).unwrap();
        std::fs::write(root.join("f.jsonl"), "").unwrap();

        let mut found = subdirs(&root);
        found.sort();
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn missing_dir_is_empty_not_error() {
        let root = tmp_root("missing").join("nope");
        assert!(jsonl_files_recursive(&root).is_empty());
        assert!(jsonl_files_in(&root).is_empty());
        assert!(subdirs(&root).is_empty());
    }
}
