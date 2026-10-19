//! Validate custom tools before executing them, including previously recorded folders.

use std::path::{Path, PathBuf};

use super::SidecarError;

/// Both paths must resolve inside the profile; a link into a shared folder is not exempt.
pub(super) fn in_profile(ffmpeg: &Path, ffprobe: &Path, profile: Option<&Path>) -> bool {
    let Some(profile) = profile.filter(|p| p.is_absolute()) else {
        return false;
    };
    let Ok(profile) = std::fs::canonicalize(profile) else {
        return false;
    };
    let inside = |path: &Path| {
        let mut components = path.components();
        profile.components().all(|component| {
            components.next().is_some_and(|part| {
                part.as_os_str()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&component.as_os_str().to_string_lossy())
            })
        })
    };
    inside(ffmpeg) && inside(ffprobe)
}

pub(super) fn check_pair(
    ffmpeg: &Path,
    ffprobe: &Path,
) -> Result<(PathBuf, PathBuf), SidecarError> {
    check_pair_with_profile(ffmpeg, ffprobe, true)
}

fn check_pair_with_profile(
    ffmpeg: &Path,
    ffprobe: &Path,
    allow_profile: bool,
) -> Result<(PathBuf, PathBuf), SidecarError> {
    let ffmpeg = std::fs::canonicalize(ffmpeg).map_err(|_| SidecarError::UntrustedFolder)?;
    let ffprobe = std::fs::canonicalize(ffprobe).map_err(|_| SidecarError::UntrustedFolder)?;
    let profile = std::env::var_os("USERPROFILE").map(PathBuf::from);
    if allow_profile && in_profile(&ffmpeg, &ffprobe, profile.as_deref()) {
        return Ok((ffmpeg, ffprobe));
    }
    for exe in [&ffmpeg, &ffprobe] {
        check_permissions(exe, false)?;
        let dir = exe.parent().ok_or(SidecarError::UntrustedFolder)?;
        check_permissions(dir, true)?;
        check_permissions(dir.parent().ok_or(SidecarError::UntrustedFolder)?, true)?;
    }
    Ok((ffmpeg, ffprobe))
}

#[cfg(windows)]
fn unsafe_rights(rights: u32, directory: bool) -> bool {
    use windows::Win32::Storage::FileSystem::{
        DELETE, FILE_ADD_FILE, FILE_APPEND_DATA, FILE_DELETE_CHILD, FILE_WRITE_DATA, WRITE_DAC,
        WRITE_OWNER,
    };
    // Generic access can appear in an ACL too: reject generic write/all without relying on
    // the ACL API to map them to file-specific rights.
    let generic_write_or_all = 0x4000_0000 | 0x1000_0000;
    let writes = if directory {
        FILE_ADD_FILE.0 | FILE_DELETE_CHILD.0
    } else {
        FILE_WRITE_DATA.0 | FILE_APPEND_DATA.0 | DELETE.0
    };
    rights & (writes | WRITE_DAC.0 | WRITE_OWNER.0 | generic_write_or_all) != 0
}

#[cfg(windows)]
fn check_permissions(path: &Path, directory: bool) -> Result<(), SidecarError> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        BuildTrusteeWithSidW, GetEffectiveRightsFromAclW, GetNamedSecurityInfoW, SE_FILE_OBJECT,
        TRUSTEE_W,
    };
    use windows::Win32::Security::{
        CreateWellKnownSid, DACL_SECURITY_INFORMATION, EqualSid, OWNER_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID, SECURITY_MAX_SID_SIZE, WinAuthenticatedUserSid,
        WinBuiltinUsersSid, WinInteractiveSid, WinWorldSid,
    };
    use windows::core::PCWSTR;

    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    let mut dacl = std::ptr::null_mut();
    let mut owner = PSID::default();
    // The descriptor owns the ACL and owner SID until LocalFree below. Every error, including
    // an unreadable or null DACL, refuses execution.
    unsafe {
        let result = (|| {
            GetNamedSecurityInfoW(
                PCWSTR(name.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | OWNER_SECURITY_INFORMATION,
                Some(&mut owner),
                None,
                Some(&mut dacl),
                None,
                &mut descriptor,
            )
            .ok()
            .map_err(|_| SidecarError::UntrustedFolder)?;
            if dacl.is_null() || owner.is_invalid() {
                return Err(SidecarError::UntrustedFolder);
            }
            for kind in [
                WinWorldSid,
                WinAuthenticatedUserSid,
                WinBuiltinUsersSid,
                WinInteractiveSid,
            ] {
                let mut buffer = [0u32; SECURITY_MAX_SID_SIZE as usize / 4];
                let sid = PSID(buffer.as_mut_ptr().cast());
                let mut size = SECURITY_MAX_SID_SIZE;
                CreateWellKnownSid(kind, None, Some(sid), &mut size)
                    .map_err(|_| SidecarError::UntrustedFolder)?;
                // An owner can change the DACL even without an explicit WRITE_DAC grant.
                if EqualSid(owner, sid).is_ok() {
                    return Err(SidecarError::UntrustedFolder);
                }
                let mut trustee = TRUSTEE_W::default();
                BuildTrusteeWithSidW(&mut trustee, Some(sid));
                let mut rights = 0;
                GetEffectiveRightsFromAclW(dacl, &trustee, &mut rights)
                    .ok()
                    .map_err(|_| SidecarError::UntrustedFolder)?;
                if unsafe_rights(rights, directory) {
                    return Err(SidecarError::UntrustedFolder);
                }
            }
            Ok(())
        })();
        if !descriptor.0.is_null() {
            let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        }
        result
    }
}

#[cfg(not(windows))]
fn check_permissions(_path: &Path, _directory: bool) -> Result<(), SidecarError> {
    Err(SidecarError::UntrustedFolder)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::test_util::TestDir;

    #[test]
    fn profile_shortcut_requires_both_resolved_files_and_a_component_boundary() {
        let dir = TestDir::new("sidecar-profile");
        let profile = dir.path().join("user");
        let other = dir.path().join("user-other");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let profile = std::fs::canonicalize(profile).unwrap();
        let other = std::fs::canonicalize(other).unwrap();
        let ffmpeg = profile.join("ffmpeg.exe");
        let ffprobe = profile.join("ffprobe.exe");
        assert!(in_profile(&ffmpeg, &ffprobe, Some(&profile)));
        assert!(!in_profile(
            &ffmpeg,
            &other.join("ffprobe.exe"),
            Some(&profile)
        ));
        assert!(!in_profile(&ffmpeg, &ffprobe, None));
        assert!(!in_profile(&ffmpeg, &ffprobe, Some(Path::new("relative"))));
    }

    #[cfg(windows)]
    #[test]
    fn resolved_junction_targets_decide_the_profile_shortcut() {
        use std::os::windows::process::CommandExt;
        let dir = TestDir::new("sidecar-junction");
        let profile = dir.path().join("profile");
        let shared = dir.path().join("shared");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::create_dir_all(&shared).unwrap();
        for name in ["ffmpeg.exe", "ffprobe.exe"] {
            std::fs::write(shared.join(name), b"not executable").unwrap();
        }
        let link = profile.join("tools");
        let output = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&link)
            .arg(&shared)
            .creation_flags(0x0800_0000)
            .output()
            .unwrap();
        assert!(output.status.success(), "junction creation failed");
        let ffmpeg = std::fs::canonicalize(link.join("ffmpeg.exe")).unwrap();
        let ffprobe = std::fs::canonicalize(link.join("ffprobe.exe")).unwrap();
        assert!(!in_profile(&ffmpeg, &ffprobe, Some(&profile)));
        assert!(in_profile(&ffmpeg, &ffprobe, Some(&shared)));
        assert_eq!(
            check_pair(&link.join("ffmpeg.exe"), &link.join("ffprobe.exe")).unwrap(),
            (ffmpeg, ffprobe)
        );
    }

    #[cfg(windows)]
    #[test]
    fn rights_masks_reject_each_mutation_right_and_allow_read_execute() {
        use windows::Win32::Storage::FileSystem::*;
        for right in [
            FILE_WRITE_DATA,
            FILE_APPEND_DATA,
            DELETE,
            WRITE_DAC,
            WRITE_OWNER,
        ] {
            assert!(unsafe_rights(right.0, false), "file: {right:?}");
        }
        for right in [FILE_ADD_FILE, FILE_DELETE_CHILD, WRITE_DAC, WRITE_OWNER] {
            assert!(unsafe_rights(right.0, true), "directory: {right:?}");
        }
        for directory in [false, true] {
            assert!(!unsafe_rights(0, directory));
            assert!(!unsafe_rights(
                FILE_GENERIC_READ.0 | FILE_GENERIC_EXECUTE.0,
                directory
            ));
            assert!(unsafe_rights(FILE_ALL_ACCESS.0, directory));
            assert!(unsafe_rights(0x4000_0000, directory));
            assert!(unsafe_rights(0x1000_0000, directory));
        }
        assert!(!unsafe_rights(FILE_ADD_SUBDIRECTORY.0, true));
        assert!(unsafe_rights(FILE_DELETE_CHILD.0, true));
        assert!(!unsafe_rights(FILE_DELETE_CHILD.0, false));
    }

    #[cfg(windows)]
    #[test]
    fn shared_write_permissions_are_refused_without_executing_dummy_tools() {
        use std::os::windows::process::CommandExt;
        let dir = TestDir::new("sidecar-acl");
        let parent = dir.path().join("parent");
        let bin = parent.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let ffmpeg = bin.join("ffmpeg.exe");
        let ffprobe = bin.join("ffprobe.exe");
        std::fs::write(&ffmpeg, b"not executable").unwrap();
        std::fs::write(&ffprobe, b"not executable").unwrap();
        assert!(check_pair_with_profile(&ffmpeg, &ffprobe, false).is_ok());
        let output = std::process::Command::new("icacls.exe")
            .arg(&parent)
            .args(["/grant", "*S-1-5-11:(OI)(CI)M"])
            .creation_flags(0x0800_0000)
            .output()
            .unwrap();
        assert!(output.status.success(), "icacls failed");
        let error = check_pair_with_profile(&ffmpeg, &ffprobe, false).unwrap_err();
        assert_eq!(error.code(), "SIDECAR_UNTRUSTED_FOLDER");
        for (path, directory) in [
            (&ffmpeg, false),
            (&ffprobe, false),
            (&bin, true),
            (&parent, true),
        ] {
            assert!(check_permissions(path, directory).is_err());
        }
    }
}
