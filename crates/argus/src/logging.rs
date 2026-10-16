//! Tracing setup: a size-rolling file under `<argus_home>/logs/argus.log`,
//! plus stderr in dev builds.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::Context as _;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;

/// Roll `argus.log` once it passes this many bytes.
const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;
/// How many rotated files to keep (`argus.log.1` .. `argus.log.N`).
const MAX_ROTATED_FILES: usize = 5;

/// Installs the global tracing subscriber.
///
/// Always writes to `<argus_home>/logs/argus.log`, creating the directory and
/// the file on first launch. Dev builds also log to stderr. `RUST_LOG`
/// overrides the level, which defaults to `info`.
pub fn init(argus_home: &Path) -> anyhow::Result<()> {
    let log_dir = argus_home.join("logs");
    fs::create_dir_all(&log_dir)
        .with_context(|| format!("failed to create {}", log_dir.display()))?;

    let log_path = log_dir.join("argus.log");
    let writer = RollingWriter::new(log_path.clone(), MAX_LOG_BYTES, MAX_ROTATED_FILES)
        .with_context(|| format!("failed to open {}", log_path.display()))?;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(writer);

    let subscriber = tracing_subscriber::registry().with(filter).with(file_layer);

    // Stderr is a dev convenience; release builds rely on the log file.
    if cfg!(debug_assertions) {
        subscriber
            .with(tracing_subscriber::fmt::layer().with_writer(io::stderr))
            .init();
    } else {
        subscriber.init();
    }

    Ok(())
}

/// A cloneable handle to the shared rolling log file, used as the tracing
/// [`MakeWriter`].
#[derive(Clone)]
struct RollingWriter {
    inner: Arc<Mutex<RollingFile>>,
}

impl RollingWriter {
    fn new(path: PathBuf, max_bytes: u64, max_rotated: usize) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let written = file.metadata()?.len();
        Ok(Self {
            inner: Arc::new(Mutex::new(RollingFile {
                path,
                file,
                max_bytes,
                max_rotated,
                written,
            })),
        })
    }
}

/// The writer handed to tracing for one event: a locked [`RollingFile`].
struct RollingWriterGuard<'a>(MutexGuard<'a, RollingFile>);

impl Write for RollingWriterGuard<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl<'a> MakeWriter<'a> for RollingWriter {
    type Writer = RollingWriterGuard<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        RollingWriterGuard(
            self.inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }
}

/// An append-only log file that renames itself out of the way when it grows
/// past `max_bytes`.
struct RollingFile {
    path: PathBuf,
    file: File,
    max_bytes: u64,
    max_rotated: usize,
    written: u64,
}

impl RollingFile {
    fn rotate(&mut self) -> io::Result<()> {
        if self.max_rotated == 0 {
            self.file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&self.path)?;
            self.written = 0;
            return Ok(());
        }

        // Drop the oldest archive, shift the rest up, then archive the current
        // file as argus.log.1 and start a fresh argus.log.
        let oldest = rotated_path(&self.path, self.max_rotated);
        if oldest.exists() {
            fs::remove_file(&oldest)?;
        }
        for index in (1..self.max_rotated).rev() {
            let from = rotated_path(&self.path, index);
            if from.exists() {
                fs::rename(&from, rotated_path(&self.path, index + 1))?;
            }
        }
        fs::rename(&self.path, rotated_path(&self.path, 1))?;

        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.written = 0;
        Ok(())
    }
}

impl Write for RollingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written + buf.len() as u64 > self.max_bytes {
            self.rotate()?;
        }
        let written = self.file.write(buf)?;
        self.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// `argus.log` with an index appended: `argus.log.1`, `argus.log.2`, …
fn rotated_path(path: &Path, index: usize) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{index}"));
    PathBuf::from(name)
}
