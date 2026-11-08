//! Bounded, asynchronous application diagnostics. Never log file bodies or credentials here.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use tracing::{Level, Metadata};
use tracing_subscriber::{
    EnvFilter,
    filter::{LevelFilter, filter_fn},
    fmt::MakeWriter,
    layer::SubscriberExt,
};

const QUEUE_CAPACITY: usize = 512;
const MAX_EVENT_BYTES: usize = 64 * 1024;
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
const LOG_FILE_COUNT: usize = 4;
const FLUSH_INTERVAL: Duration = Duration::from_millis(200);
const TRUNCATION_MARKER: &[u8] = b" ... [log event truncated]\n";
const DEFAULT_FILTER: &str =
    "info,gitviews=debug,gitviews_app=debug,gitviews_service=debug,gitviews_git=debug";

/// Keep this guard alive until GPUI exits so queued diagnostics are flushed and joined.
#[must_use]
pub struct DiagnosticsGuard {
    writer: Option<AsyncWriter>,
    worker: Option<JoinHandle<()>>,
    log_path: Option<PathBuf>,
}

impl DiagnosticsGuard {
    pub fn log_path(&self) -> Option<&Path> {
        self.log_path.as_deref()
    }
}

impl Drop for DiagnosticsGuard {
    fn drop(&mut self) {
        if let Some(writer) = self.writer.take() {
            // Blocking is restricted to shutdown; normal event writes only use try_send.
            let _ = writer.state.sender.send(Message::Shutdown);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Install the process subscriber, with stderr as a non-fatal fallback.
pub fn init(level: gitviews_service::LogLevel) -> DiagnosticsGuard {
    let directory = default_log_directory();
    let requested_path = directory.join("gitviews.log");
    let (output, log_path) =
        match RotatingFile::open(requested_path.clone(), MAX_LOG_BYTES, LOG_FILE_COUNT) {
            Ok(file) => (Output::File(file), Some(requested_path)),
            Err(error) => {
                report_failure("cannot open diagnostic log", &error);
                (Output::Stderr, None)
            }
        };
    let mut guard = match start_worker(output, log_path) {
        Ok(guard) => guard,
        Err(error) => {
            report_failure("cannot start log writer", &error);
            DiagnosticsGuard {
                writer: None,
                worker: None,
                log_path: None,
            }
        }
    };
    let make_writer = EventWriterFactory(guard.writer.clone());
    let configured = configured_filter(level);
    let (filter, invalid_filter) =
        build_filter_with_default(std::env::var("RUST_LOG").ok().as_deref(), &configured);
    let subscriber = tracing_subscriber::registry()
        .with(filter)
        .with(filter_fn(allow_metadata))
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_target(true)
                .with_thread_ids(true)
                .with_thread_names(true)
                .with_writer(make_writer),
        );
    if tracing::subscriber::set_global_default(subscriber).is_err() {
        // A host may already own the process subscriber; do not replace its panic hook.
        guard.log_path = None;
        return guard;
    }
    install_panic_hook();
    if invalid_filter {
        tracing::warn!(target: "gitviews::diagnostics", "Invalid RUST_LOG; using default diagnostic filter");
    }
    tracing::info!(
        target: "gitviews::diagnostics",
        log_path = ?guard.log_path(),
        queue_capacity = QUEUE_CAPACITY,
        max_log_bytes = MAX_LOG_BYTES,
        file_count = LOG_FILE_COUNT,
        "Diagnostics initialized"
    );
    guard
}

fn default_log_directory() -> PathBuf {
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/Application Support"));
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")));
    log_directory(
        std::env::var_os("GITVIEWS_LOG_DIR").map(PathBuf::from),
        base,
        std::env::temp_dir(),
    )
}

fn log_directory(
    override_path: Option<PathBuf>,
    base: Option<PathBuf>,
    fallback: PathBuf,
) -> PathBuf {
    override_path
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| base.unwrap_or(fallback).join("GitViews").join("logs"))
}

#[cfg(test)]
fn build_filter(directives: Option<&str>) -> (EnvFilter, bool) {
    build_filter_with_default(directives, DEFAULT_FILTER)
}

fn configured_filter(level: gitviews_service::LogLevel) -> String {
    use gitviews_service::LogLevel;
    match level {
        LogLevel::Debug => DEFAULT_FILTER.to_owned(),
        LogLevel::Trace => format!(
            "info,gitviews={0},gitviews_app={0},gitviews_service={0},gitviews_git={0}",
            level.as_str()
        ),
        _ => level.as_str().to_owned(),
    }
}

fn build_filter_with_default(directives: Option<&str>, fallback: &str) -> (EnvFilter, bool) {
    let directives = directives
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(fallback);
    let builder = EnvFilter::builder()
        .with_regex(false)
        .with_default_directive(LevelFilter::INFO.into());
    match builder.parse(directives) {
        Ok(filter) => (filter, false),
        Err(_) => (EnvFilter::new(fallback), true),
    }
}

fn allow_metadata(metadata: &Metadata<'_>) -> bool {
    allow_level(metadata.target(), metadata.level())
}

fn allow_level(target: &str, level: &Level) -> bool {
    *level <= Level::INFO
        || target == "gitviews"
        || target.starts_with("gitviews::")
        || target.starts_with("gitviews_")
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |information| {
        // Panic payloads can contain source text or authentication data. Only record location.
        if let Some(location) = information.location() {
            tracing::error!(
                target: "gitviews::diagnostics",
                file = location.file(),
                line = location.line(),
                column = location.column(),
                "Application panicked; default panic hook follows"
            );
        } else {
            tracing::error!(target: "gitviews::diagnostics", "Application panicked");
        }
        previous(information);
    }));
}

enum Message {
    Event(Vec<u8>),
    Shutdown,
}

struct QueueState {
    sender: SyncSender<Message>,
    dropped: AtomicU64,
}

#[derive(Clone)]
struct AsyncWriter {
    state: Arc<QueueState>,
}

#[derive(Clone)]
struct EventWriterFactory(Option<AsyncWriter>);

impl<'a> MakeWriter<'a> for EventWriterFactory {
    type Writer = EventWriter;

    fn make_writer(&'a self) -> Self::Writer {
        EventWriter {
            writer: self.0.clone(),
            bytes: Vec::with_capacity(256),
            truncated: false,
        }
    }
}

struct EventWriter {
    writer: Option<AsyncWriter>,
    bytes: Vec<u8>,
    truncated: bool,
}

impl Write for EventWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let available = MAX_EVENT_BYTES
            .saturating_sub(TRUNCATION_MARKER.len())
            .saturating_sub(self.bytes.len());
        let count = available.min(bytes.len());
        self.bytes.extend_from_slice(&bytes[..count]);
        self.truncated |= count != bytes.len();
        // Tell the formatter the full input was consumed; truncation is intentional.
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for EventWriter {
    fn drop(&mut self) {
        if self.truncated {
            // A byte cap can cut through a Chinese path or another multibyte field.
            if let Err(error) = std::str::from_utf8(&self.bytes) {
                self.bytes.truncate(error.valid_up_to());
            }
            self.bytes.extend_from_slice(TRUNCATION_MARKER);
        }
        if self.bytes.is_empty() {
            return;
        }
        if let Some(writer) = &self.writer {
            let event = Message::Event(std::mem::take(&mut self.bytes));
            match writer.state.sender.try_send(event) {
                Ok(()) => {}
                Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                    writer.state.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        } else {
            let _ = io::stderr().lock().write_all(&self.bytes);
        }
    }
}

fn start_worker(output: Output, log_path: Option<PathBuf>) -> io::Result<DiagnosticsGuard> {
    let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
    let state = Arc::new(QueueState {
        sender,
        dropped: AtomicU64::new(0),
    });
    let worker_state = Arc::clone(&state);
    let worker = thread::Builder::new()
        .name("gitviews-log-io".into())
        .spawn(move || run_worker(output, receiver, &worker_state))?;
    Ok(DiagnosticsGuard {
        writer: Some(AsyncWriter { state }),
        worker: Some(worker),
        log_path,
    })
}

fn run_worker(mut output: Output, receiver: Receiver<Message>, state: &QueueState) {
    let mut last_flush = Instant::now();
    loop {
        match receiver.recv_timeout(FLUSH_INTERVAL) {
            Ok(Message::Event(bytes)) => output.write(&bytes),
            Ok(Message::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        if last_flush.elapsed() >= FLUSH_INTERVAL {
            report_dropped(&mut output, state);
            output.flush();
            last_flush = Instant::now();
        }
    }
    report_dropped(&mut output, state);
    output.flush();
}

fn report_dropped(output: &mut Output, state: &QueueState) {
    let dropped = state.dropped.swap(0, Ordering::Relaxed);
    if dropped == 0 {
        return;
    }
    let utc = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    output.write(
        format!(
            "unix_utc={}.{:03} WARN gitviews::diagnostics thread=gitviews-log-io dropped_log_events={dropped} reason=queue_full_or_closed\n",
            utc.as_secs(),
            utc.subsec_millis()
        )
        .as_bytes(),
    );
}

enum Output {
    File(RotatingFile),
    Stderr,
}

impl Output {
    fn write(&mut self, bytes: &[u8]) {
        let result = match self {
            Self::File(file) => file.write_all(bytes),
            Self::Stderr => io::stderr().lock().write_all(bytes),
        };
        if let Err(error) = result
            && matches!(self, Self::File(_))
        {
            report_failure("diagnostic log write failed", &error);
            *self = Self::Stderr;
            let _ = io::stderr().lock().write_all(bytes);
        }
    }

    fn flush(&mut self) {
        let result = match self {
            Self::File(file) => file.flush(),
            Self::Stderr => io::stderr().lock().flush(),
        };
        if let Err(error) = result
            && matches!(self, Self::File(_))
        {
            report_failure("diagnostic log flush failed", &error);
            *self = Self::Stderr;
        }
    }
}

struct RotatingFile {
    path: PathBuf,
    writer: Option<BufWriter<File>>,
    bytes: u64,
    max_bytes: u64,
    file_count: usize,
}

impl RotatingFile {
    fn open(path: PathBuf, max_bytes: u64, file_count: usize) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let bytes = file.metadata()?.len();
        let mut result = Self {
            path,
            writer: Some(BufWriter::new(file)),
            bytes,
            max_bytes: max_bytes.max(1),
            file_count: file_count.max(1),
        };
        if bytes >= result.max_bytes {
            result.rotate()?;
        }
        Ok(result)
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.bytes > 0 && self.bytes.saturating_add(bytes.len() as u64) > self.max_bytes {
            self.rotate()?;
        }
        self.writer
            .as_mut()
            .ok_or_else(|| io::Error::other("log file is closed"))?
            .write_all(bytes)?;
        self.bytes = self.bytes.saturating_add(bytes.len() as u64);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(writer) = &mut self.writer {
            writer.flush()?;
        }
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.flush()?;
        self.writer.take(); // Close the Windows file handle before renaming.
        for index in (1..self.file_count).rev() {
            let destination = self.archive_path(index);
            remove_if_exists(&destination)?;
            let source = if index == 1 {
                self.path.clone()
            } else {
                self.archive_path(index - 1)
            };
            match fs::rename(&source, &destination) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&self.path)?;
        self.writer = Some(BufWriter::new(file));
        self.bytes = 0;
        Ok(())
    }

    fn archive_path(&self, index: usize) -> PathBuf {
        self.path.with_file_name(format!("gitviews.{index}.log"))
    }
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn report_failure(message: &str, error: &io::Error) {
    // Unlike eprintln!, a closed stderr must not cause logging to panic.
    let _ = writeln!(
        io::stderr().lock(),
        "GitViews: {message} ({error}); using stderr."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    static DIRECTORY_ID: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let directory = std::env::temp_dir().join(format!(
                "gitviews-diagnostics-{}-{}",
                std::process::id(),
                DIRECTORY_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&directory).expect("create temporary log directory");
            Self(directory)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn log_location_honors_override_and_platform_base() {
        assert_eq!(
            log_directory(
                Some("custom".into()),
                Some("settings".into()),
                "temp".into()
            ),
            PathBuf::from("custom")
        );
        assert_eq!(
            log_directory(None, Some("settings".into()), "temp".into()),
            PathBuf::from("settings").join("GitViews/logs")
        );
        assert_eq!(
            log_directory(Some(PathBuf::new()), None, "temp".into()),
            PathBuf::from("temp").join("GitViews/logs")
        );
    }

    #[test]
    fn debug_targets_are_restricted_to_gitviews() {
        for target in [
            "gitviews",
            "gitviews::timing",
            "gitviews_app::ui",
            "gitviews_service",
        ] {
            assert!(allow_level(target, &Level::DEBUG));
        }
        for target in ["gpui", "hyper", "gitviewsevil"] {
            assert!(!allow_level(target, &Level::DEBUG));
            assert!(!allow_level(target, &Level::TRACE));
            assert!(allow_level(target, &Level::INFO));
            assert!(allow_level(target, &Level::ERROR));
        }
    }

    #[test]
    fn invalid_filter_uses_default_without_panicking() {
        let (filter, invalid) = build_filter(Some("[invalid"));
        assert!(invalid);
        assert_eq!(filter.to_string(), build_filter(None).0.to_string());
        assert!(!build_filter(Some("gitviews_app=debug,warn")).1);
        assert!(!build_filter(None).1);
    }

    #[test]
    fn configured_log_level_and_environment_override_filter_real_events() {
        use gitviews_service::LogLevel;
        for (level, environment, expected) in [
            (LogLevel::Off, None, 0),
            (LogLevel::Warn, None, 2),
            (LogLevel::Debug, None, 4),
            (LogLevel::Trace, None, 5),
            (LogLevel::Debug, Some("error"), 1),
            (LogLevel::Warn, Some("[invalid"), 2),
        ] {
            let directory = TestDirectory::new();
            let path = directory.0.join("gitviews.log");
            let file = RotatingFile::open(path.clone(), MAX_LOG_BYTES, LOG_FILE_COUNT).unwrap();
            let guard = start_worker(Output::File(file), Some(path.clone())).unwrap();
            let (filter, _) = build_filter_with_default(environment, &configured_filter(level));
            let subscriber = tracing_subscriber::registry()
                .with(filter)
                .with(filter_fn(allow_metadata))
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(false)
                        .with_writer(EventWriterFactory(guard.writer.clone())),
                );
            tracing::subscriber::with_default(subscriber, || {
                tracing::error!(target: "gitviews::settings", "configured-level-event");
                tracing::warn!(target: "gitviews::settings", "configured-level-event");
                tracing::info!(target: "gitviews::settings", "configured-level-event");
                tracing::debug!(target: "gitviews::settings", "configured-level-event");
                tracing::trace!(target: "gitviews::settings", "configured-level-event");
                tracing::debug!(target: "gpui", "third-party-debug");
            });
            drop(guard);
            let contents = fs::read_to_string(path).unwrap();
            assert_eq!(
                contents.matches("configured-level-event").count(),
                expected,
                "{level:?} / {environment:?}"
            );
            assert!(!contents.contains("third-party-debug"));
        }
    }

    #[test]
    fn full_queue_drops_events_without_blocking() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let state = Arc::new(QueueState {
            sender,
            dropped: AtomicU64::new(0),
        });
        let factory = EventWriterFactory(Some(AsyncWriter {
            state: state.clone(),
        }));
        for value in [b"first\n", b"later\n"] {
            let mut writer = factory.make_writer();
            writer.write_all(value).expect("format event");
        }
        assert_eq!(state.dropped.load(Ordering::Relaxed), 1);
        let Message::Event(bytes) = receiver.try_recv().expect("first queued event") else {
            panic!("expected log event");
        };
        assert_eq!(bytes, b"first\n");
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn oversized_event_has_bounded_storage_and_truncation_marker() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let factory = EventWriterFactory(Some(AsyncWriter {
            state: Arc::new(QueueState {
                sender,
                dropped: AtomicU64::new(0),
            }),
        }));
        let mut writer = factory.make_writer();
        writer
            .write_all(&vec![b'x'; MAX_EVENT_BYTES * 3])
            .expect("format long event");
        drop(writer);
        let Message::Event(bytes) = receiver.try_recv().expect("queued event") else {
            panic!("expected log event");
        };
        assert_eq!(bytes.len(), MAX_EVENT_BYTES);
        assert!(bytes.ends_with(TRUNCATION_MARKER));
    }

    #[test]
    fn truncated_event_keeps_chinese_text_valid_utf8() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let factory = EventWriterFactory(Some(AsyncWriter {
            state: Arc::new(QueueState {
                sender,
                dropped: AtomicU64::new(0),
            }),
        }));
        let mut writer = factory.make_writer();
        writer
            .write_all("中文路径".repeat(MAX_EVENT_BYTES / 3).as_bytes())
            .expect("format Chinese event");
        drop(writer);
        let Message::Event(bytes) = receiver.try_recv().expect("queued event") else {
            panic!("expected log event");
        };
        assert!(bytes.len() <= MAX_EVENT_BYTES);
        assert!(std::str::from_utf8(&bytes).is_ok());
        assert!(bytes.ends_with(TRUNCATION_MARKER));
    }

    #[test]
    fn rotation_retains_current_and_three_archives() {
        let directory = TestDirectory::new();
        let path = directory.0.join("gitviews.log");
        let mut file = RotatingFile::open(path.clone(), 8, 4).expect("open log");
        for value in [b"0000\n", b"1111\n", b"2222\n", b"3333\n", b"4444\n"] {
            file.write_all(value).expect("write log event");
        }
        file.flush().expect("flush log");
        assert_eq!(fs::read(&path).expect("current log"), b"4444\n");
        assert_eq!(
            fs::read(file.archive_path(1)).expect("archive 1"),
            b"3333\n"
        );
        assert_eq!(
            fs::read(file.archive_path(2)).expect("archive 2"),
            b"2222\n"
        );
        assert_eq!(
            fs::read(file.archive_path(3)).expect("archive 3"),
            b"1111\n"
        );
        assert!(!file.archive_path(4).exists());
    }

    #[test]
    fn startup_rotates_a_full_existing_log() {
        let directory = TestDirectory::new();
        let path = directory.0.join("gitviews.log");
        fs::write(&path, b"existing").expect("seed existing log");
        let mut file = RotatingFile::open(path.clone(), 8, 4).expect("open existing log");
        file.write_all(b"new\n").expect("write new log");
        file.flush().expect("flush new log");
        assert_eq!(fs::read(path).expect("current log"), b"new\n");
        assert_eq!(
            fs::read(file.archive_path(1)).expect("archive"),
            b"existing"
        );
    }

    #[test]
    fn guard_flushes_events_and_drop_count_before_joining() {
        let directory = TestDirectory::new();
        let path = directory.0.join("gitviews.log");
        let file = RotatingFile::open(path.clone(), 1024, 4).expect("open worker log");
        let guard = start_worker(Output::File(file), Some(path.clone())).expect("start worker");
        let factory = EventWriterFactory(guard.writer.clone());
        let mut writer = factory.make_writer();
        writer.write_all(b"completed\n").expect("format event");
        drop(writer);
        guard
            .writer
            .as_ref()
            .expect("worker writer")
            .state
            .dropped
            .store(7, Ordering::Relaxed);
        drop(guard);
        let contents = fs::read_to_string(path).expect("read flushed worker log");
        assert!(contents.starts_with("completed\n"));
        assert!(contents.contains("dropped_log_events=7"));
    }

    #[test]
    fn default_subscriber_writes_diagnostic_stage_fields_and_flushes_on_shutdown() {
        let directory = TestDirectory::new();
        let path = directory.0.join("gitviews.log");
        let file = RotatingFile::open(path.clone(), MAX_LOG_BYTES, LOG_FILE_COUNT)
            .expect("open subscriber log");
        let guard = start_worker(Output::File(file), Some(path.clone())).expect("start log worker");
        let (filter, invalid) = build_filter(None);
        assert!(!invalid);
        let subscriber = tracing_subscriber::registry()
            .with(filter)
            .with(filter_fn(allow_metadata))
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_target(true)
                    .with_thread_ids(true)
                    .with_thread_names(true)
                    .with_writer(EventWriterFactory(guard.writer.clone())),
            );
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                target: "gitviews::tasks",
                "background",
                operation = "submodule.load",
                request_id = 43_u64,
                queue_ms = 2_u64,
            );
            let _entered = span.enter();
            tracing::debug!(
                target: "gitviews::submodules",
                stage = "snapshot",
                elapsed_ms = 17_u64,
                "diagnostic stage completed"
            );
            tracing::debug!(target: "gpui", "third party debug must stay hidden");
        });
        // No sleep or explicit flush: guard shutdown must drain the real formatter event.
        drop(guard);
        let contents = fs::read_to_string(path).expect("read flushed subscriber log");
        assert!(contents.contains("DEBUG"));
        assert!(contents.contains("gitviews::submodules"));
        assert!(contents.contains("background"));
        assert!(contents.contains("submodule.load"));
        assert!(contents.contains("request_id=43"));
        assert!(contents.contains("queue_ms=2"));
        assert!(contents.contains("snapshot"));
        assert!(contents.contains("elapsed_ms=17"));
        assert!(contents.contains("ThreadId("));
        let timestamp = contents.split_whitespace().next().expect("UTC timestamp");
        assert!(timestamp.contains('T') && timestamp.ends_with('Z'));
        assert!(!contents.contains('\u{1b}'));
        assert!(!contents.contains("third party debug"));
    }
}
