//! Rolling journal files: `squeezegram-YYYY-MM-DD.jsonl` (UTC date), a new part
//! `squeezegram-YYYY-MM-DD.N.jsonl` when a file reaches its size cap, then the retention
//! (age and total size) applied at every rotation. Lines are written straight to the file
//! (no buffer), so the last line before a crash is on disk.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use chrono::{Datelike, NaiveDate};

pub const PREFIX: &str = "squeezegram-";
pub const EXTENSION: &str = ".jsonl";

/// Size and age caps of the journal folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    /// A file reaching this size continues in the next part.
    pub max_file_bytes: u64,
    /// Every journal file together, the current one included.
    pub max_total_bytes: u64,
    /// A file whose date is this many days old or more is deleted.
    pub max_age_days: i64,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            max_file_bytes: 10 * 1024 * 1024,
            max_total_bytes: 50 * 1024 * 1024,
            max_age_days: 7,
        }
    }
}

/// A journal file found in the folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogFile {
    pub date: NaiveDate,
    pub part: u32,
    pub path: PathBuf,
    pub size: u64,
}

pub fn file_name(date: NaiveDate, part: u32) -> String {
    let day = format!("{:04}-{:02}-{:02}", date.year(), date.month(), date.day());
    if part == 0 {
        format!("{PREFIX}{day}{EXTENSION}")
    } else {
        format!("{PREFIX}{day}.{part}{EXTENSION}")
    }
}

/// Date and part of a journal file name; `None` for any other file (TDLib log, database…).
pub fn parse_name(name: &str) -> Option<(NaiveDate, u32)> {
    let rest = name.strip_prefix(PREFIX)?.strip_suffix(EXTENSION)?;
    let (day, part) = match rest.split_once('.') {
        Some((day, part)) if !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()) => {
            (day, part.parse().ok()?)
        }
        Some(_) => return None,
        None => (rest, 0),
    };
    let mut pieces = day.split('-');
    let (year, month, date) = (pieces.next()?, pieces.next()?, pieces.next()?);
    if pieces.next().is_some() || year.len() != 4 || month.len() != 2 || date.len() != 2 {
        return None;
    }
    let date =
        NaiveDate::from_ymd_opt(year.parse().ok()?, month.parse().ok()?, date.parse().ok()?)?;
    Some((date, part))
}

/// Journal files of `dir`, oldest first.
pub fn list(dir: &Path) -> io::Result<Vec<LogFile>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some((date, part)) = name.to_str().and_then(parse_name) else {
            continue;
        };
        // Not `entry.metadata()`: on NTFS the directory entry of a file still open for
        // writing (this session's, or another instance's) reports a stale size.
        let metadata = fs::metadata(entry.path())?;
        if !metadata.is_file() {
            continue;
        }
        files.push(LogFile {
            date,
            part,
            path: entry.path(),
            size: metadata.len(),
        });
    }
    files.sort_by_key(|file| (file.date, file.part));
    Ok(files)
}

struct Current {
    date: NaiveDate,
    part: u32,
    path: PathBuf,
    file: File,
    size: u64,
}

pub struct RollingFile {
    dir: PathBuf,
    retention: Retention,
    current: Option<Current>,
}

impl RollingFile {
    /// Creates the folder; the first line opens (or continues) today's file.
    pub fn open(dir: PathBuf, retention: Retention) -> io::Result<Self> {
        fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            retention,
            current: None,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn current_path(&self) -> Option<&Path> {
        self.current.as_ref().map(|c| c.path.as_path())
    }

    /// Appends `line` and a newline to the file of `date`, rotating first when needed.
    pub fn write_line(&mut self, date: NaiveDate, line: &str) -> io::Result<()> {
        let needed = line.len() as u64 + 1;
        let rotate = match &self.current {
            None => true,
            Some(current) => {
                current.date != date
                    || (current.size > 0 && current.size + needed > self.retention.max_file_bytes)
            }
        };
        if rotate {
            self.rotate(date, needed)?;
        }
        let Some(current) = self.current.as_mut() else {
            return Err(io::Error::other("no journal file"));
        };
        let mut buffer = Vec::with_capacity(line.len() + 1);
        buffer.extend_from_slice(line.as_bytes());
        buffer.push(b'\n');
        // One write per line: lines of concurrent sessions never interleave mid-line.
        current.file.write_all(&buffer)?;
        current.size += needed;
        Ok(())
    }

    fn rotate(&mut self, date: NaiveDate, needed: u64) -> io::Result<()> {
        let mut part = match &self.current {
            Some(current) if current.date == date => current.part + 1,
            // Continue the last part of that day (an earlier session of the same day).
            _ => list(&self.dir)?
                .iter()
                .filter(|file| file.date == date)
                .map(|file| file.part)
                .max()
                .unwrap_or(0),
        };
        let (path, size) = loop {
            let path = self.dir.join(file_name(date, part));
            let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if size == 0 || size + needed <= self.retention.max_file_bytes {
                break (path, size);
            }
            part += 1;
        };
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        self.current = Some(Current {
            date,
            part,
            path,
            file,
            size,
        });
        // Retention problems never stop the journal.
        let _ = self.sweep(date);
        Ok(())
    }

    /// Deletes files `max_age_days` old or more, then the oldest ones until the others fit in
    /// `max_total_bytes` minus one file cap (room for the current file to grow). The current
    /// file is never deleted.
    pub fn sweep(&self, today: NaiveDate) -> io::Result<()> {
        let current = self.current_path().map(Path::to_path_buf);
        let mut kept = Vec::new();
        for file in list(&self.dir)? {
            if Some(&file.path) == current.as_ref() {
                continue;
            }
            if (today - file.date).num_days() >= self.retention.max_age_days {
                fs::remove_file(&file.path)?;
            } else {
                kept.push(file);
            }
        }
        let budget = self
            .retention
            .max_total_bytes
            .saturating_sub(self.retention.max_file_bytes);
        let mut total: u64 = kept.iter().map(|file| file.size).sum();
        for file in kept {
            if total <= budget {
                break;
            }
            fs::remove_file(&file.path)?;
            total -= file.size;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::test_util::TestDir;

    fn day(text: &str) -> NaiveDate {
        parse_name(&format!("{PREFIX}{text}{EXTENSION}")).unwrap().0
    }

    #[test]
    fn names_are_dated_and_numbered() {
        let date = NaiveDate::from_ymd_opt(2026, 10, 9).unwrap();
        assert_eq!(file_name(date, 0), "squeezegram-2026-10-09.jsonl");
        assert_eq!(file_name(date, 3), "squeezegram-2026-10-09.3.jsonl");
        assert_eq!(parse_name("squeezegram-2026-10-09.jsonl"), Some((date, 0)));
        assert_eq!(
            parse_name("squeezegram-2026-10-09.3.jsonl"),
            Some((date, 3))
        );
        for other in [
            "td.log",
            "squeezegram.db",
            "squeezegram-2026-10-09.log",
            "squeezegram-2026-13-01.jsonl",
            "squeezegram-2026-10-09.x.jsonl",
            "squeezegram-26-10-09.jsonl",
        ] {
            assert_eq!(parse_name(other), None, "{other}");
        }
    }

    #[test]
    fn rotates_by_day_and_by_size_and_appends_to_an_existing_day() {
        let dir = TestDir::new("log-rotate");
        let retention = Retention {
            max_file_bytes: 100,
            max_total_bytes: 10_000,
            max_age_days: 7,
        };
        let mut log = RollingFile::open(dir.path().to_path_buf(), retention).unwrap();
        let line = "x".repeat(39); // 40 bytes with the newline
        let first = day("2026-10-08");
        for _ in 0..3 {
            log.write_line(first, &line).unwrap();
        }
        let second = day("2026-10-09");
        log.write_line(second, &line).unwrap();
        let names: Vec<String> = list(dir.path())
            .unwrap()
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            [
                "squeezegram-2026-10-08.jsonl",
                "squeezegram-2026-10-08.1.jsonl",
                "squeezegram-2026-10-09.jsonl"
            ]
        );
        let sizes: Vec<u64> = list(dir.path()).unwrap().iter().map(|f| f.size).collect();
        assert_eq!(sizes, [80, 40, 40], "never above the file cap");

        // A new session the same day continues the last part.
        drop(log);
        let mut again = RollingFile::open(dir.path().to_path_buf(), retention).unwrap();
        again.write_line(second, &line).unwrap();
        assert_eq!(
            again.current_path().unwrap().file_name().unwrap(),
            "squeezegram-2026-10-09.jsonl"
        );
        assert_eq!(
            std::fs::metadata(again.current_path().unwrap())
                .unwrap()
                .len(),
            80
        );
    }

    #[test]
    fn retention_deletes_old_days_then_the_oldest_over_the_total_and_nothing_else() {
        let dir = TestDir::new("log-retention");
        for (name, size) in [
            ("squeezegram-2026-09-30.jsonl", 10),  // 9 days old
            ("squeezegram-2026-10-02.jsonl", 10),  // 7 days old: gone
            ("squeezegram-2026-10-03.jsonl", 300), // 6 days old: over the total, gone
            ("squeezegram-2026-10-04.jsonl", 300),
            ("squeezegram-2026-10-05.jsonl", 300),
        ] {
            dir.file(name, &vec![b'x'; size]);
        }
        dir.file("td.log", &[b'x'; 5000]);
        dir.file("squeezegram.db", b"x");
        let retention = Retention {
            max_file_bytes: 300,
            max_total_bytes: 1000,
            max_age_days: 7,
        };
        let mut log = RollingFile::open(dir.path().to_path_buf(), retention).unwrap();
        log.write_line(day("2026-10-09"), "{}").unwrap();
        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "squeezegram-2026-10-04.jsonl",
                "squeezegram-2026-10-05.jsonl",
                "squeezegram-2026-10-09.jsonl",
                "squeezegram.db",
                "td.log"
            ]
        );
        let total: u64 = list(dir.path()).unwrap().iter().map(|f| f.size).sum();
        assert!(total <= retention.max_total_bytes, "{total}");
    }
}
