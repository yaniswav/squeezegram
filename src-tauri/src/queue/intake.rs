//! Paths dropped or picked for `queue_add`: a file is taken as is, a folder is walked
//! recursively and only its media files are kept (videos and the images of `media::plan`).

use std::path::{Path, PathBuf};

use super::QueueError;
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
/// (`FILE_UNREADABLE`), as for a single file. Links and junctions inside a folder are not
/// followed (no cycles); hidden entries (`.name`) are skipped silently.
pub fn expand(paths: &[String]) -> Result<Expanded, QueueError> {
    let mut expanded = Expanded::default();
    for path in paths {
        let unreadable = || QueueError::FileUnreadable(path.clone());
        let metadata = std::fs::metadata(path).map_err(|_| unreadable())?;
        if metadata.is_dir() {
            let mut skipped = 0;
            walk(Path::new(path), &mut expanded.files, &mut skipped).map_err(|_| unreadable())?;
            if skipped > 0 {
                expanded.skipped.push(Skipped {
                    folder: path.clone(),
                    count: skipped,
                });
            }
        } else {
            expanded.files.push(path.clone());
        }
    }
    Ok(expanded)
}

fn walk(dir: &Path, files: &mut Vec<String>, skipped: &mut usize) -> std::io::Result<()> {
    let mut entries: Vec<(String, PathBuf, std::fs::FileType)> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        entries.push((name.to_lowercase(), entry.path(), entry.file_type()?));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    for (_, path, kind) in entries {
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            walk(&path, files, skipped)?;
        } else if kind.is_file() && is_media(&path) {
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

        let expanded = expand(&[single.clone(), root.to_string_lossy().into_owned()]).unwrap();
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
            expand(std::slice::from_ref(&missing)),
            Err(QueueError::FileUnreadable(p)) if p == missing
        ));
        let empty = dir.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        assert_eq!(
            expand(&[empty.to_string_lossy().into_owned()]).unwrap(),
            Expanded::default()
        );
    }
}
