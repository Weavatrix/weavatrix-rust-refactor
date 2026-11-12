//! The order of retained applies, and the directories a rename created for them.
//!
//! Undo ids are 32 random hex characters. The worktree lists receipts in lexicographic order,
//! so the last receipt is not the newest apply. This file is the order we applied in. It also
//! remembers directories that did not exist before the apply, so a rollback can remove them
//! once they are empty again. The worktree restores file bytes and leaves those directories.

use blazingly_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

const JOURNAL_NAME: &str = "refactor-journal.json";
const LOCK_NAME: &str = "refactor-write.lock";
const STALE_AFTER: Duration = Duration::from_secs(10 * 60);

struct Journal {
    undos: Vec<String>,
    dirs: BTreeMap<String, Vec<String>>,
}

impl Journal {
    fn empty() -> Self {
        Self {
            undos: Vec::new(),
            dirs: BTreeMap::new(),
        }
    }
}

fn control_dir(root: &Path) -> PathBuf {
    root.join(".weavatrix")
}

fn journal_path(root: &Path) -> PathBuf {
    control_dir(root).join(JOURNAL_NAME)
}

fn load(root: &Path) -> Journal {
    let Ok(text) = std::fs::read_to_string(journal_path(root)) else {
        return Journal::empty();
    };
    let Ok(value) = blazingly_json::from_str::<Value>(&text) else {
        return Journal::empty();
    };
    let undos = value
        .get("undos")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let mut dirs = BTreeMap::new();
    if let Some(object) = value.get("dirs").and_then(Value::as_object) {
        for (id, paths) in object {
            let Some(paths) = paths.as_array() else {
                continue;
            };
            dirs.insert(
                id.clone(),
                paths
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
            );
        }
    }
    Journal { undos, dirs }
}

fn store(root: &Path, journal: &Journal) {
    let dir = control_dir(root);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let mut undos = Vec::new();
    for id in &journal.undos {
        undos.push(Value::String(id.clone()));
    }
    let mut dirs = Map::new();
    for (id, paths) in &journal.dirs {
        let values = paths.iter().cloned().map(Value::String).collect();
        dirs.insert(id.clone(), Value::Array(values));
    }
    let mut object = Map::new();
    object.insert("undos".to_owned(), Value::Array(undos));
    object.insert("dirs".to_owned(), Value::Object(dirs));
    let Ok(text) = blazingly_json::to_string(&Value::Object(object)) else {
        return;
    };
    let temporary = dir.join("refactor-journal.json.tmp");
    if std::fs::write(&temporary, text).is_err() {
        return;
    }
    let destination = journal_path(root);
    if std::fs::rename(&temporary, &destination).is_err() {
        let _ = std::fs::remove_file(&destination);
        let _ = std::fs::rename(&temporary, &destination);
    }
}

/// Records a successful apply. `dirs` are repository-relative parents created for it.
pub(super) fn note_apply(root: &Path, undo_id: &str, dirs: &[String]) {
    let mut journal = load(root);
    journal.undos.retain(|id| id != undo_id);
    journal.undos.push(undo_id.to_owned());
    if dirs.is_empty() {
        journal.dirs.remove(undo_id);
    } else {
        journal.dirs.insert(undo_id.to_owned(), dirs.to_vec());
    }
    store(root, &journal);
}

/// Drops one apply from the journal and returns the directories it created.
pub(super) fn forget_undo(root: &Path, undo_id: &str) -> Vec<String> {
    let mut journal = load(root);
    journal.undos.retain(|id| id != undo_id);
    let dirs = journal.dirs.remove(undo_id).unwrap_or_default();
    store(root, &journal);
    dirs
}

/// The newest recorded apply that is still retained.
///
/// A journal that does not name any current receipt falls back to that receipt only when it is
/// the only one. Several unrecorded receipts have no trustworthy order, so the caller reports
/// that there is nothing it can roll back safely.
pub(super) fn latest_undo(root: &Path, receipt_ids: &[String]) -> Option<String> {
    let journal = load(root);
    if let Some(id) = journal
        .undos
        .iter()
        .rev()
        .find(|id| receipt_ids.iter().any(|receipt| receipt == *id))
    {
        return Some(id.clone());
    }
    if receipt_ids.len() == 1 {
        return receipt_ids.first().cloned();
    }
    None
}

/// Removes directories this crate created, deepest first, and only while they are empty.
pub(super) fn prune_empty(root: &Path, relative_dirs: &[String]) {
    let mut dirs: Vec<&String> = relative_dirs.iter().collect();
    dirs.sort_by_key(|path| {
        std::cmp::Reverse(
            path.bytes()
                .filter(|byte| *byte == b'/' || *byte == b'\\')
                .count(),
        )
    });
    for relative in dirs {
        let Some(path) = confined(root, relative) else {
            continue;
        };
        let Ok(meta) = path.symlink_metadata() else {
            continue;
        };
        if meta.file_type().is_symlink() || !meta.is_dir() {
            continue;
        }
        let Ok(mut entries) = std::fs::read_dir(&path) else {
            continue;
        };
        if entries.next().is_none() {
            let _ = std::fs::remove_dir(&path);
        }
    }
}

fn confined(root: &Path, relative: &str) -> Option<PathBuf> {
    if relative.is_empty() {
        return None;
    }
    let path = Path::new(relative);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return None;
    }
    Some(root.join(path))
}

fn busy(reason: impl Into<String>) -> Value {
    json!({
        "status": "REPO_BUSY",
        "reason": reason.into(),
    })
}

/// A process-wide write lock for one repository.
///
/// The same process already serialises a repository on one lane. A second process — another
/// agent, or the CLI — does not share that lane. The lock is the file's existence. It is not a
/// held handle, so a crash leaves it behind and the next writer takes it after ten minutes.
#[must_use]
pub(super) struct WriteLock {
    path: PathBuf,
}

impl WriteLock {
    pub(super) fn acquire(root: &Path) -> Result<Self, Value> {
        let dir = control_dir(root);
        if linked(&dir) {
            return Err(busy("the repository control directory is a symbolic link"));
        }
        if let Err(error) = std::fs::create_dir_all(&dir) {
            return Err(busy(format!(
                "the repository could not be locked for writing: {error}"
            )));
        }
        let path = dir.join(LOCK_NAME);
        if linked(&path) {
            return Err(busy("the repository write lock is a symbolic link"));
        }
        if create_lock(&path)
            || (stale(&path) && std::fs::remove_file(&path).is_ok() && create_lock(&path))
        {
            return Ok(Self { path });
        }
        Err(busy("another process is writing this repository"))
    }
}

impl Drop for WriteLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn linked(path: &Path) -> bool {
    path.symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink())
}

fn create_lock(path: &Path) -> bool {
    let Ok(mut file) = OpenOptions::new().write(true).create_new(true).open(path) else {
        return false;
    };
    let _ = writeln!(file, "{}", std::process::id());
    true
}

fn stale(path: &Path) -> bool {
    if linked(path) {
        return false;
    }
    let Ok(meta) = path.metadata() else {
        return true;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    modified.elapsed().is_ok_and(|age| age >= STALE_AFTER)
}

#[cfg(test)]
mod tests {
    use super::{WriteLock, forget_undo, latest_undo, note_apply, prune_empty};

    fn root(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("wvxr-journal-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("root");
        path
    }

    #[test]
    fn the_newest_recorded_apply_wins_over_lexicographic_order() {
        let root = root("order");
        let older = "f".repeat(32);
        let newer = "0".repeat(32);
        note_apply(&root, &older, &[]);
        note_apply(&root, &newer, &["src/deep".to_owned()]);
        let receipts = vec![older.clone(), newer.clone()];
        assert_eq!(
            latest_undo(&root, &receipts).as_deref(),
            Some(newer.as_str())
        );
        let dirs = forget_undo(&root, &newer);
        assert_eq!(dirs, vec!["src/deep".to_owned()]);
        assert_eq!(
            latest_undo(&root, &receipts).as_deref(),
            Some(older.as_str())
        );
    }

    #[test]
    fn one_unrecorded_receipt_is_still_the_one_to_roll_back() {
        let root = root("single");
        let only = vec!["a".repeat(32)];
        assert_eq!(latest_undo(&root, &only), Some(only[0].clone()));
        assert!(latest_undo(&root, &["a".repeat(32), "b".repeat(32)]).is_none());
    }

    #[test]
    fn empty_created_directories_are_removed_deepest_first() {
        let root = root("dirs");
        std::fs::create_dir_all(root.join("src/deep")).expect("dirs");
        std::fs::write(root.join("src/keep.txt"), "x").expect("file");
        prune_empty(&root, &["src".to_owned(), "src/deep".to_owned()]);
        assert!(!root.join("src/deep").exists());
        assert!(root.join("src/keep.txt").exists());
    }

    #[test]
    fn a_held_lock_refuses_a_second_writer_and_releases_on_drop() {
        let root = root("lock");
        let first = WriteLock::acquire(&root).expect("first");
        match WriteLock::acquire(&root) {
            Err(refusal) => assert_eq!(
                refusal
                    .get("status")
                    .and_then(blazingly_json::Value::as_str),
                Some("REPO_BUSY")
            ),
            Ok(lock) => {
                drop(lock);
                panic!("a second writer acquired the lock");
            }
        }
        drop(first);
        assert!(WriteLock::acquire(&root).is_ok());
    }
}
