//! Versions written in `app_start` and in the debug export: app, Windows,
//! ffmpeg in use, `tdjson.dll` the transport would load (hash), build mode.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};

use crate::sidecars::SidecarService;
use crate::transport::tdlib::library::SearchPaths;

/// `ffmpeg -version` takes ~30 ms; a cold, freshly scanned executable can take longer.
const VERSION_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Serialize)]
pub struct OsVersion {
    pub product: Option<String>,
    pub display_version: Option<String>,
    pub build: Option<String>,
    pub ubr: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FfmpegInfo {
    pub source: Option<&'static str>,
    pub ffmpeg_path: Option<String>,
    pub version: Option<String>,
    pub error_code: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TdlibInfo {
    pub dll_path: Option<String>,
    pub present: bool,
    pub sha256: Option<String>,
    /// `getOption("version")`: only once a TDLib client runs (not wired in this build).
    pub version: Option<String>,
}

pub fn build_mode() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }
}

/// `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion` (GetVersionEx lies past Windows 8).
#[cfg(windows)]
pub fn os_version() -> Option<OsVersion> {
    use windows::Win32::System::Registry::{
        HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
    };
    use windows::core::{HSTRING, w};

    let key = w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion");
    let text = |name: &str| -> Option<String> {
        let name = HSTRING::from(name);
        let mut buffer = [0u16; 256];
        let mut size = (buffer.len() * 2) as u32;
        // SAFETY: the buffer and its byte size are valid for the call; strings are NUL-ended.
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key,
                &name,
                RRF_RT_REG_SZ,
                None,
                Some(buffer.as_mut_ptr().cast()),
                Some(&mut size),
            )
        };
        if status.is_err() {
            return None;
        }
        let len = (size as usize / 2).saturating_sub(1).min(buffer.len());
        Some(String::from_utf16_lossy(&buffer[..len]))
    };
    let mut ubr = 0u32;
    let mut size = 4u32;
    // SAFETY: `ubr` is a valid 4-byte out buffer.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key,
            w!("UBR"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut ubr).cast()),
            Some(&mut size),
        )
    };
    Some(OsVersion {
        product: text("ProductName"),
        display_version: text("DisplayVersion"),
        build: text("CurrentBuildNumber"),
        ubr: status.is_ok().then_some(ubr),
    })
}

#[cfg(not(windows))]
pub fn os_version() -> Option<OsVersion> {
    None
}

/// First line of `<exe> -version`, bounded by `VERSION_TIMEOUT` (blocking).
fn tool_version(exe: &Path) -> Option<String> {
    let mut command = Command::new(exe);
    command
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = command.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    });
    let deadline = Instant::now() + VERSION_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(15)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let banner = reader.join().ok()?;
    crate::sidecars::discover::version_of(&banner, "ffmpeg")
}

/// The ffmpeg the media stage would use now, and its version (runs `ffmpeg -version`).
pub fn ffmpeg_info(sidecars: Option<&SidecarService>) -> FfmpegInfo {
    let Some(sidecars) = sidecars else {
        return FfmpegInfo {
            source: None,
            ffmpeg_path: None,
            version: None,
            error_code: Some("IO_ERROR"),
        };
    };
    match sidecars.resolve() {
        Ok(resolved) => FfmpegInfo {
            source: Some(resolved.source.as_str()),
            ffmpeg_path: Some(resolved.ffmpeg.to_string_lossy().into_owned()),
            version: tool_version(&resolved.ffmpeg),
            error_code: None,
        },
        Err(error) => FfmpegInfo {
            source: None,
            ffmpeg_path: None,
            version: None,
            error_code: Some(error.code()),
        },
    }
}

/// The `tdjson.dll` the transport would load (`SQUEEZEGRAM_TDJSON`, then the build's order
/// between next to the executable and `%LOCALAPPDATA%\squeezegram\tdlib\bin`, see `library`),
/// else the first place searched.
fn tdjson_path() -> Option<PathBuf> {
    match SearchPaths::from_process().locate() {
        Ok((path, _)) => Some(path),
        Err(searched) => searched.into_iter().next(),
    }
}

/// `tdjson.dll`: present or not, SHA-256 (about 0.1 s, cached while the file is unchanged).
pub fn tdlib_info() -> TdlibInfo {
    let path = tdjson_path();
    let present = path.as_deref().is_some_and(Path::is_file);
    TdlibInfo {
        dll_path: path.as_ref().map(|p| p.to_string_lossy().into_owned()),
        present,
        sha256: path
            .as_deref()
            .filter(|_| present)
            .and_then(crate::transport::tdlib::library::sha256_cached),
        version: None,
    }
}

/// Everything of `app_start` and of the export's `report.json` "versions".
pub fn versions(sidecars: Option<&SidecarService>) -> Value {
    json!({
        "app_version": env!("CARGO_PKG_VERSION"),
        "build": build_mode(),
        "os": os_version(),
        "ffmpeg": ffmpeg_info(sidecars),
        "tdlib": tdlib_info(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_have_the_documented_shape() {
        let value = versions(None);
        assert_eq!(value["app_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(value["build"], build_mode());
        assert_eq!(value["ffmpeg"]["error_code"], "IO_ERROR");
        assert!(value["tdlib"]["present"].is_boolean());
        if cfg!(windows) {
            let os = &value["os"];
            assert!(os["build"].as_str().is_some_and(|b| !b.is_empty()), "{os}");
        }
    }
}
