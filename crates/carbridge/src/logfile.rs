// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Copies of the log in files of CarBridge's own. The system log on a router is a small ring that the kernel's
//! messages fill quickly; the file keeps CarBridge's lines for much longer, and the web interface shows it.
//!
//! While detailed logging is on, the lines also go to a detailed log in a directory on persistent storage, with
//! diagnostics that the other logs leave out ([`detail`]). It is kept in [`DETAIL_FILES`] files of [`DETAIL_SIZE`]
//! bytes at most, the oldest removed as a new one starts, and written in chunks rather than line by line, which spares
//! the storage's flash.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Once the file would grow past this size it becomes `<file>.1`, replacing the one before, and a new file starts.
const MAX_SIZE: u64 = 128 * 1024;
/// The name of the detailed log's newest file in its directory; the older ones are `<name>.1` (the newest of them)
/// to `<name>.<DETAIL_FILES - 1>`.
pub const DETAIL_NAME: &str = "carbridge.log";
/// How many files the detailed log keeps, the one written to among them.
pub const DETAIL_FILES: usize = 10;
/// The size of each of the detailed log's files, at most: 10 MiB in all.
pub const DETAIL_SIZE: u64 = 1024 * 1024;
/// How much of the detailed log waits in memory before it is written out.
const DETAIL_CHUNK: usize = 64 * 1024;
/// How long the detailed log's lines wait in memory at most, as long as [`tick`] is called often enough.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const DETAIL_DELAY: Duration = Duration::from_secs(5);

static FILE: OnceLock<Mutex<LogFile>> = OnceLock::new();
static DETAIL: Mutex<Option<Detail>> = Mutex::new(None);
/// Whether the detailed log is open: read without the lock, so that the lines only it takes cost nothing otherwise.
static DETAILED: AtomicBool = AtomicBool::new(false);

/// Starts copying the log to `path`, after what the file already holds.
pub fn open(path: &Path) -> io::Result<()> {
    let file = LogFile::open(path.to_owned(), MAX_SIZE, 1, false)?;
    FILE.set(Mutex::new(file))
        .map_err(|_| io::Error::other("the log file is already open"))
}

/// Adds a line to the file, if one is open, and to the detailed log while it is on, after the local time.
pub fn write(line: &str) {
    if let Some(file) = FILE.get() {
        lock(file).write(&format!("{} {line}\n", timestamp(false)));
    }
    detail(line);
}

/// Whether the detailed log is on.
pub fn detailed() -> bool {
    DETAILED.load(Ordering::Relaxed)
}

/// Adds a line to the detailed log only, while it is on.
pub fn detail(line: &str) {
    if !detailed() {
        return;
    }
    if let Some(detail) = lock(&DETAIL).as_mut() {
        detail.file.write(&format!("{} {line}\n", timestamp(true)));
    }
}

/// Turns the detailed log on in `dir`, after what its files already hold, or off with `None`. Says in the log when
/// it changes; one that cannot be opened stays off.
pub fn set_detailed(dir: Option<&Path>) {
    let mut detail = lock(&DETAIL);
    if detail.as_ref().map(|d| d.dir.as_path()) == dir {
        return;
    }
    if let Some(mut old) = detail.take() {
        DETAILED.store(false, Ordering::Relaxed);
        old.file
            .write(&format!("{} detailed log off\n", timestamp(true)));
        old.file.flush();
    }
    let Some(dir) = dir else {
        drop(detail);
        write("detailed log off");
        return;
    };
    match Detail::open(dir) {
        Ok(opened) => {
            *detail = Some(opened);
            DETAILED.store(true, Ordering::Relaxed);
            drop(detail);
            write(&format!(
                "detailed log on, in {}: {} files of {} KiB at most, the oldest removed first",
                dir.display(),
                DETAIL_FILES,
                DETAIL_SIZE / 1024
            ));
        }
        Err(err) => {
            drop(detail);
            write(&format!(
                "detailed log stays off: cannot write to {}: {err}",
                dir.display()
            ));
        }
    }
}

/// Writes out what the detailed log holds in memory once it has waited long enough; called every few seconds.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn tick() {
    if let Some(detail) = lock(&DETAIL).as_mut() {
        if detail
            .file
            .waiting_since()
            .is_some_and(|since| since.elapsed() >= DETAIL_DELAY)
        {
            detail.file.flush();
        }
    }
}

/// Writes out what the detailed log holds in memory, as before CarBridge exits.
pub fn flush() {
    if let Some(detail) = lock(&DETAIL).as_mut() {
        detail.file.flush();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The detailed log, open in its directory.
struct Detail {
    dir: PathBuf,
    file: LogFile,
}

impl Detail {
    fn open(dir: &Path) -> io::Result<Detail> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let file = LogFile::open(dir.join(DETAIL_NAME), DETAIL_SIZE, DETAIL_FILES - 1, true)?;
        Ok(Detail {
            dir: dir.to_owned(),
            file,
        })
    }
}

/// A log file that rotates: once it would grow past `max_size` it becomes `<file>.1`, `<file>.1` becomes `<file>.2`
/// and so on up to `<file>.<keep>`, which the one before replaces, and a new file starts.
struct LogFile {
    path: PathBuf,
    file: File,
    /// The size of the file with what waits in memory for it.
    size: u64,
    max_size: u64,
    keep: usize,
    /// Whether lines wait in memory to be written in chunks, rather than each as it comes.
    buffered: bool,
    pending: Vec<u8>,
    /// Since when the oldest line in `pending` has waited.
    pending_since: Option<Instant>,
}

impl LogFile {
    fn open(path: PathBuf, max_size: u64, keep: usize, buffered: bool) -> io::Result<LogFile> {
        let file = create(&path, false)?;
        let size = file.metadata()?.len();
        Ok(LogFile {
            path,
            file,
            size,
            max_size,
            keep: keep.max(1),
            buffered,
            pending: Vec::new(),
            pending_since: None,
        })
    }

    fn write(&mut self, text: &str) {
        if self.size > 0 && self.size + text.len() as u64 > self.max_size {
            self.flush();
            self.rotate();
        }
        if !self.buffered {
            if self.file.write_all(text.as_bytes()).is_ok() {
                self.size += text.len() as u64;
            }
            return;
        }
        self.pending.extend_from_slice(text.as_bytes());
        self.pending_since.get_or_insert_with(Instant::now);
        self.size += text.len() as u64;
        if self.pending.len() >= DETAIL_CHUNK {
            self.flush();
        }
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn waiting_since(&self) -> Option<Instant> {
        self.pending_since
    }

    /// Writes out what waits in memory. What cannot be written is dropped, not kept to grow.
    fn flush(&mut self) {
        if !self.pending.is_empty() {
            if self.file.write_all(&self.pending).is_err() {
                self.size -= self.pending.len() as u64;
            }
            self.pending.clear();
        }
        self.pending_since = None;
    }

    /// Moves each kept file one place on, the oldest giving way, keeps the full file as `<file>.1` and starts an empty
    /// one. If that fails, the lines go on to the old file.
    fn rotate(&mut self) {
        let numbered = |n: usize| {
            let mut path = self.path.clone().into_os_string();
            path.push(format!(".{n}"));
            PathBuf::from(path)
        };
        for n in (1..self.keep).rev() {
            let _ = fs::rename(numbered(n), numbered(n + 1));
        }
        if fs::rename(&self.path, numbered(1)).is_ok() {
            if let Ok(file) = create(&self.path, true) {
                self.file = file;
            }
        }
        self.size = 0;
    }
}

/// Opens the file for appending, readable by its owner only, since the log names devices.
fn create(path: &Path, truncate: bool) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(!truncate)
        .write(true)
        .truncate(truncate)
        .mode(0o600)
        .open(path)
}

/// The local time as `YYYY-MM-DD hh:mm:ss`, in the zone `TZ` names, with `.mmm` milliseconds when `millis` is set.
fn timestamp(millis: bool) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    // The type of the C library's time, which localtime_r takes.
    let seconds = now.as_secs() as _;
    // SAFETY: localtime_r fills the `tm` it is given and keeps no pointer to it.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&seconds, &mut tm).is_null() {
            return String::from("-");
        }
        tm
    };
    let stamp = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    );
    if millis {
        format!("{stamp}.{:03}", now.subsec_millis())
    } else {
        stamp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("carbridge-logfile-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The files in `dir`, by name, with their contents.
    fn files(dir: &Path) -> Vec<(String, String)> {
        let mut files: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (
                    entry.file_name().into_string().unwrap(),
                    fs::read_to_string(entry.path()).unwrap(),
                )
            })
            .collect();
        files.sort();
        files
    }

    #[test]
    fn appends_and_rotates() {
        let dir = scratch("rotate");
        let path = dir.join("carbridge.log");
        fs::write(&path, "old\n").unwrap();

        let mut log = LogFile::open(path.clone(), 12, 1, false).unwrap();
        log.write("one\n");
        assert_eq!(fs::read_to_string(&path).unwrap(), "old\none\n");
        log.write("two\n");
        log.write("three\n");
        assert_eq!(
            fs::read_to_string(dir.join("carbridge.log.1")).unwrap(),
            "old\none\ntwo\n"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "three\n");

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_line_longer_than_the_file_is_still_written() {
        let dir = scratch("long");
        let path = dir.join("carbridge.log");
        let mut log = LogFile::open(path.clone(), 4, 1, false).unwrap();
        log.write("a long line\n");
        assert_eq!(fs::read_to_string(&path).unwrap(), "a long line\n");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rolls_over_keeping_a_fixed_number_of_files_the_oldest_removed_first() {
        let dir = scratch("roll");
        let path = dir.join("d.log");
        // Files of two lines of 2 bytes: the newest and two older ones.
        let mut log = LogFile::open(path.clone(), 4, 2, false).unwrap();
        for line in ["a\n", "b\n", "c\n", "d\n", "e\n", "f\n", "g\n"] {
            log.write(line);
        }
        assert_eq!(
            files(&dir),
            [
                ("d.log".to_owned(), "g\n".to_owned()),
                ("d.log.1".to_owned(), "e\nf\n".to_owned()),
                ("d.log.2".to_owned(), "c\nd\n".to_owned()),
            ]
        );
        // "a" and "b" gave way first; the rest goes on in order.
        for line in ["h\n", "i\n"] {
            log.write(line);
        }
        assert_eq!(
            files(&dir),
            [
                ("d.log".to_owned(), "i\n".to_owned()),
                ("d.log.1".to_owned(), "g\nh\n".to_owned()),
                ("d.log.2".to_owned(), "e\nf\n".to_owned()),
            ]
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_total_stays_under_the_cap() {
        let dir = scratch("cap");
        let path = dir.join("d.log");
        let (max_size, keep) = (100, 4);
        let mut log = LogFile::open(path.clone(), max_size, keep, true).unwrap();
        for n in 0..1000 {
            log.write(&format!("line {n}\n"));
        }
        log.flush();
        let files = files(&dir);
        assert_eq!(files.len(), keep + 1);
        let total: usize = files.iter().map(|(_, text)| text.len()).sum();
        assert!(total as u64 <= max_size * (keep as u64 + 1), "{total}");
        for (_, text) in &files {
            assert!(text.len() as u64 <= max_size);
        }
        // The newest lines are kept, in order, across the files from the oldest to the newest.
        let mut joined = String::new();
        for n in (1..=keep).rev() {
            joined += &fs::read_to_string(dir.join(format!("d.log.{n}"))).unwrap();
        }
        joined += &fs::read_to_string(&path).unwrap();
        assert!(joined.ends_with("line 998\nline 999\n"));
        let numbers: Vec<u32> = joined
            .lines()
            .map(|line| line["line ".len()..].parse().unwrap())
            .collect();
        assert!(numbers.windows(2).all(|pair| pair[1] == pair[0] + 1));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn buffered_lines_wait_for_a_flush_or_a_full_chunk() {
        let dir = scratch("buffer");
        let path = dir.join("d.log");
        let mut log = LogFile::open(path.clone(), DETAIL_SIZE, 2, true).unwrap();
        log.write("one\n");
        assert_eq!(fs::read_to_string(&path).unwrap(), "");
        assert!(log.waiting_since().is_some());
        log.flush();
        assert_eq!(fs::read_to_string(&path).unwrap(), "one\n");
        assert!(log.waiting_since().is_none());
        let line = "x".repeat(DETAIL_CHUNK);
        log.write(&line);
        assert_eq!(fs::read_to_string(&path).unwrap().len(), 4 + DETAIL_CHUNK);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_reopened_log_goes_on_after_what_it_holds() {
        let dir = scratch("reopen");
        let path = dir.join("d.log");
        let mut log = LogFile::open(path.clone(), 8, 1, true).unwrap();
        log.write("abc\n");
        log.flush();
        drop(log);
        let mut log = LogFile::open(path.clone(), 8, 1, true).unwrap();
        log.write("def\n");
        log.write("ghi\n");
        log.flush();
        assert_eq!(
            fs::read_to_string(dir.join("d.log.1")).unwrap(),
            "abc\ndef\n"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "ghi\n");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn timestamp_shape() {
        let stamp = timestamp(false);
        assert_eq!(stamp.len(), 19, "{stamp}");
        assert_eq!(&stamp[4..5], "-");
        assert_eq!(&stamp[10..11], " ");
        assert_eq!(&stamp[13..14], ":");
        let stamp = timestamp(true);
        assert_eq!(stamp.len(), 23, "{stamp}");
        assert_eq!(&stamp[19..20], ".");
    }
}
