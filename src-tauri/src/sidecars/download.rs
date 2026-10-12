//! Pinned download with `.part` resume, SHA-256 check before extraction, zip extraction.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::{EXE, SidecarError};
use crate::queue::events::{EventSink, SidecarProgress};

/// A pinned archive: URL, exact size and SHA-256.
#[derive(Debug, Clone)]
pub struct Build {
    pub tag: String,
    pub url: String,
    pub size: u64,
    pub sha256: String,
}

const ATTEMPTS: usize = 3;
const CHUNK: usize = 256 * 1024;
const PROGRESS_EVERY: Duration = Duration::from_millis(250);
/// Share of the progress bar taken by the download; the rest is verification and extraction.
const DOWNLOAD_SHARE: f64 = 90.0;

/// Files kept from the archive, relative to its single root folder.
fn wanted() -> [String; 3] {
    [
        format!("bin/ffmpeg{EXE}"),
        format!("bin/ffprobe{EXE}"),
        "LICENSE.txt".into(),
    ]
}

/// Download progress written to the journal (the UI gets it at 4 Hz).
const LOG_EVERY: Duration = Duration::from_secs(5);

struct Reporter<'a> {
    sink: &'a dyn EventSink,
    name: &'a str,
    last: Option<Instant>,
    logged: Option<Instant>,
}

impl Reporter<'_> {
    fn report(&mut self, percent: f64, bytes_per_sec: Option<f64>, force: bool) {
        let now = Instant::now();
        if self.logged.is_none_or(|at| now - at >= LOG_EVERY) {
            self.logged = Some(now);
            tracing::info!(
                event = "sidecar_download_progress",
                name = self.name,
                percent = (percent * 10.0).round() / 10.0,
                bytes_per_sec = bytes_per_sec.unwrap_or(0.0).round(),
                "download progress"
            );
        }
        if !force && self.last.is_some_and(|at| now - at < PROGRESS_EVERY) {
            return;
        }
        self.last = Some(now);
        self.sink.sidecar_progress(&SidecarProgress {
            name: self.name.to_owned(),
            percent: percent.clamp(0.0, 100.0),
            bytes_per_sec,
        });
    }
}

/// Downloads `build` into `base`, verifies it and installs it in `base\<dir_name>`.
pub fn install_build(
    build: &Build,
    base: &Path,
    dir_name: &str,
    sink: &dyn EventSink,
) -> Result<(), SidecarError> {
    fs::create_dir_all(base)?;
    let mut reporter = Reporter {
        sink,
        name: dir_name,
        last: None,
        logged: None,
    };
    let part = base.join(format!(
        "{}.zip.part",
        build.sha256.get(..12).unwrap_or("download")
    ));
    let started = Instant::now();
    download(build, &part, &mut reporter)?;
    tracing::info!(
        event = "sidecar_download_done",
        dur_ms = started.elapsed().as_millis() as u64,
        bytes = build.size,
        "download complete"
    );

    reporter.report(DOWNLOAD_SHARE, None, true);
    let started = Instant::now();
    let actual = file_sha256(&part)?;
    let expected = build.sha256.to_ascii_lowercase();
    let dur_ms = started.elapsed().as_millis() as u64;
    if actual != expected {
        tracing::error!(
            event = "sidecar_hash",
            dur_ms,
            ok = false,
            expected = expected.as_str(),
            actual = actual.as_str(),
            error_code = "SIDECAR_CHECKSUM",
            "SHA-256 of the archive does not match the pinned one"
        );
        let _ = fs::remove_file(&part);
        return Err(SidecarError::Checksum);
    }
    tracing::info!(
        event = "sidecar_hash",
        dur_ms,
        ok = true,
        actual = actual.as_str(),
        "SHA-256 of the archive checked"
    );
    reporter.report(DOWNLOAD_SHARE + 5.0, None, true);

    let staging = base.join(format!("{dir_name}.new"));
    let _ = fs::remove_dir_all(&staging);
    let started = Instant::now();
    if let Err(error) = extract(&part, &staging) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    tracing::info!(
        event = "sidecar_extract",
        dur_ms = started.elapsed().as_millis() as u64,
        into = %staging.display(),
        "archive extracted"
    );
    fs::write(staging.join("BUILD.txt"), &build.tag)?;
    let target = base.join(dir_name);
    if target.exists() {
        fs::remove_dir_all(&target)?;
    }
    fs::rename(&staging, &target)?;
    let _ = fs::remove_file(&part);
    reporter.report(100.0, None, true);
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn file_sha256(path: &Path) -> Result<String, SidecarError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

fn download(build: &Build, part: &Path, reporter: &mut Reporter<'_>) -> Result<(), SidecarError> {
    let mut last_error = String::new();
    for attempt in 1..=ATTEMPTS {
        match download_once(build, part, reporter) {
            Ok(()) => return Ok(()),
            // The `.part` is kept: the next attempt resumes where this one stopped.
            Err(SidecarError::Download(message)) => {
                tracing::warn!(
                    event = "sidecar_download_retry",
                    attempt,
                    error_code = "SIDECAR_DOWNLOAD",
                    error = message.as_str(),
                    "download attempt failed"
                );
                last_error = message;
            }
            Err(other) => return Err(other),
        }
    }
    Err(SidecarError::Download(last_error))
}

fn download_once(
    build: &Build,
    part: &Path,
    reporter: &mut Reporter<'_>,
) -> Result<(), SidecarError> {
    let fail = |error: &dyn std::fmt::Display| SidecarError::Download(error.to_string());

    let mut offset = fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    if offset > build.size {
        fs::remove_file(part)?;
        offset = 0;
    }
    if offset == build.size {
        return Ok(());
    }

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .build()
        .into();
    let mut request = agent.get(&build.url);
    if offset > 0 {
        request = request.header("Range", format!("bytes={offset}-"));
    }
    tracing::info!(
        event = "sidecar_download_start",
        resume_from = offset,
        size = build.size,
        "download request"
    );
    let response = request.call().map_err(|e| fail(&e))?;
    let status = response.status().as_u16();
    tracing::debug!(
        event = "sidecar_download_response",
        status,
        "download response"
    );
    let mut file = match status {
        206 => OpenOptions::new().append(true).open(part)?,
        // The server ignored the range: start over.
        200 => {
            offset = 0;
            File::create(part)?
        }
        // Our `.part` is not a prefix of the file: start over on the next attempt.
        416 => {
            fs::remove_file(part)?;
            return Err(SidecarError::Download("HTTP 416".into()));
        }
        other => return Err(SidecarError::Download(format!("HTTP {other}"))),
    };

    let mut body = response.into_body().into_reader();
    let started = Instant::now();
    let mut received = 0u64;
    let mut buffer = vec![0u8; CHUNK];
    loop {
        let read = body.read(&mut buffer).map_err(|e| fail(&e))?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])?;
        received += read as u64;
        let done = offset + received;
        let speed = received as f64 / started.elapsed().as_secs_f64().max(0.001);
        reporter.report(
            done as f64 / build.size as f64 * DOWNLOAD_SHARE,
            Some(speed),
            false,
        );
    }
    file.flush()?;
    let total = offset + received;
    if total != build.size {
        return Err(SidecarError::Download(format!(
            "incomplete: {total} of {} bytes",
            build.size
        )));
    }
    Ok(())
}

fn extract(archive: &Path, into: &Path) -> Result<(), SidecarError> {
    let bad = |error: &dyn std::fmt::Display| SidecarError::Extract(error.to_string());
    let mut zip = zip::ZipArchive::new(File::open(archive)?).map_err(|e| bad(&e))?;
    let wanted = wanted();
    let mut found = Vec::new();
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(|e| bad(&e))?;
        // Only exact, known names are written: nothing from the archive picks a path.
        let name = entry.name().replace('\\', "/");
        let Some((_, relative)) = name.split_once('/') else {
            continue;
        };
        if !wanted.iter().any(|w| w == relative) {
            continue;
        }
        let out: PathBuf = into.join(relative);
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = File::create(&out)?;
        std::io::copy(&mut entry, &mut file)?;
        found.push(relative.to_owned());
    }
    for must in &wanted[..2] {
        if !found.contains(must) {
            return Err(SidecarError::Extract(format!(
                "{must} is not in the archive"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::queue::events::recorder::Recorder;
    use crate::queue::test_util::TestDir;
    use crate::settings::SettingsStore;
    use crate::sidecars::{SidecarService, Source};

    /// Incompressible bytes, so the archive keeps its size.
    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect()
    }

    fn make_zip() -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut bytes));
            let options = zip::write::SimpleFileOptions::default();
            for (name, data) in [
                ("ffmpeg-test/bin/ffmpeg.exe", noise(300_000, 1)),
                ("ffmpeg-test/bin/ffprobe.exe", noise(200_000, 2)),
                ("ffmpeg-test/bin/ffplay.exe", vec![3u8; 1000]),
                ("ffmpeg-test/LICENSE.txt", b"lgpl".to_vec()),
                ("ffmpeg-test/doc/index.html", b"<html>".to_vec()),
            ] {
                zip.start_file(name, options).unwrap();
                zip.write_all(&data).unwrap();
            }
            zip.finish().unwrap();
        }
        bytes
    }

    fn build_for(bytes: &[u8], url: String) -> Build {
        Build {
            tag: "test".into(),
            url,
            size: bytes.len() as u64,
            sha256: hex(&Sha256::digest(bytes)),
        }
    }

    /// Minimal HTTP/1.1 server: honours `Range`, can cut the first response short.
    struct FakeServer {
        url: String,
        requests: Arc<AtomicUsize>,
        ranges: Arc<std::sync::Mutex<Vec<Option<u64>>>>,
    }

    fn serve(body: Vec<u8>, cut_first_at: Option<usize>) -> FakeServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/ffmpeg.zip", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let ranges = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (count, seen) = (requests.clone(), ranges.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut start = None;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                        start = value.trim().trim_end_matches('-').parse::<u64>().ok();
                    }
                }
                let nth = count.fetch_add(1, Ordering::SeqCst);
                seen.lock().unwrap().push(start);
                let from = start.unwrap_or(0) as usize;
                let part = &body[from..];
                let head = if start.is_some() {
                    format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {from}-{}/{}\r\nConnection: close\r\n\r\n",
                        part.len(),
                        body.len() - 1,
                        body.len()
                    )
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        part.len()
                    )
                };
                let _ = stream.write_all(head.as_bytes());
                let sent = match (nth, cut_first_at) {
                    (0, Some(cut)) => &part[..cut],
                    _ => part,
                };
                let _ = stream.write_all(sent);
            }
        });
        FakeServer {
            url,
            requests,
            ranges,
        }
    }

    #[test]
    fn installs_only_the_needed_files_after_checking_the_hash() {
        let zip = make_zip();
        let server = serve(zip.clone(), None);
        let dir = TestDir::new("sidecar-ok");
        let sink = Recorder::default();
        install_build(
            &build_for(&zip, server.url.clone()),
            dir.path(),
            "ffmpeg",
            &sink,
        )
        .unwrap();

        let target = dir.path().join("ffmpeg");
        assert_eq!(
            fs::read(target.join("bin/ffmpeg.exe")).unwrap().len(),
            300_000
        );
        assert!(target.join("bin/ffprobe.exe").is_file());
        assert!(target.join("LICENSE.txt").is_file());
        assert!(
            !target.join("bin/ffplay.exe").exists(),
            "ffplay is not installed"
        );
        assert!(!target.join("doc").exists());
        assert_eq!(
            fs::read_to_string(target.join("BUILD.txt")).unwrap(),
            "test"
        );
        let leftovers: Vec<_> = fs::read_dir(dir.path()).unwrap().flatten().collect();
        assert_eq!(leftovers.len(), 1, "no .part or staging left");

        let progress = sink.sidecar.lock().unwrap();
        assert_eq!(progress.last().unwrap().percent, 100.0);
        assert!(progress.windows(2).all(|w| w[0].percent <= w[1].percent));
    }

    #[test]
    fn a_wrong_hash_is_refused_and_nothing_is_extracted() {
        let zip = make_zip();
        let server = serve(zip.clone(), None);
        let dir = TestDir::new("sidecar-bad");
        let mut build = build_for(&zip, server.url.clone());
        build.sha256 = "0".repeat(64);
        let error = install_build(&build, dir.path(), "ffmpeg", &Recorder::default()).unwrap_err();
        assert_eq!(error.code(), "SIDECAR_CHECKSUM");
        assert_eq!(
            fs::read_dir(dir.path()).unwrap().count(),
            0,
            "part deleted, no install"
        );
    }

    #[test]
    fn an_interrupted_download_resumes_from_the_part_file() {
        let zip = make_zip();
        // The first response stops after 100 000 bytes; the retry must ask for the rest.
        let server = serve(zip.clone(), Some(100_000));
        let dir = TestDir::new("sidecar-resume");
        install_build(
            &build_for(&zip, server.url.clone()),
            dir.path(),
            "ffmpeg",
            &Recorder::default(),
        )
        .unwrap();
        assert!(dir.path().join("ffmpeg/bin/ffmpeg.exe").is_file());
        assert_eq!(server.requests.load(Ordering::SeqCst), 2);
        assert_eq!(*server.ranges.lock().unwrap(), vec![None, Some(100_000)]);
    }

    #[test]
    fn a_leftover_part_from_a_previous_run_is_resumed() {
        let zip = make_zip();
        let server = serve(zip.clone(), None);
        let dir = TestDir::new("sidecar-leftover");
        let build = build_for(&zip, server.url.clone());
        fs::write(
            dir.path().join(format!("{}.zip.part", &build.sha256[..12])),
            &zip[..50_000],
        )
        .unwrap();
        install_build(&build, dir.path(), "ffmpeg", &Recorder::default()).unwrap();
        assert_eq!(*server.ranges.lock().unwrap(), vec![Some(50_000)]);
    }

    #[test]
    fn an_http_error_is_a_download_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/x.zip", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten().take(ATTEMPTS) {
                let mut buffer = [0u8; 2048];
                let _ = stream.read(&mut buffer);
                let _ = stream.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
        });
        let dir = TestDir::new("sidecar-404");
        let build = Build {
            tag: "t".into(),
            url,
            size: 10,
            sha256: "0".repeat(64),
        };
        let error = install_build(&build, dir.path(), "ffmpeg", &Recorder::default()).unwrap_err();
        assert_eq!(error.code(), "SIDECAR_DOWNLOAD");
    }

    #[test]
    fn resolution_prefers_the_user_directory_and_never_uses_the_path() {
        let dir = TestDir::new("sidecar-resolve");
        let settings_dir = TestDir::new("sidecar-resolve-settings");
        let settings =
            Arc::new(SettingsStore::load(settings_dir.path().join("settings.json")).unwrap());
        let base = dir.path().join("sidecars");
        let service = SidecarService::new(base.clone(), settings.clone());
        assert_eq!(service.resolve().unwrap_err().code(), "FFMPEG_MISSING");
        assert!(!service.status().installed);

        let installed = base.join("ffmpeg/bin");
        fs::create_dir_all(&installed).unwrap();
        fs::write(installed.join(format!("ffmpeg{EXE}")), b"x").unwrap();
        fs::write(installed.join(format!("ffprobe{EXE}")), b"x").unwrap();
        assert_eq!(service.resolve().unwrap().source, Source::Installed);

        let custom = dir.path().join("mine");
        fs::create_dir_all(&custom).unwrap();
        fs::write(custom.join(format!("ffmpeg{EXE}")), b"x").unwrap();
        fs::write(custom.join(format!("ffprobe{EXE}")), b"x").unwrap();
        settings
            .set(&serde_json::json!({ "ffmpeg": { "custom_dir": custom.to_string_lossy() } }))
            .unwrap();
        let resolved = service.resolve().unwrap();
        assert_eq!(resolved.source, Source::Custom);
        assert_eq!(resolved.ffmpeg, custom.join(format!("ffmpeg{EXE}")));

        // A wrong user directory is an error, not a silent fallback to another build.
        let missing = dir.path().join("nope");
        settings
            .set(&serde_json::json!({ "ffmpeg": { "custom_dir": missing.to_string_lossy() } }))
            .unwrap();
        assert_eq!(service.resolve().unwrap_err().code(), "FFMPEG_MISSING");
    }
}
