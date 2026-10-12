//! Start with Windows: one value in `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
//!
//! A registry value rather than `tauri-plugin-autostart`: the plugin wraps exactly this value
//! (through `auto-launch`) and adds a crate, a page-facing permission set and a JavaScript
//! package for something the core does in a few calls with the `windows` crate it already
//! has. The page never reaches the registry: it asks `autostart_set`, which checks the
//! license.
//!
//! The entry is the only record of the choice (no setting to drift from it). Windows also
//! keeps an on/off flag per entry for the Startup page of Task Manager
//! (`...\Explorer\StartupApproved\Run`): turning the option on clears it, so an entry the
//! user once switched off there starts again when they switch the option back on.

use std::io;
use std::path::Path;

use super::BACKGROUND_ARG;
use crate::entitlements::{Entitlements, Feature};

/// The persistence of the "start with Windows" choice; the registry in the app, a fake in tests.
pub trait Autostart: Send + Sync {
    fn is_enabled(&self) -> bool;
    /// Creates or replaces the entry, which runs `command`.
    fn enable(&self, command: &str) -> io::Result<()>;
    /// Removes the entry; already gone is not an error.
    fn disable(&self) -> io::Result<()>;
}

/// The command line the entry runs: the executable, quoted, and the hidden-start argument.
pub fn command_line(exe: &Path) -> String {
    format!("\"{}\" {BACKGROUND_ARG}", exe.display())
}

/// Why `set` did nothing.
#[derive(Debug, thiserror::Error)]
pub enum AutostartError {
    #[error("start with Windows is a paid feature")]
    NotLicensed,
    #[error("registry: {0}")]
    Io(#[from] io::Error),
}

/// Turns the entry on (licensed only) or off (always: losing the license must not trap the
/// entry). Returns whether the entry exists afterwards.
pub fn set(
    autostart: &dyn Autostart,
    entitlements: &dyn Entitlements,
    exe: &Path,
    enabled: bool,
) -> Result<bool, AutostartError> {
    if enabled {
        if !entitlements.allows(Feature::Background) {
            return Err(AutostartError::NotLicensed);
        }
        autostart.enable(&command_line(exe))?;
    } else {
        autostart.disable()?;
    }
    Ok(autostart.is_enabled())
}

#[cfg(windows)]
pub use registry::RegistryAutostart;

#[cfg(windows)]
mod registry {
    use std::io;

    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
    };
    use windows::core::{PCWSTR, w};

    use super::Autostart;

    const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    const APPROVED_KEY: PCWSTR =
        w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\Run");
    const VALUE: PCWSTR = w!("squeezegram");

    /// `Ok` for success, and for a value that was not there when `missing_ok`.
    fn check(status: WIN32_ERROR, missing_ok: bool) -> io::Result<()> {
        if status == ERROR_SUCCESS || (missing_ok && status == ERROR_FILE_NOT_FOUND) {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status.0 as i32))
        }
    }

    pub struct RegistryAutostart;

    impl Autostart for RegistryAutostart {
        fn is_enabled(&self) -> bool {
            let mut size = 0u32;
            // SAFETY: no data buffer is given; `size` is a valid out value. The key and value
            // names are NUL-ended literals.
            let status = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    RUN_KEY,
                    VALUE,
                    RRF_RT_REG_SZ,
                    None,
                    None,
                    Some(&mut size),
                )
            };
            status == ERROR_SUCCESS
        }

        fn enable(&self, command: &str) -> io::Result<()> {
            let data: Vec<u16> = command.encode_utf16().chain(Some(0)).collect();
            // SAFETY: `data` is NUL-ended and its byte size is given; the names are literals.
            let status = unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    RUN_KEY,
                    VALUE,
                    REG_SZ.0,
                    Some(data.as_ptr().cast()),
                    (data.len() * 2) as u32,
                )
            };
            check(status, false)?;
            // SAFETY: the names are NUL-ended literals.
            let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, APPROVED_KEY, VALUE) };
            check(status, true)
        }

        fn disable(&self) -> io::Result<()> {
            // SAFETY: the names are NUL-ended literals.
            let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE) };
            check(status, true)
        }
    }
}

/// No Run key outside Windows: the option is never on.
#[cfg(not(windows))]
pub struct RegistryAutostart;

#[cfg(not(windows))]
impl Autostart for RegistryAutostart {
    fn is_enabled(&self) -> bool {
        false
    }

    fn enable(&self, _command: &str) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }

    fn disable(&self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use super::*;
    use crate::entitlements::{EntitlementStatus, FreeOnly};

    /// The registry value, in memory.
    #[derive(Default)]
    struct Fake {
        command: Mutex<Option<String>>,
        fail: bool,
    }

    impl Autostart for Fake {
        fn is_enabled(&self) -> bool {
            self.command.lock().unwrap().is_some()
        }

        fn enable(&self, command: &str) -> io::Result<()> {
            if self.fail {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            *self.command.lock().unwrap() = Some(command.to_owned());
            Ok(())
        }

        fn disable(&self) -> io::Result<()> {
            if self.fail {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            *self.command.lock().unwrap() = None;
            Ok(())
        }
    }

    /// Grants `background` only.
    struct Licensed;

    impl Entitlements for Licensed {
        fn allows(&self, feature: Feature) -> bool {
            feature == Feature::Background
        }

        fn status(&self) -> EntitlementStatus {
            EntitlementStatus::FREE_BUILD
        }
    }

    fn exe() -> PathBuf {
        PathBuf::from(r"C:\Program Files\squeezegram\squeezegram.exe")
    }

    #[test]
    fn the_entry_runs_the_quoted_executable_with_the_hidden_start_argument() {
        assert_eq!(
            command_line(&exe()),
            r#""C:\Program Files\squeezegram\squeezegram.exe" --background"#
        );
    }

    #[test]
    fn turning_it_on_needs_the_license_and_writes_nothing_without_it() {
        let fake = Fake::default();
        let error = set(&fake, &FreeOnly, &exe(), true).unwrap_err();
        assert!(matches!(error, AutostartError::NotLicensed));
        assert!(!fake.is_enabled());

        assert!(set(&fake, &Licensed, &exe(), true).unwrap());
        assert_eq!(
            fake.command.lock().unwrap().as_deref(),
            Some(command_line(&exe()).as_str())
        );
    }

    #[test]
    fn turning_it_off_removes_the_entry_even_after_the_license_is_lost() {
        let fake = Fake::default();
        set(&fake, &Licensed, &exe(), true).unwrap();
        // The license goes away: the entry stays until the user removes it...
        assert!(fake.is_enabled());
        // ...and removing it is never refused.
        assert!(!set(&fake, &FreeOnly, &exe(), false).unwrap());
        assert!(!fake.is_enabled());
        // Already gone is fine.
        assert!(!set(&fake, &FreeOnly, &exe(), false).unwrap());
    }

    #[test]
    fn a_registry_failure_is_reported_and_leaves_the_state_as_it_was() {
        let fake = Fake {
            fail: true,
            ..Fake::default()
        };
        assert!(matches!(
            set(&fake, &Licensed, &exe(), true),
            Err(AutostartError::Io(_))
        ));
        assert!(!fake.is_enabled());
    }
}
