//! Caption templates: `{name}`-style variables replaced per item, right before the send,
//! when the final file is known. Plain text in, plain text out; nothing is ever evaluated.
//!
//! Unknown variables stay as written (a typo is visible in the message instead of vanishing),
//! `{{` and `}}` produce literal braces, and a variable without a value (`{saving}` of an
//! uncompressed file) is replaced by nothing.

use std::path::Path;

/// What the variables are made of. Raw values: the formatting is done here, once, so the
/// preview and the send cannot drift apart.
#[derive(Debug, Clone, PartialEq)]
pub struct Fields<'a> {
    /// Name of the file the user queued (not the `_sq` copy that is sent: that name means
    /// nothing to the people reading the caption).
    pub file_name: &'a str,
    pub duration_s: Option<u32>,
    pub original_size: u64,
    pub final_size: u64,
    /// Displayed size of a video or photo; `None` for a document.
    pub resolution: Option<(u32, u32)>,
    /// Send date, local, ISO `yyyy-mm-dd`.
    pub date: &'a str,
    /// 1-based position of the file in its batch, and the batch size.
    pub index: u32,
    pub count: u32,
}

pub const VARIABLES: [&str; 11] = [
    "name",
    "file",
    "ext",
    "duration",
    "original_size",
    "final_size",
    "saving",
    "resolution",
    "date",
    "index",
    "count",
];

impl Fields<'_> {
    fn value(&self, variable: &str) -> Option<String> {
        let path = Path::new(self.file_name);
        Some(match variable {
            "name" => path
                .file_stem()
                .map_or_else(String::new, |s| s.to_string_lossy().into_owned()),
            "file" => self.file_name.to_owned(),
            "ext" => path
                .extension()
                .map_or_else(String::new, |s| s.to_string_lossy().into_owned()),
            "duration" => self.duration_s.map(duration).unwrap_or_default(),
            "original_size" => size(self.original_size),
            "final_size" => size(self.final_size),
            "saving" => saving(self.original_size, self.final_size),
            "resolution" => self
                .resolution
                .map(|(w, h)| format!("{w}x{h}"))
                .unwrap_or_default(),
            "date" => self.date.to_owned(),
            "index" => self.index.to_string(),
            "count" => self.count.to_string(),
            _ => return None,
        })
    }
}

/// `h:mm:ss`, or `m:ss` under an hour.
fn duration(seconds: u32) -> String {
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// 1024 steps and KB/MB/GB labels with at most one decimal, as the app shows sizes
/// (`formatBytes` in `src/lib/format.ts`), in English: the language of the caption is the
/// user's, not the interface's, and a send does not know the latter.
fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut exponent = 0;
    let mut value = bytes as f64;
    while value >= 1024.0 && exponent < UNITS.len() - 1 {
        value /= 1024.0;
        exponent += 1;
    }
    if exponent == 0 {
        return format!("{bytes} B");
    }
    let rounded = (value * 10.0).round() / 10.0;
    // 1023.96 KB rounds to 1024: say 1 MB instead.
    if rounded >= 1024.0 && exponent < UNITS.len() - 1 {
        return format!("1 {}", UNITS[exponent + 1]);
    }
    if rounded.fract() == 0.0 {
        format!("{rounded:.0} {}", UNITS[exponent])
    } else {
        format!("{rounded:.1} {}", UNITS[exponent])
    }
}

/// Percent saved by the compression; empty when the file did not get smaller.
fn saving(original: u64, sent: u64) -> String {
    if original == 0 || sent >= original {
        return String::new();
    }
    let percent = (1.0 - sent as f64 / original as f64) * 100.0;
    format!("{}%", percent.round() as u64)
}

pub fn render(template: &str, fields: &Fields<'_>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(at) = rest.find(['{', '}']) {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        if let Some(after) = tail.strip_prefix("{{") {
            out.push('{');
            rest = after;
        } else if let Some(after) = tail.strip_prefix("}}") {
            out.push('}');
            rest = after;
        } else if let Some(inner) = tail.strip_prefix('{') {
            // A name runs up to the next brace, which must close it: `{a{b}` keeps the first
            // brace as text and reads `{b}` on its own.
            match inner.find(['{', '}']) {
                Some(end) if inner.as_bytes()[end] == b'}' => {
                    match fields.value(&inner[..end]) {
                        Some(value) => out.push_str(&value),
                        None => out.push_str(&tail[..end + 2]),
                    }
                    rest = &inner[end + 1..];
                }
                _ => {
                    out.push('{');
                    rest = inner;
                }
            }
        } else {
            out.push('}');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}

/// The caption of the first file as the add panel shows it, before anything is encoded.
/// The name, the original size, the date and the count are real; the duration, the resolution
/// and the sizes after compression are samples (the panel says so).
pub fn preview(template: &str, file: Option<(&str, u64)>, count: u32, today: &str) -> String {
    let (file_name, original_size) = file.unwrap_or(("video.mp4", 48 * 1024 * 1024));
    render(
        template,
        &Fields {
            file_name,
            duration_s: Some(205),
            original_size,
            final_size: original_size / 5 * 2,
            resolution: Some((1920, 1080)),
            date: today,
            index: 1,
            count: count.max(1),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields() -> Fields<'static> {
        Fields {
            file_name: "Holiday clip.final.mkv",
            duration_s: Some(3725),
            original_size: 200 * 1024 * 1024,
            final_size: 50 * 1024 * 1024,
            resolution: Some((1920, 1080)),
            date: "2026-10-12",
            index: 2,
            count: 5,
        }
    }

    #[test]
    fn every_variable_is_replaced() {
        let f = fields();
        let expected = [
            ("name", "Holiday clip.final"),
            ("file", "Holiday clip.final.mkv"),
            ("ext", "mkv"),
            ("duration", "1:02:05"),
            ("original_size", "200 MB"),
            ("final_size", "50 MB"),
            ("saving", "75%"),
            ("resolution", "1920x1080"),
            ("date", "2026-10-12"),
            ("index", "2"),
            ("count", "5"),
        ];
        assert_eq!(expected.len(), VARIABLES.len());
        for (variable, value) in expected {
            assert_eq!(render(&format!("{{{variable}}}"), &f), value, "{variable}");
        }
    }

    #[test]
    fn variables_mix_with_text() {
        assert_eq!(
            render(
                "{name} ({index}/{count}) - {resolution}, {duration}",
                &fields()
            ),
            "Holiday clip.final (2/5) - 1920x1080, 1:02:05"
        );
    }

    #[test]
    fn unknown_variables_stay_as_written() {
        let f = fields();
        for text in [
            "{nope}", "{Name}", "{}", "{ name }", "{name", "name}", "{1+1}",
        ] {
            assert_eq!(render(text, &f), text);
        }
        assert_eq!(render("{a{name}", &f), "{aHoliday clip.final");
        assert_eq!(render("{name}}", &f), "Holiday clip.final}");
    }

    #[test]
    fn doubled_braces_are_literal() {
        let f = fields();
        assert_eq!(render("{{name}}", &f), "{name}");
        assert_eq!(render("{{{name}}}", &f), "{Holiday clip.final}");
        assert_eq!(render("a {{ b }} c", &f), "a { b } c");
    }

    #[test]
    fn nothing_is_evaluated() {
        // Values are inserted as text and never rescanned.
        let f = Fields {
            file_name: "{count}.mp4",
            ..fields()
        };
        assert_eq!(render("{name}", &f), "{count}");
    }

    #[test]
    fn missing_values_are_empty() {
        let f = Fields {
            duration_s: None,
            resolution: None,
            final_size: 200 * 1024 * 1024,
            file_name: "notes",
            ..fields()
        };
        assert_eq!(
            render("[{duration}][{resolution}][{saving}][{ext}]", &f),
            "[][][][]"
        );
        assert_eq!(render("{name}", &f), "notes");
    }

    #[test]
    fn a_larger_or_empty_original_saves_nothing() {
        assert_eq!(saving(100, 120), "");
        assert_eq!(saving(0, 0), "");
        assert_eq!(saving(1000, 999), "0%");
        assert_eq!(saving(1000, 1), "100%");
    }

    #[test]
    fn unicode_names_are_kept() {
        let f = Fields {
            file_name: "été ★ 日本語.mov",
            ..fields()
        };
        assert_eq!(
            render("{name}|{ext}|{file}", &f),
            "été ★ 日本語|mov|été ★ 日本語.mov"
        );
        assert_eq!(render("★ {{é}} ★", &f), "★ {é} ★");
    }

    #[test]
    fn the_preview_uses_the_real_file_and_samples_for_the_rest() {
        let text = preview(
            "{file} {original_size} {final_size} {saving} {duration} {resolution} {index}/{count}",
            Some(("a.mkv", 100 * 1024)),
            3,
            "2026-10-12",
        );
        assert_eq!(text, "a.mkv 100 KB 40 KB 60% 3:25 1920x1080 1/3");
        assert_eq!(preview("{name}|{count}", None, 0, "2026-10-12"), "video|1");
    }

    #[test]
    fn sizes_read_like_the_app() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(1023), "1023 B");
        assert_eq!(size(1024), "1 KB");
        assert_eq!(size(1536), "1.5 KB");
        assert_eq!(size(1024 * 1024 + 1024 * 52), "1.1 MB");
        assert_eq!(size(5 * 1024 * 1024 * 1024), "5 GB");
        assert_eq!(size(1024 * 1024 - 1), "1 MB");
    }

    #[test]
    fn durations_use_hours_only_when_needed() {
        assert_eq!(duration(0), "0:00");
        assert_eq!(duration(59), "0:59");
        assert_eq!(duration(61), "1:01");
        assert_eq!(duration(3600), "1:00:00");
        assert_eq!(duration(36000), "10:00:00");
    }
}
