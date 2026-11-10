//! The usage log as a file of its own (`wye serve --usage-log`): appended,
//! created readable only by its owner, and rotated by the writer task at
//! [`ROTATE_BYTES`]. A rotated file is renamed beside the log
//! ([`log_files::rotated`]) and never deleted: what to keep is the
//! operator's choice.

use std::{
    io,
    path::{Path, PathBuf},
};

use tokio::io::AsyncWriteExt;

use super::unix_nanos;
use crate::log_files;

/// Size at which the log is rotated: the line that reaches it is finished
/// first, so a record never spans two files.
pub(crate) const ROTATE_BYTES: u64 = 64 << 20;

/// The usage log file, rotated at a size.
pub struct UsageFile {
    path: PathBuf,
    file: tokio::fs::File,
    /// Bytes in the current file.
    size: u64,
    /// The size each file is rotated at.
    max: u64,
    /// The size the current file is rotated at: `max`, or further on after a
    /// rotation failed, so a failing rename is retried once per `max` bytes,
    /// not on every write.
    limit: u64,
    /// Unix seconds in the last rotated file's name. Two rotations within a
    /// second take successive seconds, so names stay unique and in order.
    last: u64,
    /// Whether a failed rotation was reported; only the first is.
    reported: bool,
}

impl UsageFile {
    /// Opens the usage log at `path`, appending, created with mode 600.
    pub fn open(path: &Path) -> io::Result<Self> {
        Self::with_max(path, ROTATE_BYTES)
    }

    pub(super) fn with_max(path: &Path, max: u64) -> io::Result<Self> {
        let file = create(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            path: path.to_owned(),
            file: tokio::fs::File::from_std(file),
            size,
            max,
            limit: max,
            last: 0,
            reported: false,
        })
    }

    /// Writes `buf`, whole lines, rotating where a line reaches the limit.
    pub(super) async fn write_lines(&mut self, buf: &[u8]) -> io::Result<()> {
        let mut rest = buf;
        loop {
            if self.size >= self.limit {
                self.file.flush().await?;
                self.rotate().await;
            }
            if rest.is_empty() {
                break;
            }
            let (now, later) = rest
                .split_at_checked(cut(rest, self.limit.saturating_sub(self.size)))
                .unwrap_or((rest, &[]));
            self.file.write_all(now).await?;
            self.size = self.size.saturating_add(now.len() as u64);
            rest = later;
        }
        self.file.flush().await
    }

    /// Renames the log beside itself and opens a new one at its path. On
    /// failure the current file goes on taking records, and the first
    /// failure is reported.
    async fn rotate(&mut self) {
        let now = unix_nanos() / 1_000_000_000;
        let at = now.max(self.last.saturating_add(1));
        let path = self.path.clone();
        let rotated = tokio::task::spawn_blocking(move || {
            // A name already taken, by an earlier run, is never replaced.
            let mut at = at;
            let to = loop {
                let to = log_files::rotated(&path, at);
                match std::fs::symlink_metadata(&to) {
                    Err(e) if e.kind() == io::ErrorKind::NotFound => break to,
                    Err(e) => return Err(e),
                    Ok(_) => at = at.saturating_add(1),
                }
            };
            std::fs::rename(&path, &to)?;
            Ok((create(&path)?, at))
        })
        .await
        .unwrap_or_else(|e| Err(io::Error::other(e)));
        match rotated {
            Ok((file, at)) => {
                self.file = tokio::fs::File::from_std(file);
                self.size = 0;
                self.limit = self.max;
                self.last = at;
            }
            Err(e) => {
                if !self.reported {
                    self.reported = true;
                    eprintln!(
                        "wye: cannot rotate the usage log {}: {e}; it keeps growing",
                        self.path.display()
                    );
                }
                self.limit = self.size.saturating_add(self.max);
            }
        }
    }
}

/// Opens `path` for appending, creating it readable only by its owner.
fn create(path: &Path) -> io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

/// How much of `rest` (whole lines) to write when `room` bytes are left
/// before the limit: through the end of the line that reaches it, else all.
fn cut(rest: &[u8], room: u64) -> usize {
    let from = usize::try_from(room)
        .unwrap_or(usize::MAX)
        .saturating_sub(1);
    rest.get(from..)
        .and_then(|tail| tail.iter().position(|&b| b == b'\n'))
        .map_or(rest.len(), |i| from + i + 1)
}

#[cfg(test)]
mod tests {
    use wye_testkit::TempDir;

    use super::*;

    fn lines(range: std::ops::Range<usize>) -> Vec<String> {
        // Lengths vary, so lines cross the limit at every offset.
        range
            .map(|i| format!("{{\"n\":{i},\"pad\":\"{}\"}}\n", "x".repeat(i % 37)))
            .collect()
    }

    /// The log's files, oldest first: rotated ones by name, then the log.
    fn files(dir: &Path) -> Vec<PathBuf> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_line_is_cut_where_it_reaches_the_limit() {
        let buf = b"aaaa\nbb\ncc\n";
        assert_eq!(cut(buf, 1), 5, "the first line reaches it");
        assert_eq!(cut(buf, 5), 5, "it ends exactly at the limit");
        assert_eq!(cut(buf, 6), 8);
        assert_eq!(cut(buf, 100), buf.len());
        assert_eq!(cut(buf, 0), 5);
        assert_eq!(cut(b"no newline", 3), 10);
    }

    #[tokio::test]
    async fn rotation_keeps_records_whole_in_order_and_deletes_nothing() {
        let dir = TempDir::new("rotation");
        let path = dir.join("usage.jsonl");
        let max = 1000;
        let mut log = UsageFile::with_max(&path, max).unwrap();
        let written = lines(0..2000);
        // Batches of 1 to 30 lines, as the writer sends them.
        let mut i = 0;
        while i < written.len() {
            let end = (i + 1 + i % 30).min(written.len());
            log.write_lines(written[i..end].concat().as_bytes())
                .await
                .unwrap();
            i = end;
        }
        let files = files(&dir);
        let longest = written.iter().map(String::len).max().unwrap() as u64;
        let (log_file, rotated) = files.split_last().unwrap();
        assert_eq!(log_file, &path, "the log sorts after its rotated files");
        assert!(rotated.len() > 50, "{}", rotated.len());
        let mut read = Vec::new();
        for file in &files {
            let text = std::fs::read_to_string(file).unwrap();
            assert!(text.is_empty() || text.ends_with('\n'), "{file:?}");
            if file != &path {
                let name = file.file_name().unwrap().to_str().unwrap();
                assert!(log_files::is_rotated(&path, name), "{name}");
                let size = text.len() as u64;
                assert!(size >= max && size < max + longest, "{name}: {size}");
            }
            read.extend(text.split_inclusive('\n').map(str::to_owned));
        }
        assert_eq!(read, written, "every record once, whole, in order");
        #[cfg(unix)]
        for file in &files {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{file:?}: {mode:o}");
        }
    }

    #[tokio::test]
    async fn an_existing_log_is_appended_and_rotated_names_are_never_replaced() {
        let dir = TempDir::new("rotation-kept");
        let path = dir.join("usage.jsonl");
        std::fs::write(&path, "an old line past the limit\n").unwrap();
        // Names the next rotations would take, from an earlier run.
        let now = unix_nanos() / 1_000_000_000;
        let taken: Vec<_> = (now..now + 5)
            .map(|at| log_files::rotated(&path, at))
            .collect();
        for file in &taken {
            std::fs::write(file, "kept\n").unwrap();
        }
        let mut log = UsageFile::with_max(&path, 10).unwrap();
        log.write_lines(b"a new line\nanother\n").await.unwrap();
        for file in &taken {
            assert_eq!(std::fs::read_to_string(file).unwrap(), "kept\n");
        }
        let files = files(&dir);
        assert_eq!(files.len(), taken.len() + 3, "{files:?}");
        let text: Vec<_> = files
            .iter()
            .filter(|f| !taken.contains(f))
            .map(|f| std::fs::read_to_string(f).unwrap())
            .collect();
        // The old file was already at the limit, so it rotated first.
        assert_eq!(
            text,
            ["an old line past the limit\n", "a new line\n", "another\n"]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_log_that_cannot_rotate_keeps_its_records() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("rotation-denied");
        let path = dir.join("usage.jsonl");
        let mut log = UsageFile::with_max(&path, 10).unwrap();
        std::fs::set_permissions(&*dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        // A superuser renames anyway; there is nothing to test then.
        let denied = std::fs::write(dir.join("probe"), "").is_err();
        let written = lines(0..50).concat();
        log.write_lines(written.as_bytes()).await.unwrap();
        std::fs::set_permissions(&*dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        if denied {
            assert_eq!(files(&dir), std::slice::from_ref(&path));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), written);
            assert!(log.reported);
            // Retried after another `max` bytes, not on every write.
            assert!(log.limit > log.size, "{} {}", log.limit, log.size);
        }
    }
}
