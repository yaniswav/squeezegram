//! Inno-specific arguments; the updater's default NSIS behavior remains untouched.

use std::path::Path;

use super::UpdateError;

const INNO_FLAG: &str = "inno.flag";

fn invalid(cause: &'static str) -> UpdateError {
    UpdateError::new(UpdateError::CHECK_FAILED, cause)
}

pub(super) fn is_inno(exe_dir: &Path) -> Result<bool, UpdateError> {
    match std::fs::metadata(exe_dir.join(INNO_FLAG)) {
        Ok(metadata) if metadata.is_file() => Ok(true),
        Ok(_) => Err(invalid("installer_marker_invalid")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(invalid("installer_marker_unreadable")),
    }
}

pub(super) fn arguments(exe_dir: &Path, background: bool) -> Result<Vec<String>, UpdateError> {
    let dir = exe_dir
        .to_str()
        .filter(|dir| {
            exe_dir.is_absolute()
                && exe_dir.parent().is_some()
                && !dir.ends_with(['\\', '/'])
                && !dir.chars().any(|c| c == '"' || c.is_control())
        })
        .ok_or_else(|| invalid("installer_path_invalid"))?;
    // The plugin joins these arguments verbatim for ShellExecute, not Command::arg.
    // Roots and trailing separators are rejected rather than guessing quote parsing.
    let mut args = vec![
        "/SILENT".into(),
        "/NORESTART".into(),
        "/SP-".into(),
        "/SQZUPDATE=1".into(),
    ];
    if background {
        args.push("/SQZBACKGROUND=1".into());
    }
    args.push(format!("/DIR=\"{dir}\""));
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::test_util::TestDir;

    #[test]
    fn only_a_file_marks_an_inno_installation() {
        let dir = TestDir::new("update-inno-marker");
        assert!(!is_inno(dir.path()).unwrap());
        assert!(!is_inno(&dir.path().join("missing")).unwrap());
        std::fs::write(dir.path().join(INNO_FLAG), b"").unwrap();
        assert!(is_inno(dir.path()).unwrap());
        let other = TestDir::new("update-inno-marker-dir");
        std::fs::create_dir(other.path().join(INNO_FLAG)).unwrap();
        assert_eq!(
            is_inno(other.path()).unwrap_err().cause,
            "installer_marker_invalid"
        );
    }

    #[test]
    fn metadata_errors_other_than_absence_do_not_fall_back_to_nsis() {
        let dir = TestDir::new("update-inno-marker-error");
        assert_eq!(
            is_inno(&dir.path().join("invalid\0path"))
                .unwrap_err()
                .cause,
            "installer_marker_unreadable"
        );
    }

    #[test]
    fn relative_paths_and_command_line_injections_are_rejected() {
        for dir in ["", "squeezegram", "C:relative", "\\rooted"] {
            assert_eq!(
                arguments(Path::new(dir), false).unwrap_err().cause,
                "installer_path_invalid"
            );
        }
        let base = TestDir::new("update-inno-invalid-path");
        for tail in [
            "bad\" /SQZBACKGROUND=1",
            "bad\npath",
            "bad\rpath",
            "bad\tpath",
            "bad\0path",
        ] {
            assert!(arguments(&base.path().join(tail), false).is_err());
        }
    }

    #[test]
    fn paths_with_spaces_and_unicode_remain_one_quoted_argument() {
        let base = TestDir::new("update-inno-path");
        let path = base.path().join("Programmes été 日本語");
        assert_eq!(
            arguments(&path, false).unwrap(),
            [
                "/SILENT",
                "/NORESTART",
                "/SP-",
                "/SQZUPDATE=1",
                &format!("/DIR=\"{}\"", path.display()),
            ]
        );
    }

    #[test]
    fn background_is_an_explicit_flag_not_an_arbitrary_argument_tail() {
        let base = TestDir::new("update-inno-background");
        let args = arguments(base.path(), true).unwrap();
        assert_eq!(
            &args[..5],
            [
                "/SILENT",
                "/NORESTART",
                "/SP-",
                "/SQZUPDATE=1",
                "/SQZBACKGROUND=1"
            ]
        );
        assert_eq!(args.len(), 6);
    }

    #[test]
    fn roots_and_trailing_separators_are_rejected() {
        let base = TestDir::new("update-inno-trailing-separator");
        let path = format!("{}{}", base.path().display(), std::path::MAIN_SEPARATOR);
        assert!(arguments(Path::new(&path), false).is_err());
        #[cfg(windows)]
        for root in ["C:\\", "\\\\server\\share"] {
            assert!(arguments(Path::new(root), false).is_err());
        }
    }

    #[cfg(windows)]
    #[test]
    fn non_unicode_windows_paths_are_rejected() {
        use std::os::windows::ffi::OsStringExt;
        let path = std::ffi::OsString::from_wide(&[b'C' as u16, b':' as u16, b'\\' as u16, 0xd800]);
        assert!(arguments(Path::new(&path), false).is_err());
    }
}
