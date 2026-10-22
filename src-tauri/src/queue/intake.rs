//! Paths dropped or picked for `queue_add`: a file is taken as is, a folder is walked
//! recursively and only its media files are kept (videos and the images of `media::plan`).

use std::path::{Path, PathBuf};

use super::QueueError;
use crate::caps::Caps;
use crate::media::plan;

/// Video containers ffprobe/ffmpeg handle; anything else in a folder is skipped.
const VIDEO_EXTENSIONS: [&str; 15] = [
    "mp4", "m4v", "mov", "mkv", "webm", "avi", "wmv", "flv", "mpg", "mpeg", "ts", "mts", "m2ts",
    "3gp", "ogv",
];

pub fn is_media(path: &Path) -> bool {
    plan::is_image(path)
        || path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| VIDEO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Size and modification time (whole seconds since the epoch, 0 if unknown) of a file, as
/// `items.source_size` and `items.source_mtime` record them; `None` if it is not a file.
pub fn file_stamp(path: &Path) -> Option<(i64, i64)> {
    let metadata = std::fs::metadata(path).ok().filter(|m| m.is_file())?;
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |age| age.as_secs() as i64);
    Some((metadata.len() as i64, mtime))
}

/// A folder in which some files were left out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub folder: String,
    pub count: usize,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Expanded {
    /// In the given order; inside a folder, sorted by name (case-insensitive), depth first.
    pub files: Vec<String>,
    pub skipped: Vec<Skipped>,
}

/// Expands folders. A path that does not exist or cannot be read refuses the whole call
/// (`FILE_UNREADABLE`), as for a single file; so does a folder deeper than `caps.folder_depth`
/// levels or more than `caps.files_per_add` files in all (`FOLDER_TOO_DEEP`, `TOO_MANY_FILES`).
/// Links and junctions inside a folder are not followed (no cycles); hidden entries (`.name`)
/// are skipped silently.
pub fn expand(paths: &[String], caps: &Caps) -> Result<Expanded, QueueError> {
    let mut expanded = Expanded::default();
    for path in paths {
        let unreadable = || QueueError::FileUnreadable(path.clone());
        let metadata = std::fs::metadata(path).map_err(|_| unreadable())?;
        if metadata.is_dir() {
            let mut skipped = 0;
            walk(Path::new(path), caps, &mut expanded.files, &mut skipped).map_err(|error| {
                match error {
                    QueueError::Io(_) => unreadable(),
                    other => other,
                }
            })?;
            if skipped > 0 {
                expanded.skipped.push(Skipped {
                    folder: path.clone(),
                    count: skipped,
                });
            }
        } else {
            too_many(expanded.files.len(), caps)?;
            expanded.files.push(path.clone());
        }
    }
    Ok(expanded)
}

/// `TOO_MANY_FILES` when one more file would pass the cap of an add.
fn too_many(files: usize, caps: &Caps) -> Result<(), QueueError> {
    if files >= caps.files_per_add {
        return Err(QueueError::OverCap {
            code: "TOO_MANY_FILES",
            max: caps.files_per_add,
        });
    }
    Ok(())
}

type Listing = std::vec::IntoIter<(String, PathBuf, std::fs::FileType)>;

/// The entries of `dir`, sorted by name (case-insensitive), hidden ones left out.
fn entries(dir: &Path) -> std::io::Result<Listing> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        entries.push((name.to_lowercase(), entry.path(), entry.file_type()?));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries.into_iter())
}

/// Depth first with a stack of its own, so that no tree can overflow the thread's: one pending
/// listing per level, at most `caps.folder_depth` below `dir`.
fn walk(
    dir: &Path,
    caps: &Caps,
    files: &mut Vec<String>,
    skipped: &mut usize,
) -> Result<(), QueueError> {
    let mut levels = vec![entries(dir)?];
    while let Some(level) = levels.last_mut() {
        let Some((_, path, kind)) = level.next() else {
            levels.pop();
            continue;
        };
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            // `levels.len()` is the depth of the folder about to be listed.
            if levels.len() > caps.folder_depth {
                return Err(QueueError::OverCap {
                    code: "FOLDER_TOO_DEEP",
                    max: caps.folder_depth,
                });
            }
            levels.push(entries(&path)?);
        } else if kind.is_file() && is_media(&path) {
            too_many(files.len(), caps)?;
            files.push(path.to_string_lossy().into_owned());
        } else {
            *skipped += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::test_util::TestDir;

    #[test]
    fn folders_are_walked_recursively_and_filtered() {
        let dir = TestDir::new("intake");
        let root = dir.path().join("drop");
        std::fs::create_dir_all(root.join("sub").join("deeper")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        for name in [
            "b.MP4",
            "A.mkv",
            "notes.txt",
            "Thumbs.db",
            "sub/c.jpg",
            "sub/deeper/d.mov",
            "sub/readme.nfo",
            ".hidden/e.mp4",
        ] {
            std::fs::write(root.join(name), b"x").unwrap();
        }
        let single = dir.file("loose.bin", b"x");

        let expanded = expand(
            &[single.clone(), root.to_string_lossy().into_owned()],
            &Caps::default(),
        )
        .unwrap();
        let names: Vec<String> = expanded
            .files
            .iter()
            .map(|f| {
                Path::new(f)
                    .strip_prefix(dir.path())
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(
            names,
            [
                "loose.bin",
                "drop/A.mkv",
                "drop/b.MP4",
                "drop/sub/c.jpg",
                "drop/sub/deeper/d.mov"
            ],
            "a file given directly is kept whatever its type"
        );
        assert_eq!(
            expanded.skipped,
            [Skipped {
                folder: root.to_string_lossy().into_owned(),
                count: 3
            }]
        );
    }

    #[test]
    fn a_missing_path_refuses_the_call() {
        let dir = TestDir::new("intake-missing");
        let missing = dir.path().join("nope").to_string_lossy().into_owned();
        assert!(matches!(
            expand(std::slice::from_ref(&missing), &Caps::default()),
            Err(QueueError::FileUnreadable(p)) if p == missing
        ));
        let empty = dir.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        assert_eq!(
            expand(&[empty.to_string_lossy().into_owned()], &Caps::default()).unwrap(),
            Expanded::default()
        );
    }

    fn over_cap(result: Result<Expanded, QueueError>) -> (&'static str, usize) {
        match result {
            Err(QueueError::OverCap { code, max }) => (code, max),
            other => panic!("expected a cap refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_tree_deeper_than_the_cap_is_refused_and_the_cap_itself_is_walked() {
        let dir = TestDir::new("intake-depth");
        let caps = Caps {
            folder_depth: 3,
            ..Caps::default()
        };
        // `leaf` sits 3 levels under `root`: allowed; one more level is not.
        let root = dir.path().join("root");
        let leaf = root.join("a").join("b").join("c");
        std::fs::create_dir_all(&leaf).unwrap();
        std::fs::write(leaf.join("deep.mp4"), b"x").unwrap();
        let given = [root.to_string_lossy().into_owned()];
        assert_eq!(expand(&given, &caps).unwrap().files.len(), 1);
        std::fs::create_dir(leaf.join("d")).unwrap();
        assert_eq!(over_cap(expand(&given, &caps)), ("FOLDER_TOO_DEEP", 3));
    }

    #[test]
    fn a_2000_level_tree_is_walked_on_a_small_stack_and_refused_by_the_cap() {
        let dir = TestDir::new("intake-2000");
        let mut deep = dir.path().join("root");
        for _ in 0..2000 {
            deep.push("d");
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("bottom.mp4"), b"x").unwrap();
        let given = [dir.path().join("root").to_string_lossy().into_owned()];
        assert_eq!(
            over_cap(expand(&given, &Caps::default())),
            ("FOLDER_TOO_DEEP", 32)
        );
        // With the cap out of the way, a stack that a recursion of 2000 levels would overflow.
        let caps = Caps {
            folder_depth: 5000,
            ..Caps::default()
        };
        let files = std::thread::Builder::new()
            .stack_size(128 * 1024)
            .spawn(move || expand(&given, &caps).map(|expanded| expanded.files.len()))
            .unwrap()
            .join()
            .expect("no stack overflow")
            .unwrap();
        assert_eq!(files, 1);
    }

    #[test]
    fn more_files_than_the_cap_refuse_the_add_and_the_cap_itself_is_taken() {
        let dir = TestDir::new("intake-count");
        let caps = Caps {
            files_per_add: 4,
            ..Caps::default()
        };
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        for name in ["a.mp4", "b.mp4", "sub/c.mp4", "sub/d.jpg"] {
            std::fs::write(root.join(name), b"x").unwrap();
        }
        let folder = [root.to_string_lossy().into_owned()];
        assert_eq!(expand(&folder, &caps).unwrap().files.len(), 4);
        // Not media: skipped, not counted.
        std::fs::write(root.join("notes.txt"), b"x").unwrap();
        assert_eq!(expand(&folder, &caps).unwrap().files.len(), 4);
        std::fs::write(root.join("e.mov"), b"x").unwrap();
        assert_eq!(over_cap(expand(&folder, &caps)), ("TOO_MANY_FILES", 4));
        // Files given one by one count too.
        let single = dir.file("loose.bin", b"x");
        let one = Caps {
            files_per_add: 1,
            ..Caps::default()
        };
        assert_eq!(
            expand(std::slice::from_ref(&single), &one)
                .unwrap()
                .files
                .len(),
            1
        );
        assert_eq!(
            over_cap(expand(&[single.clone(), single], &one)),
            ("TOO_MANY_FILES", 1)
        );
    }
}
