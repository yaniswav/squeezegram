//! Detection of an ffmpeg/ffprobe pair already installed on the machine.
//!
//! Detection only PROPOSES candidates: nothing is chosen from here, and the `PATH` is never
//! used to pick an ffmpeg (its folders are listed as candidates, labelled as such). The user's
//! click records one in `ffmpeg.custom_dir`. The search is bounded: a fixed list of known
//! places, a shallow descent (depth and entry caps), no symbolic links followed, no disk scan,
//! and no file is read except by running `-version` / `-encoders` on a candidate.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use super::EXE;
use super::recommend::{self, DownloadProfile, Reason, Recommendation};
use crate::media::detect::{self, Encoder, Tools, parse_encoder_list};
use crate::media::procs::{self, Args, Spec};

/// Folders below a package root that are looked into (`pkg\ffmpeg-x\bin` is depth 3).
const MAX_DEPTH: usize = 3;
/// Entries read per folder, and in total per root: bounds the work whatever the disk holds.
const MAX_ENTRIES_PER_DIR: usize = 256;
const MAX_ENTRIES_PER_ROOT: usize = 1024;
/// `PATH` entries looked at, and candidates validated (each costs up to three processes).
const MAX_PATH_DIRS: usize = 64;
const MAX_CANDIDATES: usize = 16;

const VERSION_TIMEOUT: Duration = Duration::from_secs(5);
const ENCODERS_TIMEOUT: Duration = Duration::from_secs(8);

/// Encoders shown to the user (the ones the app can use for H.264).
const SHOWN_ENCODERS: [&str; 4] = ["h264_nvenc", "libx264", "libopenh264", "h264_mf"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Winget,
    Chocolatey,
    Scoop,
    ProgramFiles,
    SystemDrive,
    /// A folder of the `PATH`: shown, never preferred.
    Path,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum License {
    Gpl,
    Lgpl,
    Unknown,
}

/// A validated ffmpeg/ffprobe pair, as sent to the front.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Candidate {
    /// What the page gives back to choose this one (`sidecars_use`): the core keeps the folder
    /// of each candidate it proposed and takes no path from the page. Set by
    /// `SidecarService::remember`; 0 until then.
    pub id: u64,
    /// Folder that would be recorded in `ffmpeg.custom_dir`; shown, never sent back.
    pub dir: String,
    pub ffmpeg_path: String,
    pub ffprobe_path: String,
    pub origin: Origin,
    pub version: String,
    pub license: License,
    /// Those of h264_nvenc, libx264, libopenh264 and h264_mf the build offers.
    pub encoders: Vec<String>,
    /// H.264 encoders that really encode with this binary (test encode, not `-encoders`).
    pub working_encoders: Vec<String>,
    /// Deterministic score (see `recommend`); candidates are sorted by it, best first.
    pub score: u32,
    /// The best choice overall, compared with the download.
    pub recommended: bool,
    /// Why this score, as translatable codes.
    pub reasons: Vec<Reason>,
}

/// Result of `sidecars_discover`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Discovery {
    pub candidates: Vec<Candidate>,
    pub recommendation: Recommendation,
}

/// What a working pair reports about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probed {
    pub version: String,
    pub license: License,
    pub encoders: Vec<String>,
}

/// The places searched, injectable for tests.
#[derive(Debug, Default, Clone)]
pub struct Places {
    pub local_app_data: Option<PathBuf>,
    pub program_data: Option<PathBuf>,
    pub user_profile: Option<PathBuf>,
    pub program_files: Option<PathBuf>,
    pub system_drive_root: Option<PathBuf>,
    pub path: Vec<PathBuf>,
}

impl Places {
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var_os(name).map(PathBuf::from);
        Self {
            local_app_data: var("LOCALAPPDATA"),
            program_data: var("ProgramData"),
            user_profile: var("USERPROFILE"),
            program_files: var("ProgramFiles"),
            system_drive_root: std::env::var("SystemDrive")
                .ok()
                .filter(|drive| !drive.is_empty())
                .map(|drive| PathBuf::from(format!("{drive}\\"))),
            path: std::env::var_os("PATH")
                .map(|path| std::env::split_paths(&path).collect())
                .unwrap_or_default(),
        }
    }
}

/// A folder holding both executables, before validation.
#[derive(Debug, Clone)]
struct Found {
    dir: PathBuf,
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
    origin: Origin,
}

fn pair_in(dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let ffmpeg = dir.join(format!("ffmpeg{EXE}"));
    let ffprobe = dir.join(format!("ffprobe{EXE}"));
    (ffmpeg.is_file() && ffprobe.is_file()).then_some((ffmpeg, ffprobe))
}

/// Sub-folders by name, symbolic links excluded, capped by `budget` (entries left for the root).
fn child_dirs(dir: &Path, budget: &mut usize) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    for entry in entries.take(MAX_ENTRIES_PER_DIR) {
        if *budget == 0 {
            break;
        }
        *budget -= 1;
        let Ok(entry) = entry else { continue };
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            dirs.push(entry.path());
        }
    }
    dirs.sort();
    dirs
}

/// `dir` itself, else its sub-folders down to `depth`. A folder holding the pair is not entered.
fn walk(dir: &Path, depth: usize, origin: Origin, budget: &mut usize, out: &mut Vec<Found>) {
    if let Some((ffmpeg, ffprobe)) = pair_in(dir) {
        out.push(Found {
            dir: dir.to_path_buf(),
            ffmpeg,
            ffprobe,
            origin,
        });
        return;
    }
    if depth == 0 {
        return;
    }
    for child in child_dirs(dir, budget) {
        walk(&child, depth - 1, origin, budget, out);
    }
}

fn walk_root(dir: &Path, depth: usize, origin: Origin, out: &mut Vec<Found>) {
    let mut budget = MAX_ENTRIES_PER_ROOT;
    walk(dir, depth, origin, &mut budget, out);
}

/// Sub-folders of `parent` whose name starts with `prefix` (case-insensitive), then `walk`.
fn walk_prefixed(parent: &Path, prefix: &str, origin: Origin, out: &mut Vec<Found>) {
    let mut budget = MAX_ENTRIES_PER_ROOT;
    for dir in child_dirs(parent, &mut budget) {
        let name = dir
            .file_name()
            .map(|name| name.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if name.starts_with(prefix) {
            walk_root(&dir, MAX_DEPTH, origin, out);
        }
    }
}

/// Folders with both executables in the known places, then the `PATH`; not validated, not
/// deduplicated.
fn list_places(places: &Places) -> Vec<Found> {
    let mut out = Vec::new();
    if let Some(local) = places.local_app_data.as_deref().filter(|p| p.is_absolute()) {
        let winget = local.join("Microsoft").join("WinGet");
        // `Links` holds stable launchers: first, so it wins the deduplication.
        walk_root(&winget.join("Links"), 0, Origin::Winget, &mut out);
        walk_root(
            &winget.join("Packages"),
            MAX_DEPTH,
            Origin::Winget,
            &mut out,
        );
    }
    if let Some(data) = places.program_data.as_deref().filter(|p| p.is_absolute()) {
        let choco = data.join("chocolatey");
        walk_root(&choco.join("bin"), 0, Origin::Chocolatey, &mut out);
        walk_prefixed(&choco.join("lib"), "ffmpeg", Origin::Chocolatey, &mut out);
    }
    if let Some(user) = places.user_profile.as_deref().filter(|p| p.is_absolute()) {
        let current = user
            .join("scoop")
            .join("apps")
            .join("ffmpeg")
            .join("current");
        walk_root(&current, 1, Origin::Scoop, &mut out);
    }
    if let Some(files) = places.program_files.as_deref().filter(|p| p.is_absolute()) {
        walk_prefixed(files, "ffmpeg", Origin::ProgramFiles, &mut out);
    }
    if let Some(root) = places
        .system_drive_root
        .as_deref()
        .filter(|p| p.is_absolute())
    {
        walk_prefixed(root, "ffmpeg", Origin::SystemDrive, &mut out);
    }
    // PATH entries are candidates only; a relative entry would depend on the working folder.
    for dir in places
        .path
        .iter()
        .filter(|dir| dir.is_absolute())
        .take(MAX_PATH_DIRS)
    {
        if let Some((ffmpeg, ffprobe)) = pair_in(dir) {
            out.push(Found {
                dir: dir.clone(),
                ffmpeg,
                ffprobe,
                origin: Origin::Path,
            });
        }
    }
    out
}

/// Identity of a file for deduplication: resolved links, no `\\?\` prefix, case-insensitive.
fn identity(path: &Path) -> String {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = resolved.to_string_lossy();
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_lowercase()
}

/// Keeps the first folder of each distinct (ffmpeg, ffprobe) file pair: links and aliases
/// (winget `Links`, a `PATH` entry pointing at the same install) collapse into one.
fn dedupe(found: Vec<Found>) -> Vec<Found> {
    let mut seen = HashSet::new();
    found
        .into_iter()
        .filter(|f| seen.insert((identity(&f.ffmpeg), identity(&f.ffprobe))))
        .collect()
}

/// First line `<tool> version <v> ...` of a `-version` output.
pub(crate) fn version_of(stdout: &str, tool: &str) -> Option<String> {
    let first = stdout.lines().next()?;
    let rest = first.strip_prefix(tool)?.strip_prefix(" version ")?;
    rest.split_whitespace().next().map(str::to_owned)
}

/// A build without `--enable-gpl` is LGPL; one without a configuration line, or with
/// `--enable-nonfree`, is not classified.
pub fn license_of(version_output: &str) -> License {
    let Some(config) = version_output
        .lines()
        .find_map(|line| line.strip_prefix("configuration:"))
    else {
        return License::Unknown;
    };
    let flags: Vec<&str> = config.split_whitespace().collect();
    if flags.contains(&"--enable-nonfree") {
        License::Unknown
    } else if flags.contains(&"--enable-gpl") {
        License::Gpl
    } else {
        License::Lgpl
    }
}

async fn run_tool(exe: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    let spec = Spec::new(exe, Args::new().all(args))
        .timeout(timeout)
        .collect_stdout();
    let output = procs::run(spec, |_| {}).await.ok()?;
    output.success().then_some(output.stdout)
}

/// Runs `-version` on both tools (and `-encoders` on ffmpeg). `None` when either is not a
/// working ffmpeg / ffprobe. Executables are given by path, so tests can use stand-ins.
pub async fn probe_pair(ffmpeg: &Path, ffprobe: &Path) -> Option<Probed> {
    let banner = run_tool(ffmpeg, &["-version"], VERSION_TIMEOUT).await?;
    let version = version_of(&banner, "ffmpeg")?;
    let probe_banner = run_tool(ffprobe, &["-version"], VERSION_TIMEOUT).await?;
    version_of(&probe_banner, "ffprobe")?;
    let encoders = run_tool(ffmpeg, &["-hide_banner", "-encoders"], ENCODERS_TIMEOUT)
        .await
        .map(|text| parse_encoder_list(&text))
        .unwrap_or_default();
    Some(Probed {
        version,
        license: license_of(&banner),
        encoders: SHOWN_ENCODERS
            .iter()
            .filter(|name| encoders.iter().any(|found| found == *name))
            .map(|name| (*name).to_owned())
            .collect(),
    })
}

/// Validates a folder (itself or its `bin`) given by the user: the pair must run.
pub async fn probe_dir(dir: &Path) -> Option<Probed> {
    let (ffmpeg, ffprobe) = super::find_in(dir)?;
    probe_pair(&ffmpeg, &ffprobe).await
}

/// Searches `places`, validates every folder found (in parallel, bounded) and returns the
/// working candidates: known places first, `PATH` folders last.
pub async fn discover_in(places: Places) -> Vec<Candidate> {
    let listed = tokio::task::spawn_blocking(move || dedupe(list_places(&places)))
        .await
        .unwrap_or_default();
    let tasks: Vec<_> = listed
        .into_iter()
        .take(MAX_CANDIDATES)
        .enumerate()
        .map(|(index, found)| {
            tokio::spawn(async move {
                let probed = probe_pair(&found.ffmpeg, &found.ffprobe).await?;
                let work_dir = test_dir("cand", index);
                let working = working_encoders(&found.ffmpeg, &found.ffprobe, &work_dir).await;
                Some(Candidate {
                    id: 0,
                    dir: found.dir.to_string_lossy().into_owned(),
                    ffmpeg_path: found.ffmpeg.to_string_lossy().into_owned(),
                    ffprobe_path: found.ffprobe.to_string_lossy().into_owned(),
                    origin: found.origin,
                    version: probed.version,
                    license: probed.license,
                    encoders: probed.encoders,
                    working_encoders: working.iter().map(|e| e.name().to_owned()).collect(),
                    score: 0,
                    recommended: false,
                    reasons: Vec::new(),
                })
            })
        })
        .collect();
    let mut candidates = Vec::new();
    for task in tasks {
        if let Ok(Some(candidate)) = task.await {
            candidates.push(candidate);
        }
    }
    candidates
}

/// A private folder for the short test encodes of one binary (parallel runs never collide).
fn test_dir(kind: &str, index: usize) -> PathBuf {
    std::env::temp_dir().join(format!(
        "squeezegram-recommend-{}-{kind}{index}",
        std::process::id()
    ))
}

/// The H.264 encoders that really encode with this binary: a short `testsrc2` encode per
/// encoder (8 s limit each), checked with ffprobe. Empty when the tests cannot run.
pub async fn working_encoders(ffmpeg: &Path, ffprobe: &Path, work_dir: &Path) -> Vec<Encoder> {
    let tools = Tools { ffmpeg, ffprobe };
    let cancel = tokio_util::sync::CancellationToken::new();
    let result = detect::detect_all(&tools, work_dir, &cancel).await;
    let _ = std::fs::remove_dir_all(work_dir);
    result
        .map(|detection| detection.working())
        .unwrap_or_default()
}

/// Ranks the candidates and compares them with the download (`profile`).
pub fn rank(candidates: Vec<Candidate>, profile: &DownloadProfile) -> Discovery {
    let mut candidates = candidates;
    let recommendation = recommend::recommend(&mut candidates, profile);
    Discovery {
        candidates,
        recommendation,
    }
}

/// The hardware the pinned LGPL build drives here: really tested when it is installed
/// (`installed` = its ffmpeg and ffprobe), else the list of the pinned build.
async fn download_profile(installed: Option<(PathBuf, PathBuf)>) -> DownloadProfile {
    let Some((ffmpeg, ffprobe)) = installed else {
        return DownloadProfile::assumed();
    };
    let work_dir = test_dir("pinned", 0);
    let tools = Tools {
        ffmpeg: &ffmpeg,
        ffprobe: &ffprobe,
    };
    let result = detect::detect_all(
        &tools,
        &work_dir,
        &tokio_util::sync::CancellationToken::new(),
    )
    .await;
    let _ = std::fs::remove_dir_all(&work_dir);
    match result {
        Ok(detection) => DownloadProfile::measured(&detection.working()),
        Err(_) => DownloadProfile::assumed(),
    }
}

pub async fn discover(installed_pinned: Option<(PathBuf, PathBuf)>) -> Discovery {
    let started = std::time::Instant::now();
    let (candidates, profile) = tokio::join!(
        discover_in(Places::from_env()),
        download_profile(installed_pinned)
    );
    let discovery = rank(candidates, &profile);
    let summary: Vec<_> = discovery
        .candidates
        .iter()
        .map(|c| {
            serde_json::json!({
                "dir": c.dir, "origin": c.origin, "version": c.version,
                "license": c.license, "encoders": c.encoders,
                "working": c.working_encoders, "score": c.score,
            })
        })
        .collect();
    tracing::info!(
        event = "sidecar_discover",
        dur_ms = started.elapsed().as_millis() as u64,
        count = discovery.candidates.len(),
        json = %serde_json::json!({ "candidates": summary }),
        "ffmpeg discovery finished"
    );
    let recommendation = &discovery.recommendation;
    let scores: Vec<_> = discovery
        .candidates
        .iter()
        .map(
            |c| serde_json::json!({ "dir": c.dir, "score": c.score, "recommended": c.recommended }),
        )
        .collect();
    let hardware: Vec<_> = profile.hardware.iter().map(|e| e.name()).collect();
    tracing::info!(
        event = "sidecar_recommend",
        choice = ?recommendation.kind,
        candidate_dir = recommendation.candidate_dir.as_deref(),
        download_assumed = profile.assumed,
        json = %serde_json::json!({
            "scores": scores,
            "download_hardware": hardware,
            "reasons": recommendation.reasons.iter().map(|r| r.code).collect::<Vec<_>>(),
        }),
        "ffmpeg recommendation"
    );
    discovery
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::test_util::TestDir;

    const LGPL_BANNER: &str = "ffmpeg version n9.0.2-22-g46d8f462ee Copyright (c) 2000-2026 the FFmpeg developers\nbuilt with gcc 15\nconfiguration: --prefix=/ffbuild/prefix --disable-debug --enable-version3 --enable-libx264\nlibavutil      60.  8.100\n";
    const GPL_BANNER: &str = "ffmpeg version 7.1-full_build Copyright (c) 2000-2025 the FFmpeg developers\nconfiguration: --enable-gpl --enable-version3 --enable-libx264\n";

    fn touch_pair(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(format!("ffmpeg{EXE}")), b"x").unwrap();
        std::fs::write(dir.join(format!("ffprobe{EXE}")), b"x").unwrap();
    }

    fn dirs(found: &[Found]) -> Vec<PathBuf> {
        found.iter().map(|f| f.dir.clone()).collect()
    }

    #[test]
    fn version_and_license_are_parsed() {
        assert_eq!(
            version_of(LGPL_BANNER, "ffmpeg").as_deref(),
            Some("n9.0.2-22-g46d8f462ee")
        );
        assert_eq!(version_of("ffprobe version 7.1 x", "ffmpeg"), None);
        assert_eq!(version_of("hello", "ffmpeg"), None);
        assert_eq!(license_of(LGPL_BANNER), License::Lgpl);
        assert_eq!(license_of(GPL_BANNER), License::Gpl);
        assert_eq!(license_of("ffmpeg version 1\n"), License::Unknown);
        assert_eq!(
            license_of("configuration: --enable-gpl --enable-nonfree"),
            License::Unknown
        );
    }

    #[test]
    fn known_places_are_found_and_path_comes_last() {
        let dir = TestDir::new("discover-places");
        let root = dir.path();
        let local = root.join("local");
        let data = root.join("data");
        let user = root.join("user");
        let files = root.join("files");
        let drive = root.join("drive");
        touch_pair(
            &local.join(r"Microsoft\WinGet\Packages\Gyan.FFmpeg_x\ffmpeg-8.0-full_build\bin"),
        );
        touch_pair(&local.join(r"Microsoft\WinGet\Links"));
        touch_pair(&data.join(r"chocolatey\bin"));
        touch_pair(&data.join(r"chocolatey\lib\ffmpeg\tools\ffmpeg\bin"));
        touch_pair(&data.join(r"chocolatey\lib\other\tools\bin"));
        touch_pair(&user.join(r"scoop\apps\ffmpeg\current\bin"));
        touch_pair(&files.join(r"FFmpeg-7\bin"));
        touch_pair(&drive.join(r"ffmpeg\bin"));
        touch_pair(&drive.join(r"Documents\ffmpeg\bin"));
        let on_path = root.join("tools");
        touch_pair(&on_path);

        let found = list_places(&Places {
            local_app_data: Some(local.clone()),
            program_data: Some(data.clone()),
            user_profile: Some(user.clone()),
            program_files: Some(files.clone()),
            system_drive_root: Some(drive.clone()),
            path: vec![on_path.clone(), PathBuf::from("relative")],
        });
        let origins: Vec<Origin> = found.iter().map(|f| f.origin).collect();
        assert_eq!(
            origins,
            [
                Origin::Winget,
                Origin::Winget,
                Origin::Chocolatey,
                Origin::Chocolatey,
                Origin::Scoop,
                Origin::ProgramFiles,
                Origin::SystemDrive,
                Origin::Path,
            ]
        );
        let dirs = dirs(&found);
        assert_eq!(dirs[0], local.join(r"Microsoft\WinGet\Links"));
        assert!(dirs.contains(&data.join(r"chocolatey\lib\ffmpeg\tools\ffmpeg\bin")));
        assert!(
            !dirs
                .iter()
                .any(|d| d.starts_with(data.join(r"chocolatey\lib\other")))
        );
        assert!(!dirs.contains(&drive.join(r"Documents\ffmpeg\bin")));
        assert_eq!(dirs.last(), Some(&on_path));
    }

    #[test]
    fn the_search_is_shallow_and_a_folder_needs_both_executables() {
        let dir = TestDir::new("discover-shallow");
        let drive = dir.path().join("drive");
        // Too deep for the depth cap.
        touch_pair(&drive.join(r"ffmpeg\a\b\c\bin"));
        // Only ffmpeg.
        std::fs::create_dir_all(drive.join(r"ffmpeg-half\bin")).unwrap();
        std::fs::write(drive.join(format!(r"ffmpeg-half\bin\ffmpeg{EXE}")), b"x").unwrap();
        let found = list_places(&Places {
            system_drive_root: Some(drive),
            ..Places::default()
        });
        assert!(found.is_empty(), "{:?}", dirs(&found));
    }

    #[test]
    fn entries_are_capped() {
        let dir = TestDir::new("discover-cap");
        let packages = dir.path().join("Packages");
        std::fs::create_dir_all(&packages).unwrap();
        for index in 0..(MAX_ENTRIES_PER_DIR + 10) {
            std::fs::create_dir_all(packages.join(format!("p{index:04}"))).unwrap();
        }
        // Sorted last: beyond the per-folder cap, so never looked at.
        touch_pair(&packages.join("zzz").join("bin"));
        let mut out = Vec::new();
        walk_root(&packages, MAX_DEPTH, Origin::Winget, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn links_to_the_same_files_are_deduplicated() {
        let dir = TestDir::new("discover-dedupe");
        let real = dir.path().join("real");
        touch_pair(&real);
        let other = dir.path().join("other");
        touch_pair(&other);
        let found = |dir: &Path, origin| Found {
            dir: dir.to_path_buf(),
            ffmpeg: dir.join(format!("ffmpeg{EXE}")),
            ffprobe: dir.join(format!("ffprobe{EXE}")),
            origin,
        };
        // The same folder spelled twice (case and `.`), as winget Links and PATH would.
        let upper = PathBuf::from(real.to_string_lossy().to_uppercase());
        let kept = dedupe(vec![
            found(&real, Origin::Winget),
            found(&real.join("."), Origin::Path),
            found(&upper, Origin::Path),
            found(&other, Origin::Path),
        ]);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].origin, Origin::Winget);
        assert_eq!(kept[1].dir, other);
    }

    /// Stand-in tools: `.cmd` scripts printing what the real ones print (names are free in
    /// `probe_pair`; `discover_in` insists on `ffmpeg.exe`).
    fn script(dir: &Path, name: &str, lines: &[&str]) -> PathBuf {
        let path = dir.join(name);
        let mut text = String::from("@echo off\r\n");
        for line in lines {
            // `echo.` keeps leading blanks and empty lines intact.
            text.push_str(&format!("echo.{line}\r\n"));
        }
        std::fs::write(&path, text).unwrap();
        path
    }

    #[tokio::test]
    async fn probe_reports_version_license_and_encoders() {
        let dir = TestDir::new("discover-probe");
        let ffmpeg = script(
            dir.path(),
            "fake-ffmpeg.cmd",
            &[
                "ffmpeg version 7.1-full_build Copyright",
                "configuration: --enable-gpl --enable-libx264",
            ],
        );
        // One script answers both `-version` and `-encoders` by printing both outputs; the
        // encoder parser ignores the banner lines.
        let encoders = script(
            dir.path(),
            "fake-ffmpeg2.cmd",
            &[
                "ffmpeg version 8 Copyright",
                "configuration: --disable-gpl",
                " V....D libx264              libx264 H.264",
                " V....D h264_mf             H264 via MediaFoundation",
                " V....D mpeg4               MPEG-4 part 2",
                " A....D aac                 AAC",
            ],
        );
        let ffprobe = script(
            dir.path(),
            "fake-ffprobe.cmd",
            &["ffprobe version 7.1 Copyright"],
        );

        let probed = probe_pair(&ffmpeg, &ffprobe).await.unwrap();
        assert_eq!(probed.version, "7.1-full_build");
        assert_eq!(probed.license, License::Gpl);
        let probed = probe_pair(&encoders, &ffprobe).await.unwrap();
        assert_eq!(probed.version, "8");
        assert_eq!(probed.license, License::Lgpl);
        assert_eq!(probed.encoders, ["libx264", "h264_mf"]);
    }

    #[tokio::test]
    async fn a_pair_that_is_not_ffmpeg_is_refused() {
        let dir = TestDir::new("discover-refuse");
        let ffmpeg = script(dir.path(), "ffmpeg.cmd", &["ffmpeg version 7 x"]);
        let liar = script(dir.path(), "liar.cmd", &["hello"]);
        let failing = dir.path().join("failing.cmd");
        std::fs::write(&failing, "@echo off\r\nexit /b 1\r\n").unwrap();
        let probe = script(dir.path(), "ffprobe.cmd", &["ffprobe version 7 x"]);
        assert!(probe_pair(&ffmpeg, &probe).await.is_some());
        assert!(probe_pair(&liar, &probe).await.is_none());
        assert!(probe_pair(&ffmpeg, &liar).await.is_none());
        assert!(probe_pair(&failing, &probe).await.is_none());
        assert!(
            probe_pair(&dir.path().join("absent.cmd"), &probe)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn non_working_executables_yield_no_candidate() {
        let dir = TestDir::new("discover-fake-exe");
        let drive = dir.path().join("drive");
        touch_pair(&drive.join("ffmpeg"));
        let candidates = discover_in(Places {
            system_drive_root: Some(drive),
            ..Places::default()
        })
        .await;
        assert!(candidates.is_empty());
    }

    /// Against the real ffmpeg downloaded in the session scratchpad; run with `--ignored`.
    #[tokio::test]
    #[ignore = "needs a real ffmpeg at a machine-specific path"]
    async fn real_ffmpeg_is_probed() {
        let bin_dir = std::env::var("SQUEEZEGRAM_TEST_FFMPEG_DIR")
            .expect("SQUEEZEGRAM_TEST_FFMPEG_DIR must point to the bin folder of an ffmpeg build");
        let bin = Path::new(&bin_dir);
        let probed = probe_dir(bin).await.expect("a working pair");
        eprintln!("real ffmpeg at {}: {probed:?}", bin.display());
        assert!(!probed.version.is_empty());
        assert!(!probed.encoders.is_empty());
    }

    /// The real machine: ranks what `discover` proposes (prints it); run with
    /// `--ignored --nocapture`. The scratchpad LGPL build stands for the installed download.
    #[tokio::test]
    #[ignore = "depends on what is installed on this machine"]
    async fn real_machine_discovery() {
        let bin_dir = std::env::var("SQUEEZEGRAM_TEST_FFMPEG_DIR")
            .expect("SQUEEZEGRAM_TEST_FFMPEG_DIR must point to the bin folder of an ffmpeg build");
        let bin = Path::new(&bin_dir);
        let installed = super::super::find_in(bin);
        if let Some((ffmpeg, ffprobe)) = &installed {
            let work = test_dir("test", 99);
            let working = working_encoders(ffmpeg, ffprobe, &work).await;
            eprintln!("LGPL build at {}: working {working:?}", bin.display());
        }
        for (label, installed) in [
            ("pinned not installed", None),
            ("pinned installed", installed),
        ] {
            let started = std::time::Instant::now();
            let discovery = discover(installed).await;
            eprintln!("== {label} ({} ms)", started.elapsed().as_millis());
            for c in &discovery.candidates {
                eprintln!(
                    "{:>4} {} v{} {:?} working={:?} recommended={} reasons={:?}",
                    c.score,
                    c.dir,
                    c.version,
                    c.license,
                    c.working_encoders,
                    c.recommended,
                    c.reasons.iter().map(|r| r.code).collect::<Vec<_>>()
                );
            }
            eprintln!("recommendation: {:?}", discovery.recommendation);
        }
        let candidates = discover_in(Places::from_env()).await;
        let mut dirs: Vec<_> = candidates
            .iter()
            .map(|c| identity(Path::new(&c.ffmpeg_path)))
            .collect();
        let count = dirs.len();
        dirs.dedup();
        assert_eq!(dirs.len(), count);
    }
}
