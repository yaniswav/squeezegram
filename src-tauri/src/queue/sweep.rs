//! Startup sweep of orphan temp folders (F15: encoded files are kept for a retry, so nothing
//! else may be left behind).
//!
//! Every item encodes in `<temp root>\<item id>`. Recovery and `clear` delete the files the
//! items point to; a crash, a removed item or a changed setting can still leave a folder that no
//! item references. This removes those, and nothing else:
//!
//! - only in the app's default root: a folder the user picked is never swept;
//! - only direct children of the root that are folders named like an item id (digits): the
//!   root may be a folder the user picked, and `detect` or anything the user keeps there is not
//!   ours;
//! - never a folder an item points to (`source_path`, `temp_path` or `output_path` inside it),
//!   except that an EMPTY one is removed when only finished (`sent`, `cancelled`) items point
//!   to it: its files were deleted with the send or the cancel, and nothing else would ever
//!   remove it while the row exists;
//! - never through a symlink or a junction (a reparse point on Windows): they are skipped, so
//!   a link can not lead the deletion outside the root;
//! - never the root itself, and only files directly inside a folder (the layout the encoder
//!   writes); anything else makes the folder skipped;
//! - what cannot be deleted is skipped, never an error.

use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};

use super::State;
use super::store::Store;

/// What a sweep did, for the journal line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Sweep {
    /// Orphan folders deleted.
    pub removed: usize,
    /// Entries left on purpose: referenced by an item, or not ours.
    pub kept: usize,
    /// Entries that should go but were not touched or not fully deleted (links, locked files).
    pub skipped: usize,
}

/// The paths the items point to: those of items that may still need them, and those of items
/// that are over (`sent`, `cancelled`).
#[derive(Debug, Default)]
struct Referenced {
    active: Vec<PathBuf>,
    finished: Vec<PathBuf>,
}

/// Every path the items point to, in any state; `None` when the items can not be read.
fn referenced_paths(store: &Store) -> Option<Referenced> {
    let items = store.list_items().ok()?;
    let mut referenced = Referenced::default();
    for item in items {
        let list = if matches!(item.state, State::Sent | State::Cancelled) {
            &mut referenced.finished
        } else {
            &mut referenced.active
        };
        list.extend(
            [Some(item.source_path), item.temp_path, item.output_path]
                .into_iter()
                .flatten()
                .map(PathBuf::from),
        );
    }
    Some(referenced)
}

/// Sweeps the app's default temp root. A folder the user picked is never swept: it may hold
/// their own digit-named folders of loose files (a "2024" folder of photos looks exactly like an
/// item folder), and deleting those would destroy their files.
pub fn sweep_default_root(temp: &crate::settings::Temp, store: &Store) -> Sweep {
    if temp.dir.as_ref().is_some_and(|dir| !dir.trim().is_empty()) {
        tracing::info!(
            event = "temp_sweep_skipped",
            reason = "custom_root",
            "temp folder chosen by the user, not swept"
        );
        return Sweep::default();
    }
    sweep_orphans(&crate::media::stage::temp_root(temp), store)
}

/// Sweeps `root` against the items of `store` and journals one line with the counts. Items
/// that can not be read sweep nothing: without them every folder would look orphaned.
pub fn sweep_orphans(root: &Path, store: &Store) -> Sweep {
    let Some(referenced) = referenced_paths(store) else {
        tracing::warn!(
            event = "temp_sweep_skipped",
            "items unreadable, no temp folder swept"
        );
        return Sweep::default();
    };
    let outcome = sweep_with(root, &referenced.active, &referenced.finished);
    tracing::info!(
        event = "temp_sweep",
        removed = outcome.removed,
        kept = outcome.kept,
        skipped = outcome.skipped,
        "orphan temp folders swept"
    );
    outcome
}

/// The sweep itself, with the paths of the items that may still need them.
pub fn sweep(root: &Path, referenced: &[PathBuf]) -> Sweep {
    sweep_with(root, referenced, &[])
}

/// `sweep`, with the paths of finished items too: a folder only they point to goes when it is
/// empty, never when something is left in it.
fn sweep_with(root: &Path, referenced: &[PathBuf], finished: &[PathBuf]) -> Sweep {
    let mut outcome = Sweep::default();
    // A root that is a link is not walked: it could point anywhere.
    match fs::symlink_metadata(root) {
        Ok(meta) if meta.is_dir() && !is_link(&meta) => {}
        _ => return outcome,
    }
    let Ok(entries) = fs::read_dir(root) else {
        return outcome;
    };
    let referenced: Vec<String> = referenced.iter().map(|path| normalize(path)).collect();
    let finished: Vec<String> = finished.iter().map(|path| normalize(path)).collect();
    let points_into = |paths: &[String], own: &str| {
        paths
            .iter()
            .any(|used| used == own || used.starts_with(&format!("{own}\\")))
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            outcome.skipped += 1;
            continue;
        };
        if !(meta.is_dir() || is_link(&meta)) || !is_item_folder_name(&entry.file_name()) {
            outcome.kept += 1;
            continue;
        }
        if is_link(&meta) {
            outcome.skipped += 1;
            continue;
        }
        let own = normalize(&path);
        if points_into(&referenced, &own) {
            outcome.kept += 1;
            continue;
        }
        if points_into(&finished, &own) {
            // Only items that are over point here: an empty folder is a leftover, anything
            // else is not ours to judge.
            if fs::remove_dir(&path).is_ok() {
                outcome.removed += 1;
            } else {
                outcome.kept += 1;
            }
            continue;
        }
        if remove_flat_folder(&path) {
            outcome.removed += 1;
        } else {
            outcome.skipped += 1;
        }
    }
    outcome
}

/// After a file of an item was deleted: its folder (`<root>\<id>`) goes too when nothing is
/// left in it, so a finished item does not leave an empty folder behind for as long as its row
/// exists. Only a folder named like an item id, never a link, and `remove_dir` itself refuses
/// a folder that holds anything.
pub(super) fn remove_empty_item_folder(file: &Path) {
    let Some(dir) = file.parent() else { return };
    if !dir.file_name().is_some_and(is_item_folder_name) {
        return;
    }
    if matches!(fs::symlink_metadata(dir), Ok(meta) if meta.is_dir() && !is_link(&meta)) {
        let _ = fs::remove_dir(dir);
    }
}

/// Folders the encoder makes are named by the item id.
fn is_item_folder_name(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit())
}

/// Deletes the files of a folder, then the folder. A subfolder or a link inside means the
/// folder is not what the encoder wrote: nothing is deleted. `false` when anything is left.
fn remove_flat_folder(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    let mut files = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { return false };
        let path = entry.path();
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() && !is_link(&meta) => files.push(path),
            _ => return false,
        }
    }
    let mut all_gone = true;
    for file in files {
        if let Err(error) = fs::remove_file(&file)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                event = "file_delete_failed",
                path = %file.display(),
                error = %error,
                "could not delete an orphan temp file"
            );
            all_gone = false;
        }
    }
    all_gone && fs::remove_dir(dir).is_ok()
}

/// A symlink, or on Windows any reparse point (junction, mount point, cloud placeholder).
fn is_link(meta: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

/// Case-insensitive, backslash-separated, no trailing separator: Windows paths compare so.
fn normalize(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::test_util::{TestDir, add_item, memory_store};

    fn folder_with(root: &Path, name: &str, files: &[&str]) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        for file in files {
            fs::write(dir.join(file), b"x").unwrap();
        }
        dir
    }

    #[test]
    fn unreferenced_item_folders_go_and_everything_else_stays() {
        let dir = TestDir::new("sweep");
        let root = dir.path().join("tmp");
        let used = folder_with(&root, "1", &["a.mp4.part", "a.mp4"]);
        let orphan = folder_with(&root, "2", &["b.mp4.part", "b.mp4"]);
        let empty = folder_with(&root, "30", &[]);
        let foreign = folder_with(&root, "detect", &["probe.mp4"]);
        let year = folder_with(&root, "2024", &["photo.jpg"]);
        fs::create_dir(year.join("nested")).unwrap();
        fs::write(root.join("7.part"), b"x").unwrap();

        let outcome = sweep(&root, &[used.join("a.mp4")]);

        assert_eq!(
            outcome,
            Sweep {
                removed: 2,
                kept: 3,
                skipped: 1
            }
        );
        assert!(used.join("a.mp4.part").exists() && used.join("a.mp4").exists());
        assert!(!orphan.exists() && !empty.exists());
        assert!(foreign.join("probe.mp4").exists());
        assert!(year.join("photo.jpg").exists(), "not the flat layout");
        assert!(root.join("7.part").exists(), "loose files are not ours");
        assert!(root.exists(), "the root is never deleted");

        assert_eq!(sweep(&root, &[used.join("a.mp4")]).removed, 0, "idempotent");
    }

    #[test]
    fn an_empty_folder_of_a_finished_item_goes_but_never_a_non_empty_one() {
        let dir = TestDir::new("sweep-finished");
        let root = dir.path().join("tmp");
        let sent = folder_with(&root, "1", &[]);
        let cancelled = folder_with(&root, "2", &[]);
        let leftover = folder_with(&root, "3", &["thumb.jpg"]);
        let active = folder_with(&root, "4", &[]);
        let both = folder_with(&root, "5", &[]);

        let outcome = sweep_with(
            &root,
            &[active.join("a_sq.mp4"), both.join("b_sq.mp4")],
            &[
                sent.join("a_sq.mp4"),
                cancelled.join("a.part"),
                leftover.join("c_sq.mp4"),
                both.join("b_sq.mp4"),
            ],
        );

        assert_eq!(
            outcome,
            Sweep {
                removed: 2,
                kept: 3,
                skipped: 0
            }
        );
        assert!(!sent.exists() && !cancelled.exists());
        assert!(leftover.join("thumb.jpg").exists(), "not empty: kept");
        assert!(
            active.exists(),
            "an item that may still run keeps its folder"
        );
        assert!(both.exists(), "one active item is enough to keep it");
    }

    #[test]
    fn the_sweep_removes_the_empty_folder_of_a_sent_item_from_the_store() {
        let dir = TestDir::new("sweep-sent-row");
        let root = dir.path().join("tmp");
        let folder = folder_with(&root, "1", &[]);
        let mut store = memory_store();
        let id = add_item(&mut store, r"C:\videos\a.mp4");
        store
            .conn()
            .execute(
                "UPDATE items SET state = 'sent', output_path = ?1 WHERE id = ?2",
                rusqlite::params![folder.join("a_sq.mp4").to_string_lossy(), id],
            )
            .unwrap();

        let outcome = sweep_orphans(&root, &store);

        assert_eq!(outcome.removed, 1);
        assert!(!folder.exists());
    }

    #[test]
    fn discarding_the_files_of_a_sent_item_leaves_no_folder_behind() {
        let dir = TestDir::new("sweep-discard");
        let root = dir.path().join("tmp");
        let folder = folder_with(&root, "7", &["a_sq.mp4"]);
        let mut store = memory_store();
        let id = add_item(&mut store, r"C:\videos\a.mp4");
        store
            .conn()
            .execute(
                "UPDATE items SET state = 'sent', output_path = ?1 WHERE id = ?2",
                rusqlite::params![folder.join("a_sq.mp4").to_string_lossy(), id],
            )
            .unwrap();

        store.discard_files(&store.get_item(id).unwrap()).unwrap();

        assert!(!folder.exists(), "the file and its now empty folder");
        assert!(root.exists());
    }

    #[test]
    fn deleting_the_last_file_of_an_item_folder_removes_the_folder() {
        let dir = TestDir::new("sweep-after-delete");
        let root = dir.path().join("tmp");
        let item = folder_with(&root, "12", &["a_sq.mp4", "thumb.jpg"]);
        let not_an_item = folder_with(&root, "clips", &["a.mp4"]);

        fs::remove_file(item.join("a_sq.mp4")).unwrap();
        remove_empty_item_folder(&item.join("a_sq.mp4"));
        assert!(item.join("thumb.jpg").exists(), "something left: kept");

        fs::remove_file(item.join("thumb.jpg")).unwrap();
        remove_empty_item_folder(&item.join("thumb.jpg"));
        assert!(!item.exists());
        assert!(root.exists(), "never the root");

        // A folder that is not named like an item id is not ours, even empty.
        fs::remove_file(not_an_item.join("a.mp4")).unwrap();
        remove_empty_item_folder(&not_an_item.join("a.mp4"));
        assert!(not_an_item.exists());
    }

    #[test]
    fn a_folder_holding_a_source_is_kept_and_paths_compare_like_windows() {
        let dir = TestDir::new("sweep-source");
        let root = dir.path().join("tmp");
        let folder = folder_with(&root, "5", &["clip.mp4"]);
        let shouting = PathBuf::from(folder.join("clip.mp4").to_string_lossy().to_uppercase())
            .to_string_lossy()
            .replace('\\', "/");

        assert_eq!(sweep(&root, &[PathBuf::from(shouting)]).removed, 0);
        assert!(folder.join("clip.mp4").exists());
        // "5" does not protect "50": the match is per path component.
        let other = folder_with(&root, "50", &["x"]);
        assert_eq!(sweep(&root, &[folder.join("clip.mp4")]).removed, 1);
        assert!(!other.exists());
    }

    #[test]
    fn a_missing_root_or_a_file_as_root_is_left_alone() {
        let dir = TestDir::new("sweep-root");
        assert_eq!(sweep(&dir.path().join("missing"), &[]), Sweep::default());
        let file = dir.file("root.txt", b"x");
        assert_eq!(sweep(Path::new(&file), &[]), Sweep::default());
        assert!(Path::new(&file).exists());
    }

    #[test]
    fn the_sweep_reads_the_paths_the_items_point_to() {
        let dir = TestDir::new("sweep-store");
        let root = dir.path().join("tmp");
        let kept = folder_with(&root, "1", &["a_sq.mp4"]);
        let orphan = folder_with(&root, "9", &["old.part"]);
        let mut store = memory_store();
        let id = add_item(&mut store, "C:\\videos\\a.mp4");
        store
            .conn()
            .execute(
                "UPDATE items SET output_path = ?1 WHERE id = ?2",
                rusqlite::params![kept.join("a_sq.mp4").to_string_lossy(), id],
            )
            .unwrap();

        let outcome = sweep_orphans(&root, &store);

        assert_eq!((outcome.removed, outcome.kept), (1, 1));
        assert!(kept.join("a_sq.mp4").exists() && !orphan.exists());
    }

    #[test]
    fn a_temp_folder_chosen_by_the_user_is_never_swept() {
        let dir = TestDir::new("sweep-custom");
        let root = dir.path().join("Videos");
        let year = folder_with(&root, "2024", &["holiday.mp4"]);
        let temp = crate::settings::Temp {
            dir: Some(root.to_string_lossy().into_owned()),
            ..Default::default()
        };

        assert_eq!(sweep_default_root(&temp, &memory_store()), Sweep::default());
        assert!(year.join("holiday.mp4").exists());
    }

    /// A junction needs no privilege on Windows; the test is skipped where `mklink` fails.
    #[cfg(windows)]
    #[test]
    fn a_junction_is_never_followed_nor_deleted() {
        let dir = TestDir::new("sweep-junction");
        let root = dir.path().join("tmp");
        fs::create_dir_all(&root).unwrap();
        let outside = folder_with(dir.path(), "outside", &["precious.txt"]);
        let link = root.join("3");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&outside)
            .output();
        if !made.is_ok_and(|out| out.status.success()) {
            return;
        }
        // A folder named like an item whose own content is a junction is refused too.
        let inner = folder_with(&root, "4", &[]);
        let made_inner = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(inner.join("link"))
            .arg(&outside)
            .output();
        if !made_inner.is_ok_and(|out| out.status.success()) {
            let _ = fs::remove_dir(&link);
            return;
        }

        let outcome = sweep(&root, &[]);

        assert!(outside.join("precious.txt").exists());
        assert!(link.exists(), "the link itself is left alone");
        assert_eq!(outcome.removed, 0);
        assert_eq!(outcome.skipped, 2, "the link and the folder holding a link");
        // Detach the junctions so the test directory can be removed without touching `outside`.
        let _ = fs::remove_dir(&link);
        let _ = fs::remove_dir(inner.join("link"));
    }
}
