//! `debug_export`: a zip with `report.json` (versions, encoder detection,
//! last queue error codes, settings without paths or identity) and the recent journal files,
//! every path masked to `<path>/file-N` (one marker per file name in the archive) and secrets
//! scrubbed again. Only journal files are read
//! from the folder: never the TDLib log, never a database.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;

use serde_json::Value;
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

use super::file;
use super::record::Record;
use super::redact::{self, Masker};

/// Uncompressed journal bytes put in an export, newest files first.
pub const MAX_LOG_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExportSummary {
    pub files: usize,
    pub lines: usize,
    /// Lines that are not journal records (torn line after a crash…): left out.
    pub skipped_lines: usize,
}

/// One journal line with every string masked; `None` if it is not a journal record.
pub fn mask_line(line: &str, masker: &Masker) -> Option<String> {
    let value: Value = serde_json::from_str(line).ok()?;
    let mut record = Record::from_value(value)?;
    record.msg = masker.mask_text(&record.msg);
    let mut fields = Value::Object(std::mem::take(&mut record.fields));
    redact::map_strings(&mut fields, &|text| masker.mask_text(text));
    if let Value::Object(map) = fields {
        record.fields = map;
    }
    Some(record.to_line())
}

/// Writes the zip at `out` (through `out.part`, then renamed).
pub fn write_zip(out: &Path, report: &Value, log_dir: &Path) -> io::Result<ExportSummary> {
    let masker = Masker::default();
    let mut report = report.clone();
    redact::map_strings(&mut report, &|text| masker.mask_text(text));

    let mut chosen = Vec::new();
    let mut budget = MAX_LOG_BYTES;
    let mut files = file::list(log_dir).unwrap_or_default();
    files.reverse();
    for log in files {
        if log.size > budget && !chosen.is_empty() {
            break;
        }
        budget = budget.saturating_sub(log.size);
        chosen.push(log);
    }
    chosen.reverse();

    let part = out.with_extension("zip.part");
    let mut zip = zip::ZipWriter::new(File::create(&part)?);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let mut summary = ExportSummary::default();

    zip.start_file("report.json", options)
        .map_err(io::Error::other)?;
    zip.write_all(
        serde_json::to_string_pretty(&report)
            .unwrap_or_default()
            .as_bytes(),
    )?;

    for log in &chosen {
        let Some(name) = log.path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        zip.start_file(format!("logs/{name}"), options)
            .map_err(io::Error::other)?;
        let reader = BufReader::new(File::open(&log.path)?);
        for line in reader.lines() {
            // A torn or non-UTF-8 line is skipped, never copied raw.
            let Ok(line) = line else {
                summary.skipped_lines += 1;
                continue;
            };
            if line.trim().is_empty() {
                continue;
            }
            match mask_line(&line, &masker) {
                Some(masked) => {
                    zip.write_all(masked.as_bytes())?;
                    zip.write_all(b"\n")?;
                    summary.lines += 1;
                }
                None => summary.skipped_lines += 1,
            }
        }
        summary.files += 1;
    }
    zip.finish().map_err(io::Error::other)?;
    std::fs::rename(&part, out)?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::test_util::TestDir;
    use serde_json::json;
    use std::io::Read;

    #[test]
    fn export_masks_paths_and_holds_only_journal_files() {
        let dir = TestDir::new("log-export");
        let logs = dir.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let line = Record {
            ts: "2026-10-09T07:12:01.123Z".into(),
            level: "info".into(),
            target: "t".into(),
            event: "ffmpeg_start".into(),
            msg: "running C:\\Users\\Alice\\Videos\\clip.mp4".into(),
            session_id: "s".into(),
            item_id: Some(1),
            fields: serde_json::from_value(json!({
                "args": ["-i", "C:\\Users\\Alice\\Videos\\clip.mp4", "D:\\tmp\\1\\clip_sq.mp4.part"]
            }))
            .unwrap(),
            ..Default::default()
        }
        .to_line();
        std::fs::write(
            logs.join("squeezegram-2026-10-09.jsonl"),
            format!("{line}\n{{\"torn\n"),
        )
        .unwrap();
        std::fs::write(logs.join("td.log"), "TDLIB SECRET LOG").unwrap();
        std::fs::write(logs.join("squeezegram.db"), "DATABASE").unwrap();

        let out = dir.path().join("export.zip");
        let report = json!({ "versions": { "ffmpeg": { "ffmpeg_path": "C:\\Users\\Alice\\ff\\bin\\ffmpeg.exe" } } });
        let summary = write_zip(&out, &report, &logs).unwrap();
        assert_eq!(
            summary,
            ExportSummary {
                files: 1,
                lines: 1,
                skipped_lines: 1
            }
        );

        let mut zip = zip::ZipArchive::new(File::open(&out).unwrap()).unwrap();
        let names: Vec<String> = zip.file_names().map(str::to_owned).collect();
        assert_eq!(names, ["report.json", "logs/squeezegram-2026-10-09.jsonl"]);
        let mut all = String::new();
        for index in 0..zip.len() {
            zip.by_index(index)
                .unwrap()
                .read_to_string(&mut all)
                .unwrap();
        }
        assert!(!all.contains("Alice"), "{all}");
        assert!(!all.contains("TDLIB") && !all.contains("DATABASE"));
        assert!(
            !all.contains("clip.") && !all.contains("clip_") && !all.contains("ffmpeg.exe"),
            "{all}"
        );
        // One marker per name across the whole archive: the report, then the journal lines.
        assert_eq!(all.matches("<path>/file-1.exe").count(), 1);
        assert_eq!(all.matches("<path>/file-2.mp4").count(), 2);
        assert!(all.contains("<path>/file-3.part"));
        assert!(!out.with_extension("zip.part").exists());
    }
}
