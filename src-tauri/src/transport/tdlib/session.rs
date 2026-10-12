//! Reset of the local TDLib session of one identity: what the user can do when the session
//! cannot be opened any more (DPAPI refuses `key.bin` after a password reset or a profile
//! migration, a copy of the app under another Windows account, a damaged database).
//!
//! Only the session's own entries are removed, inside the folder `TdlibConfig::dir` names:
//! `db\`, `files\`, `key.bin` (and the `key.bin.tmp` a crash may leave). Everything else stays:
//! `api_hash.bin`, `proxy_secret.bin`, `bin\tdjson.dll`, the TDLib log and the other identities'
//! `custom-<api_id>\` folders, which live beside the product identity's session. The queue,
//! history and settings are elsewhere and never touched.
//!
//! Links are never followed: a session folder that is itself a symbolic link or a junction is
//! refused, and an entry that is one is removed as a link, leaving its target alone.

use std::fs;
use std::io;
use std::path::Path;

use super::key::KEY_FILE;
use crate::transport::TransportError;

/// The entries removed, in this order: the database first, the key last, so an interrupted
/// reset leaves at worst a key without a database (a fresh start) and can simply be redone.
pub const SESSION_ENTRIES: [&str; 4] = ["db", "files", "key.bin.tmp", KEY_FILE];

/// Removes the session entries of `dir` and returns the names that existed. A missing folder
/// is an empty session.
pub fn reset(dir: &Path) -> Result<Vec<&'static str>, TransportError> {
    match fs::symlink_metadata(dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(io_error("session folder", &error)),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(TransportError::Protocol(
                "session folder is a link or not a folder: not reset".into(),
            ));
        }
        Ok(_) => {}
    }
    let mut removed = Vec::new();
    for name in SESSION_ENTRIES {
        let path = dir.join(name);
        let file_type = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata.file_type(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(io_error(name, &error)),
        };
        let result = if file_type.is_symlink() {
            remove_link(&path)
        } else if file_type.is_dir() {
            // Does not follow links inside the tree: a link is removed, never its target.
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        result.map_err(|error| io_error(name, &error))?;
        removed.push(name);
    }
    Ok(removed)
}

/// A symbolic link or junction itself, not what it points to.
fn remove_link(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt;
        // A directory link (symbolic link or junction) is removed like an empty folder.
        if fs::symlink_metadata(path)?.file_type().is_symlink_dir() {
            return fs::remove_dir(path);
        }
    }
    fs::remove_file(path)
}

/// The entry and the kind of failure; the path may hold the user name, the page needs neither.
fn io_error(what: &str, error: &io::Error) -> TransportError {
    TransportError::Protocol(format!("session reset, {what}: {}", error.kind()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::tdlib::tests::TempDir;
    use std::path::PathBuf;

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// A product-identity TDLib folder as an install has it: the session at the root, beside
    /// the files that are not part of it and a personal identity's own session.
    fn populated(root: &Path) {
        write(&root.join("db").join("td.binlog"), "db");
        write(&root.join("db").join("nested").join("x"), "db");
        write(&root.join("files").join("photos").join("p.jpg"), "file");
        write(&root.join(KEY_FILE), "key");
        write(&root.join("key.bin.tmp"), "half key");
        write(&root.join("api_hash.bin"), "hash");
        write(&root.join("proxy_secret.bin"), "secret");
        write(&root.join("bin").join("tdjson.dll"), "dll");
        write(&root.join("tdlib.log"), "log");
        write(
            &root.join("custom-123").join("db").join("td.binlog"),
            "other",
        );
        write(&root.join("custom-123").join(KEY_FILE), "other key");
    }

    fn kept(root: &Path) -> Vec<PathBuf> {
        [
            root.join("api_hash.bin"),
            root.join("proxy_secret.bin"),
            root.join("bin").join("tdjson.dll"),
            root.join("tdlib.log"),
        ]
        .into()
    }

    #[test]
    fn the_product_session_goes_and_everything_beside_it_stays() {
        let dir = TempDir::new("session-product");
        let root = dir.path().join("tdlib");
        populated(&root);
        let removed = reset(&root).unwrap();
        assert_eq!(removed, SESSION_ENTRIES);
        for name in SESSION_ENTRIES {
            assert!(!root.join(name).exists(), "{name} still there");
        }
        for path in kept(&root) {
            assert!(path.is_file(), "{} was removed", path.display());
        }
        // The personal identity's session is another session: untouched.
        assert!(
            root.join("custom-123")
                .join("db")
                .join("td.binlog")
                .is_file()
        );
        assert!(root.join("custom-123").join(KEY_FILE).is_file());
        // Again: nothing left to remove, no error.
        assert!(reset(&root).unwrap().is_empty());
    }

    #[test]
    fn a_personal_session_leaves_the_product_session_alone() {
        let dir = TempDir::new("session-personal");
        let root = dir.path().join("tdlib");
        populated(&root);
        let personal = root.join("custom-123");
        assert_eq!(reset(&personal).unwrap(), ["db", KEY_FILE]);
        assert!(personal.is_dir() && !personal.join("db").exists());
        for name in SESSION_ENTRIES {
            assert!(root.join(name).exists(), "product {name} was removed");
        }
        for path in kept(&root) {
            assert!(path.is_file());
        }
    }

    #[test]
    fn a_missing_folder_is_an_empty_session() {
        let dir = TempDir::new("session-missing");
        assert!(reset(&dir.path().join("nowhere")).unwrap().is_empty());
        // A file where the folder should be is not a session.
        let file = dir.path().join("file");
        write(&file, "x");
        assert!(reset(&file).is_err());
        assert!(file.is_file());
    }

    /// `mklink /J` needs no privilege, unlike a symbolic link.
    #[cfg(windows)]
    fn junction(link: &Path, target: &Path) {
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "mklink /J failed");
    }

    #[cfg(windows)]
    #[test]
    fn links_are_removed_as_links_never_followed() {
        let dir = TempDir::new("session-links");
        let outside = dir.path().join("outside");
        write(&outside.join("precious.txt"), "keep me");
        let root = dir.path().join("tdlib");
        write(&root.join(KEY_FILE), "key");
        junction(&root.join("db"), &outside);
        assert!(root.join("db").join("precious.txt").is_file());

        assert_eq!(reset(&root).unwrap(), ["db", KEY_FILE]);
        assert!(
            fs::symlink_metadata(root.join("db")).is_err(),
            "link removed"
        );
        assert_eq!(
            fs::read_to_string(outside.join("precious.txt")).unwrap(),
            "keep me"
        );

        // A session folder that is itself a junction is refused outright.
        let real = dir.path().join("real");
        write(&real.join(KEY_FILE), "key");
        write(&real.join("db").join("td.binlog"), "db");
        let linked = dir.path().join("linked");
        junction(&linked, &real);
        assert!(reset(&linked).is_err());
        assert!(real.join(KEY_FILE).is_file() && real.join("db").join("td.binlog").is_file());
        fs::remove_dir(&linked).unwrap();
    }
}
