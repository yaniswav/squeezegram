//! Recommendation among the ffmpeg builds found on the machine and the pinned download.
//!
//! Scores are deterministic and built from the encoders that REALLY work with each binary
//! (a test encode, not `-encoders`). Reasons are typed codes with parameters: the front
//! translates them. The licence never lowers a score (a GPL ffmpeg stays on the user's
//! machine); it only breaks ties, LGPL first.

use std::cmp::Ordering;

use serde::Serialize;
use serde_json::{Value, json};

use super::discover::{Candidate, License};
use crate::media::detect::Encoder;

const HW_FIRST: u32 = 100;
const HW_EXTRA: u32 = 10;
const X264: u32 = 40;
const OPENH264: u32 = 10;
const MEDIA_FOUNDATION: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    // Candidate reasons.
    HwNvenc,
    HwQsv,
    HwAmf,
    CpuX264,
    WeakFallbackOnly,
    NoWorkingEncoder,
    LicenseLgpl,
    LicenseGpl,
    // Global reasons.
    CandidateAddsX264,
    CandidateAddsHardware,
    DownloadNoCandidate,
    DownloadNoGain,
    DownloadHardwareTested,
    DownloadHardwareAssumed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Reason {
    pub code: ReasonCode,
    pub params: Value,
}

impl Reason {
    fn new(code: ReasonCode, params: Value) -> Self {
        Self { code, params }
    }

    fn plain(code: ReasonCode) -> Self {
        Self::new(code, json!({}))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Download,
    Candidate,
}

/// The overall advice, compared with the pinned download.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Recommendation {
    pub kind: Kind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_dir: Option<String>,
    pub reasons: Vec<Reason>,
}

/// What the pinned LGPL build can do on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadProfile {
    /// Hardware H.264 encoders it drives.
    pub hardware: Vec<Encoder>,
    /// `true`: the build is not installed, its list is the one of the pinned build.
    pub assumed: bool,
}

impl DownloadProfile {
    /// The hardware encoders of the pinned build, when it is not installed.
    pub fn assumed() -> Self {
        Self {
            hardware: vec![Encoder::Nvenc, Encoder::Qsv, Encoder::Amf],
            assumed: true,
        }
    }

    /// From the real detection of the installed pinned build.
    pub fn measured(working: &[Encoder]) -> Self {
        Self {
            hardware: working.iter().copied().filter(|e| e.is_gpu()).collect(),
            assumed: false,
        }
    }
}

/// Score and reasons of one build from its working encoders and licence.
pub fn score(working: &[Encoder], license: License) -> (u32, Vec<Reason>) {
    let has = |encoder| working.contains(&encoder);
    let mut score = 0;
    let mut reasons = Vec::new();
    let mut hardware = 0u32;
    for (encoder, code) in [
        (Encoder::Nvenc, ReasonCode::HwNvenc),
        (Encoder::Qsv, ReasonCode::HwQsv),
        (Encoder::Amf, ReasonCode::HwAmf),
    ] {
        if has(encoder) {
            score += if hardware == 0 { HW_FIRST } else { HW_EXTRA };
            hardware += 1;
            reasons.push(Reason::new(code, json!({ "encoder": encoder.name() })));
        }
    }
    if has(Encoder::X264) {
        score += X264;
        reasons.push(Reason::plain(ReasonCode::CpuX264));
    }
    let weak: Vec<&str> = [
        (Encoder::OpenH264, OPENH264),
        (Encoder::MediaFoundation, MEDIA_FOUNDATION),
    ]
    .into_iter()
    .filter(|(encoder, _)| has(*encoder))
    .map(|(encoder, points)| {
        score += points;
        encoder.name()
    })
    .collect();
    if hardware == 0 && !has(Encoder::X264) {
        if weak.is_empty() {
            reasons.push(Reason::plain(ReasonCode::NoWorkingEncoder));
        } else {
            reasons.push(Reason::new(
                ReasonCode::WeakFallbackOnly,
                json!({ "list": weak.join(", ") }),
            ));
        }
    }
    match license {
        License::Lgpl => reasons.push(Reason::plain(ReasonCode::LicenseLgpl)),
        License::Gpl => reasons.push(Reason::plain(ReasonCode::LicenseGpl)),
        License::Unknown => {}
    }
    (score, reasons)
}

/// Comparable form of a version string: `n9.0.2-22-gabc` -> [9, 0, 2], `7.1-full_build` ->
/// [7, 1], a dated git build `2025-10-01-git-x` -> [2025, 10, 1].
fn version_key(version: &str) -> Vec<u64> {
    // Git master build `N-124279-gabc-20260430`: its date, newer than any release number.
    if let Some(date) = version
        .strip_prefix("N-")
        .and_then(|rest| rest.rsplit('-').next())
        .filter(|date| date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()))
    {
        let part = |range: std::ops::Range<usize>| date[range].parse::<u64>().unwrap_or(0);
        return vec![part(0..4), part(4..6), part(6..8)];
    }
    let text = version.trim_start_matches(['n', 'N', 'v', 'V']);
    let numbers = |separator: char| {
        let head: String = text
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == separator)
            .collect();
        head.split(separator)
            .map_while(|part| part.parse::<u64>().ok())
            .collect::<Vec<_>>()
    };
    let dashed = numbers('-');
    // A date: year first, three numbers.
    if dashed.first().is_some_and(|year| *year >= 1000) && dashed.len() >= 3 {
        return dashed[..3].to_vec();
    }
    numbers('.')
}

fn license_rank(license: License) -> u8 {
    match license {
        License::Lgpl => 0,
        License::Gpl | License::Unknown => 1,
    }
}

/// Best first: score, then newer version, then LGPL. Stable for the rest.
fn compare(a: &Candidate, b: &Candidate) -> Ordering {
    b.score
        .cmp(&a.score)
        .then_with(|| version_key(&b.version).cmp(&version_key(&a.version)))
        .then_with(|| license_rank(a.license).cmp(&license_rank(b.license)))
}

/// What a candidate brings that the download does not: libx264, or hardware it drives that
/// the download does not.
fn gain(candidate: &Candidate, download: &DownloadProfile) -> Option<Reason> {
    let works = |encoder: Encoder| {
        candidate
            .working_encoders
            .iter()
            .any(|name| name == encoder.name())
    };
    if let Some(encoder) = Encoder::all()
        .filter(|e| e.is_gpu())
        .find(|e| works(*e) && !download.hardware.contains(e))
    {
        return Some(Reason::new(
            ReasonCode::CandidateAddsHardware,
            json!({ "encoder": encoder.name() }),
        ));
    }
    works(Encoder::X264).then(|| Reason::plain(ReasonCode::CandidateAddsX264))
}

fn download_note(download: &DownloadProfile) -> Reason {
    Reason::plain(if download.assumed {
        ReasonCode::DownloadHardwareAssumed
    } else {
        ReasonCode::DownloadHardwareTested
    })
}

/// Scores and sorts `candidates` (best first), and decides between the best of them and the
/// download. `recommended` is set on the chosen candidate only.
pub fn recommend(candidates: &mut [Candidate], download: &DownloadProfile) -> Recommendation {
    for candidate in candidates.iter_mut() {
        let working: Vec<Encoder> = candidate
            .working_encoders
            .iter()
            .filter_map(|name| Encoder::parse(name))
            .collect();
        (candidate.score, candidate.reasons) = score(&working, candidate.license);
        candidate.recommended = false;
    }
    candidates.sort_by(compare);

    let chosen = candidates
        .iter_mut()
        .filter(|candidate| candidate.score > 0)
        .find_map(|candidate| gain(candidate, download).map(|reason| (candidate, reason)));
    match chosen {
        Some((candidate, reason)) => {
            candidate.recommended = true;
            Recommendation {
                kind: Kind::Candidate,
                candidate_dir: Some(candidate.dir.clone()),
                reasons: vec![reason],
            }
        }
        None => {
            let first = if candidates.is_empty() {
                ReasonCode::DownloadNoCandidate
            } else {
                ReasonCode::DownloadNoGain
            };
            Recommendation {
                kind: Kind::Download,
                candidate_dir: None,
                reasons: vec![Reason::plain(first), download_note(download)],
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidecars::discover::Origin;

    fn candidate(dir: &str, version: &str, license: License, working: &[Encoder]) -> Candidate {
        Candidate {
            dir: dir.into(),
            ffmpeg_path: format!("{dir}\\ffmpeg.exe"),
            ffprobe_path: format!("{dir}\\ffprobe.exe"),
            origin: Origin::Winget,
            version: version.into(),
            license,
            encoders: Vec::new(),
            working_encoders: working.iter().map(|e| e.name().to_owned()).collect(),
            score: 0,
            recommended: false,
            reasons: Vec::new(),
        }
    }

    fn codes(reasons: &[Reason]) -> Vec<ReasonCode> {
        reasons.iter().map(|r| r.code).collect()
    }

    const GPU_ONLY: &[Encoder] = &[Encoder::Nvenc];
    const X264_ONLY: &[Encoder] = &[Encoder::X264];

    #[test]
    fn hardware_outweighs_libx264_which_outweighs_weak_fallbacks() {
        let gpu = score(GPU_ONLY, License::Gpl).0;
        let x264 = score(X264_ONLY, License::Gpl).0;
        let both = score(&[Encoder::Nvenc, Encoder::X264], License::Gpl).0;
        let weak = score(
            &[Encoder::OpenH264, Encoder::MediaFoundation],
            License::Lgpl,
        )
        .0;
        assert!(gpu > x264 && x264 > weak && weak > 0);
        assert!(both > gpu);
        assert_eq!(score(&[], License::Gpl).0, 0);
        // A second GPU encoder counts, but less than a first one.
        let two = score(&[Encoder::Nvenc, Encoder::Qsv], License::Gpl).0;
        assert!(two > gpu && two - gpu < gpu);
    }

    #[test]
    fn reasons_describe_what_works_and_the_licence_never_costs_points() {
        let (score_gpl, reasons) = score(&[Encoder::Nvenc, Encoder::X264], License::Gpl);
        assert_eq!(
            codes(&reasons),
            [
                ReasonCode::HwNvenc,
                ReasonCode::CpuX264,
                ReasonCode::LicenseGpl
            ]
        );
        assert_eq!(reasons[0].params["encoder"], "h264_nvenc");
        let (score_lgpl, _) = score(&[Encoder::Nvenc, Encoder::X264], License::Lgpl);
        assert_eq!(score_gpl, score_lgpl);

        let (_, weak) = score(&[Encoder::OpenH264], License::Lgpl);
        assert_eq!(
            codes(&weak),
            [ReasonCode::WeakFallbackOnly, ReasonCode::LicenseLgpl]
        );
        assert_eq!(weak[0].params["list"], "libopenh264");
        let (_, none) = score(&[], License::Unknown);
        assert_eq!(codes(&none), [ReasonCode::NoWorkingEncoder]);
    }

    #[test]
    fn versions_compare_numerically() {
        assert!(version_key("n9.0.2-22-gabc") > version_key("7.1-full_build"));
        assert!(version_key("8.0") > version_key("7.1.1"));
        assert!(version_key("7.10") > version_key("7.9"));
        assert_eq!(version_key("2025-10-01-git-abc"), [2025, 10, 1]);
        assert_eq!(version_key("N-124279-g0f6ba39122-20260430"), [2026, 4, 30]);
        assert!(version_key("N-124279-g0f6ba39122-20260430") > version_key("8.0.1-full_build"));
        assert!(version_key("garbage").is_empty());
    }

    #[test]
    fn gpu_only_candidate_does_not_beat_the_download() {
        let mut list = vec![candidate("a", "7.1", License::Gpl, GPU_ONLY)];
        let r = recommend(&mut list, &DownloadProfile::assumed());
        assert_eq!(r.kind, Kind::Download);
        assert_eq!(r.candidate_dir, None);
        assert_eq!(
            codes(&r.reasons),
            [
                ReasonCode::DownloadNoGain,
                ReasonCode::DownloadHardwareAssumed
            ]
        );
        assert!(!list[0].recommended && list[0].score >= HW_FIRST);
    }

    #[test]
    fn libx264_only_candidate_is_recommended() {
        let mut list = vec![candidate("a", "7.1", License::Gpl, X264_ONLY)];
        let r = recommend(&mut list, &DownloadProfile::assumed());
        assert_eq!(r.kind, Kind::Candidate);
        assert_eq!(r.candidate_dir.as_deref(), Some("a"));
        assert_eq!(codes(&r.reasons), [ReasonCode::CandidateAddsX264]);
        assert!(list[0].recommended);
    }

    #[test]
    fn gpu_and_libx264_candidate_is_recommended_and_ranked_first() {
        let mut list = vec![
            candidate("gpu", "8.0", License::Gpl, GPU_ONLY),
            candidate(
                "both",
                "7.1",
                License::Gpl,
                &[Encoder::Nvenc, Encoder::X264],
            ),
            candidate("x264", "8.0", License::Gpl, X264_ONLY),
        ];
        let r = recommend(&mut list, &DownloadProfile::assumed());
        let order: Vec<_> = list.iter().map(|c| c.dir.as_str()).collect();
        assert_eq!(order, ["both", "gpu", "x264"]);
        assert_eq!(r.kind, Kind::Candidate);
        assert_eq!(r.candidate_dir.as_deref(), Some("both"));
        assert_eq!(list.iter().filter(|c| c.recommended).count(), 1);
    }

    #[test]
    fn no_working_encoder_means_the_download() {
        let mut list = vec![candidate("a", "7.1", License::Gpl, &[])];
        let r = recommend(&mut list, &DownloadProfile::assumed());
        assert_eq!(r.kind, Kind::Download);
        assert_eq!(list[0].score, 0);
        assert_eq!(codes(&list[0].reasons)[0], ReasonCode::NoWorkingEncoder);
        assert_eq!(
            r.reasons[0],
            Reason::plain(ReasonCode::DownloadNoGain),
            "unusable candidates still count as found"
        );

        let r = recommend(&mut [], &DownloadProfile::assumed());
        assert_eq!(r.kind, Kind::Download);
        assert_eq!(r.reasons[0].code, ReasonCode::DownloadNoCandidate);
    }

    #[test]
    fn a_candidate_with_hardware_the_measured_download_lacks_is_recommended() {
        // The installed LGPL build has no working GPU encoder; the candidate drives NVENC.
        let measured = DownloadProfile::measured(&[Encoder::OpenH264]);
        assert!(!measured.assumed && measured.hardware.is_empty());
        let mut list = vec![candidate("gpu", "7.1", License::Gpl, GPU_ONLY)];
        let r = recommend(&mut list, &measured);
        assert_eq!(r.kind, Kind::Candidate);
        assert_eq!(r.reasons[0].code, ReasonCode::CandidateAddsHardware);
        assert_eq!(r.reasons[0].params["encoder"], "h264_nvenc");

        // Same candidate, download measured with NVENC: no gain.
        let measured = DownloadProfile::measured(&[Encoder::Nvenc, Encoder::OpenH264]);
        let mut list = vec![candidate("gpu", "7.1", License::Gpl, GPU_ONLY)];
        let r = recommend(&mut list, &measured);
        assert_eq!(r.kind, Kind::Download);
        assert_eq!(r.reasons[1].code, ReasonCode::DownloadHardwareTested);
    }

    #[test]
    fn ties_go_to_the_newer_version_then_to_lgpl() {
        let mut list = vec![
            candidate("old", "7.1", License::Lgpl, X264_ONLY),
            candidate("new-gpl", "8.0", License::Gpl, X264_ONLY),
            candidate("new-lgpl", "8.0", License::Lgpl, X264_ONLY),
        ];
        recommend(&mut list, &DownloadProfile::assumed());
        let order: Vec<_> = list.iter().map(|c| c.dir.as_str()).collect();
        assert_eq!(order, ["new-lgpl", "new-gpl", "old"]);
        assert!(list[0].recommended);
        // Equal score, equal version, equal licence: the order of discovery is kept.
        let mut same = vec![
            candidate("first", "8.0", License::Gpl, X264_ONLY),
            candidate("second", "8.0", License::Gpl, X264_ONLY),
        ];
        recommend(&mut same, &DownloadProfile::assumed());
        assert_eq!(same[0].dir, "first");
    }

    #[test]
    fn a_weak_fallback_only_candidate_never_beats_the_download() {
        let mut list = vec![candidate(
            "weak",
            "9.0",
            License::Lgpl,
            &[Encoder::OpenH264, Encoder::MediaFoundation],
        )];
        let r = recommend(&mut list, &DownloadProfile::assumed());
        assert_eq!(r.kind, Kind::Download);
        assert_eq!(codes(&list[0].reasons)[0], ReasonCode::WeakFallbackOnly);
    }
}
