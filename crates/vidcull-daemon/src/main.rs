#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tracing_subscriber::prelude::*;
use vidcull_core::Result;
use vidcull_daemon::redact::redact_fs_path;
use vidcull_daemon::{
    Daemon, DaemonConfig, DaemonRequestHandler, FileWatcher, IndexingHandler, LogBuffer,
    ScanExecutor, ShutdownToken, TaskHandler, WatchConfig, activity, enqueue_partial_backfill,
    install_signal_handlers, priority, unix_now, worker_health::WorkerHealth,
};
use vidcull_db::repo::Task;
use vidcull_ipc::{BindOutcome, EXIT_ALREADY_RUNNING, EXIT_LISTENER_FATAL, IpcServer};
use vidcull_parser::fallback::FfmpegBinaries;

struct LoggingHandler;

impl TaskHandler for LoggingHandler {
    fn handle(&mut self, task: &Task) -> Result<()> {
        tracing::info!(
            id = task.id,
            kind = %task.kind,
            attempts = task.attempts,
            "processing task",
        );
        Ok(())
    }
}

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> Result<()> {
    suppress_os_error_dialogs();

    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--export-diagnostics") {
        let dest = args
            .get(pos + 1)
            .cloned()
            .unwrap_or_else(|| "vidcull-diagnostics".to_owned());
        let logs_dir = vidcull_daemon::settings::data_dir().join("logs");
        match vidcull_daemon::diagnostics::collect_diagnostic_bundle(
            &logs_dir,
            std::path::Path::new(&dest),
        ) {
            Ok(files) => {
                println!("exported {} log file(s) to {dest}", files.len());
                return Ok(());
            }
            Err(err) => {
                eprintln!("diagnostics export failed: {err}");
                std::process::exit(1);
            }
        }
    }

    if args.iter().any(|a| a == "--build-stamp") {
        println!("{}", vidcull_daemon::build_stamp());
        return Ok(());
    }

    let log_buffer = LogBuffer::default();
    let worker_health = WorkerHealth::new();

    let log_dir = vidcull_daemon::settings::data_dir().join("logs");
    let (_file_guard, file_writer) = make_file_appender(&log_dir);

    init_subscriber(&log_buffer, file_writer);

    install_panic_hook(worker_health.clone());

    let db_path = std::env::var_os("VIDCULL_DB").map_or_else(
        || vidcull_daemon::settings::data_dir().join("vidcull.db"),
        PathBuf::from,
    );
    if let Some(parent) = db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut config = DaemonConfig::default();

    let ffmpeg = resolve_ffmpeg();
    match &ffmpeg {
        Ok(bins) => tracing::info!(
            ffmpeg = %redact_fs_path(bins.ffmpeg()),
            ffprobe = %redact_fs_path(bins.ffprobe()),
            "ffmpeg binaries resolved; indexing enabled",
        ),
        Err(err) => tracing::warn!(
            error = %err,
            "ffmpeg not found; indexing disabled (lifecycle-only). Set VIDCULL_FFMPEG_DIR or install on PATH",
        ),
    }

    let mut db = vidcull_db::open_file(&db_path)?;

    match vidcull_daemon::bridge::reconcile_pending_deletes(&mut db, unix_now()) {
        Ok(0) => {}
        Ok(n) => tracing::info!(finalized = n, "reconciled interrupted delete batches"),
        Err(err) => {
            tracing::warn!(error = %err, "pending-delete reconciliation failed; continuing");
        }
    }

    match vidcull_daemon::backup::snapshot_into(
        &db,
        &vidcull_daemon::backup::default_backup_dir(),
        unix_now(),
    ) {
        Ok(path) => tracing::info!(path = %redact_fs_path(&path), "index snapshot written"),
        Err(err) => tracing::warn!(error = %err, "index snapshot failed; continuing without it"),
    }

    let shutdown = ShutdownToken::new();
    install_signal_handlers(shutdown.clone());

    let mut settings = vidcull_daemon::settings::load(&db);
    if vidcull_daemon::settings::ensure_system_excludes(&mut settings.exclude_rules) {
        let _ = vidcull_daemon::settings::save(&db, &settings);
    }
    if vidcull_daemon::settings::migrate_partial_clips_default_on(&db, &mut settings) {
        tracing::info!("partial-clip detection enabled by default");
    }
    match vidcull_daemon::migrate_native_swap::migrate_partial_native_swap(
        &mut db,
        &config.kind,
        unix_now(),
    ) {
        Ok(outcome) if !outcome.already_migrated => tracing::info!(
            reenqueued = outcome.reenqueued,
            cleaned = outcome.cleaned,
            "native-swap partial migration",
        ),
        Ok(_) => {}
        Err(err) => tracing::warn!(
            error = %err,
            "native-swap partial migration failed; will retry next boot",
        ),
    }
    if std::env::var("VIDCULL_MAX_PERF")
        .map(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "yes" | "on"))
        .unwrap_or(false)
    {
        settings.cpu_throttle = vidcull_ipc::CpuThrottle::Full;
        tracing::info!("max-performance forced (VIDCULL_MAX_PERF)");
    }
    let throttle_control = Arc::new(vidcull_daemon::ThrottleControl::default());
    throttle_control.set_level(settings.cpu_throttle);
    throttle_control.set_idle_workers(vidcull_daemon::settings::clamp_idle_workers(
        settings.idle_worker_count,
    ));
    throttle_control.set_io_budget_cap(vidcull_daemon::storage::detect_io_budget_cap(
        &settings.scan_folders,
    ));
    throttle_control.set_indexing_enabled(settings.indexing_enabled);
    config.throttle_control = Arc::clone(&throttle_control);

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        build_stamp = vidcull_daemon::build_stamp(),
        protocol_version = vidcull_ipc::protocol::PROTOCOL_VERSION,
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        data_dir = %redact_fs_path(vidcull_daemon::settings::data_dir()),
        cpu_cores = settings.cpu_cores,
        cpu_throttle = ?settings.cpu_throttle,
        ffmpeg_available = ffmpeg.is_ok(),
        "daemon startup environment",
    );

    if !throttle_control.is_max_performance() {
        if let Err(err) = priority::lower_process_priority() {
            tracing::warn!(error = %err, "could not lower process priority; running at normal priority");
        }
    }

    let thumbnail_ffmpeg = ffmpeg.as_ref().ok().cloned();
    let thumbnails = Arc::new(vidcull_daemon::ThumbnailProvider::new(
        vidcull_daemon::thumbnails::cache_dir(),
        thumbnail_ffmpeg,
    ));
    let scan_executor = ScanExecutor::spawn(db_path.clone(), config.kind.clone(), shutdown.clone());
    let ipc_outcome = spawn_ipc_server(
        &db_path,
        &config,
        shutdown.clone(),
        log_buffer.clone(),
        Arc::clone(&thumbnails),
        Arc::clone(&throttle_control),
        worker_health.clone(),
        scan_executor.clone(),
    );
    let ipc_handle = match ipc_outcome {
        IpcSpawnOutcome::AlreadyRunning => {
            std::process::exit(EXIT_ALREADY_RUNNING);
        }
        IpcSpawnOutcome::Running(handle) => Some(handle),
        IpcSpawnOutcome::BindFailed => None,
    };
    let ipc_monitor = ipc_handle.map(|handle| {
        spawn_worker_monitor(
            "IPC server",
            handle,
            worker_health.clone(),
            shutdown.clone(),
            true,
            |()| tracing::info!("IPC server stopped"),
        )
    });

    let watch_roots = merge_scan_roots(
        std::env::var_os("VIDCULL_WATCH"),
        settings.scan_folders.clone(),
    );
    let watcher_handle = spawn_watcher(
        &db_path,
        &watch_roots,
        &config.kind,
        &settings.exclude_rules,
        shutdown.clone(),
        Arc::clone(&throttle_control),
    );
    let watcher_monitor = watcher_handle.map(|handle| {
        spawn_worker_monitor(
            "file watcher",
            handle,
            worker_health.clone(),
            shutdown.clone(),
            false,
            |stats: vidcull_daemon::WatchStats| {
                tracing::info!(
                    events = stats.events_seen,
                    enqueued = stats.changes_enqueued,
                    "file watcher stopped",
                );
            },
        )
    });

    let gate_observer = vidcull_daemon::DecodeGateObserver::default();
    if std::env::var_os("VIDCULL_RESOURCE_LOG").is_some() {
        let resource_shutdown = shutdown.clone();
        let gate_observer = gate_observer.clone();
        std::thread::spawn(move || {
            let collector = vidcull_daemon::metrics::MetricsCollector::new();
            loop {
                resource_shutdown.wait_timeout(std::time::Duration::from_secs(2));
                if resource_shutdown.is_triggered() {
                    break;
                }
                let m = collector.sample();
                let g = gate_observer.snapshot().unwrap_or_default();
                tracing::info!(
                    stage = "resource",
                    rss_bytes = m.rss_bytes,
                    cpu_permille = m.cpu_permille,
                    decode_conc_in_use = g.decode_conc_in_use,
                    decode_conc_cap = g.decode_conc_cap,
                    decode_conc_waiters = g.decode_conc_waiters,
                    base_gate_in_use = g.base_gate_in_use,
                    base_gate_cap = g.base_gate_cap,
                    partial_gate_in_use = g.partial_gate_in_use,
                    partial_gate_cap = g.partial_gate_cap,
                    seq_read_in_use = g.seq_read_in_use,
                    seq_read_cap = g.seq_read_cap,
                    active_decode_workers = g.active_decode_workers,
                    "resource sample",
                );
            }
        });
    }

    scan_executor.submit(watch_roots.clone(), settings.exclude_rules.clone());

    tracing::info!(db = %redact_fs_path(&db_path), kind = %config.kind, "vidcull-daemon started");
    let task_kind = config.kind.clone();
    let daemon = Daemon::new(config);
    let stats = match ffmpeg {
        Ok(bins) => {
            let handler_db = vidcull_db::open_file(&db_path)?;
            let partial_clips = settings.partial_clips_enabled
                || std::env::var("VIDCULL_PARTIAL_CLIPS")
                    .map(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "yes" | "on"))
                    .unwrap_or(false);
            throttle_control.set_partial_clips(partial_clips);
            if partial_clips {
                tracing::info!(
                    persisted = settings.partial_clips_enabled,
                    "partial-clip detection enabled",
                );
                run_partial_backfill(&db_path, &task_kind);
            }
            let handler = IndexingHandler::new(handler_db, bins, unix_now)
                .with_task_kind(task_kind)
                .with_partial_clips(partial_clips)
                .with_thumbnails(Arc::clone(&thumbnails));
            handler.observe_gates(&gate_observer);
            daemon
                .run_async_throttled(db, handler, shutdown, unix_now, activity::current)
                .await?
        }
        Err(_) => {
            daemon
                .run_async_throttled(db, LoggingHandler, shutdown, unix_now, activity::current)
                .await?
        }
    };
    tracing::info!(
        recovered = stats.recovered,
        processed = stats.processed,
        failed = stats.failed,
        "vidcull-daemon stopped",
    );

    if let Some(handle) = ipc_monitor {
        let _ = handle.await;
    }
    if let Some(handle) = watcher_monitor {
        let _ = handle.await;
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum MonitorAction {
    TriggerShutdown,
    FatalExit(i32),
}

fn decide_monitor_action(died: bool, fatal_exit: bool, fatal_code: i32) -> Option<MonitorAction> {
    if !died {
        return None;
    }
    Some(if fatal_exit {
        MonitorAction::FatalExit(fatal_code)
    } else {
        MonitorAction::TriggerShutdown
    })
}

fn spawn_worker_monitor<T: Send + 'static>(
    name: &'static str,
    handle: tokio::task::JoinHandle<Result<T>>,
    health: WorkerHealth,
    shutdown: ShutdownToken,
    fatal_exit: bool,
    on_ok: impl FnOnce(T) + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let action = match handle.await {
            Ok(Ok(value)) => {
                on_ok(value);
                let died = !shutdown.is_triggered();
                if died {
                    tracing::error!(
                        worker = name,
                        "worker stopped before shutdown was requested"
                    );
                }
                decide_monitor_action(died, fatal_exit, EXIT_LISTENER_FATAL)
            }
            Ok(Err(err)) => {
                tracing::error!(worker = name, error = %err, "worker stopped with an error");
                decide_monitor_action(true, fatal_exit, EXIT_LISTENER_FATAL)
            }
            Err(err) => {
                tracing::error!(worker = name, error = %err, "worker task panicked");
                decide_monitor_action(true, fatal_exit, EXIT_LISTENER_FATAL)
            }
        };
        if let Some(action) = action {
            health.record_death();
            match action {
                MonitorAction::TriggerShutdown => shutdown.trigger(),
                MonitorAction::FatalExit(code) => {
                    tracing::error!(
                        worker = name,
                        exit_code = code,
                        "worker death is fatal; exiting immediately \
                         (no graceful drain — split-brain guard)"
                    );
                    std::process::exit(code);
                }
            }
        }
    })
}

fn local_time_ms() -> tracing_subscriber::fmt::time::ChronoLocal {
    tracing_subscriber::fmt::time::ChronoLocal::new("%Y-%m-%d %H:%M:%S%.3f".to_owned())
}

fn init_subscriber(
    log_buffer: &LogBuffer,
    file_writer: Option<tracing_appender::non_blocking::NonBlocking>,
) {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let (filter_layer, reload_handle) = tracing_subscriber::reload::Layer::new(filter);
    vidcull_daemon::logctl::install_reload_handle(reload_handle);
    let base = tracing_subscriber::registry()
        .with(filter_layer)
        .with(fmt::layer().with_target(true).with_timer(local_time_ms()))
        .with(log_buffer.layer());

    if let Some(writer) = file_writer {
        let file_layer = fmt::layer()
            .with_target(true)
            .with_ansi(false)
            .with_timer(local_time_ms())
            .with_writer(writer);
        let _ = base.with(file_layer).try_init();
    } else {
        let _ = base.try_init();
    }
}

fn make_file_appender(
    log_dir: &std::path::Path,
) -> (
    Option<tracing_appender::non_blocking::WorkerGuard>,
    Option<tracing_appender::non_blocking::NonBlocking>,
) {
    use tracing_appender::rolling;

    if let Err(err) = std::fs::create_dir_all(log_dir) {
        eprintln!(
            "[vidcull-daemon] could not create log directory {}: {err}; file logging disabled",
            log_dir.display()
        );
        return (None, None);
    }

    let appender = match rolling::Builder::new()
        .rotation(rolling::Rotation::DAILY)
        .max_log_files(7)
        .filename_prefix("vidcull-daemon")
        .filename_suffix("log")
        .build(log_dir)
    {
        Ok(a) => a,
        Err(err) => {
            eprintln!(
                "[vidcull-daemon] could not create rolling appender at {}: {err}; file logging disabled",
                log_dir.display()
            );
            return (None, None);
        }
    };

    let (non_blocking, guard) = tracing_appender::non_blocking(appender);
    (Some(guard), Some(non_blocking))
}

fn install_panic_hook(health: WorkerHealth) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("<non-string panic payload>");

        let location = info.location().map_or_else(
            || "<unknown location>".to_owned(),
            |l| format!("{}:{}:{}", l.file(), l.line(), l.column()),
        );

        let backtrace = std::backtrace::Backtrace::capture();

        tracing::error!(
            panic.payload = payload,
            panic.location = %location,
            panic.backtrace = %backtrace,
            "thread panicked",
        );

        health.record_panic();

        previous(info);
    }));
}

#[cfg(windows)]
fn suppress_os_error_dialogs() {
    use windows_sys::Win32::System::Diagnostics::Debug::{
        SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SetErrorMode,
    };
    #[allow(unsafe_code)]
    unsafe {
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX);
    }
}

#[cfg(not(windows))]
fn suppress_os_error_dialogs() {}

fn resolve_ffmpeg() -> Result<FfmpegBinaries> {
    if std::env::var_os("VIDCULL_FFMPEG_DIR").is_some()
        || std::env::var_os("VIDCULL_FFMPEG").is_some()
    {
        return FfmpegBinaries::resolve();
    }
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf));
    if let Some(dir) = &exe_dir {
        for candidate in [dir.join("ffmpeg"), dir.clone()] {
            let bins = FfmpegBinaries::from_dir(&candidate);
            if bins.ffmpeg().is_file() && bins.ffprobe().is_file() {
                return Ok(bins);
            }
        }
    }
    if let Some(bins) = vendored_ffmpeg(exe_dir.as_deref()) {
        return Ok(bins);
    }
    FfmpegBinaries::resolve()
}

fn vendored_ffmpeg(exe_dir: Option<&std::path::Path>) -> Option<FfmpegBinaries> {
    let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let rel = std::path::Path::new("vendor")
        .join("ffmpeg")
        .join(&platform);

    let start_dir = exe_dir
        .map(std::path::Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok());

    if let Some(mut cursor) = start_dir {
        for _ in 0..12 {
            let bins = FfmpegBinaries::from_dir(&cursor.join(&rel));
            if bins.ffmpeg().is_file() && bins.ffprobe().is_file() {
                return Some(bins);
            }
            if let Some(parent) = cursor.parent().map(std::path::Path::to_path_buf) {
                cursor = parent;
            } else {
                break;
            }
        }
    }
    None
}

enum IpcSpawnOutcome {
    Running(tokio::task::JoinHandle<Result<()>>),
    AlreadyRunning,
    BindFailed,
}

#[allow(clippy::too_many_arguments)]
fn spawn_ipc_server(
    db_path: &std::path::Path,
    config: &DaemonConfig,
    shutdown: ShutdownToken,
    log_buffer: LogBuffer,
    thumbnails: Arc<vidcull_daemon::ThumbnailProvider>,
    throttle_control: Arc<vidcull_daemon::ThrottleControl>,
    worker_health: WorkerHealth,
    scan_executor: ScanExecutor,
) -> IpcSpawnOutcome {
    let endpoint = std::env::var("VIDCULL_IPC").unwrap_or_else(|_| vidcull_ipc::default_endpoint());
    let server = match IpcServer::try_bind(&endpoint) {
        BindOutcome::Bound(server) => server,
        BindOutcome::AlreadyRunning => {
            tracing::info!(
                endpoint = %endpoint,
                "another instance is already running; attach UI to it — exiting"
            );
            return IpcSpawnOutcome::AlreadyRunning;
        }
        BindOutcome::Failed(err) => {
            tracing::warn!(endpoint = %endpoint, error = %err, "could not bind IPC endpoint; UI cannot attach");
            return IpcSpawnOutcome::BindFailed;
        }
    };
    let handler_db = match vidcull_db::open_file(db_path) {
        Ok(db) => Arc::new(Mutex::new(db)),
        Err(err) => {
            tracing::warn!(error = %err, "could not open IPC handler database; UI cannot attach");
            return IpcSpawnOutcome::BindFailed;
        }
    };
    let handler = DaemonRequestHandler::new(
        handler_db,
        shutdown.clone(),
        log_buffer,
        config.kind.clone(),
        Arc::new(vidcull_daemon::OsFileRemover),
    )
    .with_thumbnails(thumbnails)
    .with_throttle_control(throttle_control)
    .with_scan_executor(scan_executor)
    .with_backup_dir(vidcull_daemon::backup::default_backup_dir())
    .with_worker_health(worker_health);
    let handler = Arc::new(handler);
    tracing::info!(endpoint = %endpoint, "IPC server listening");
    IpcSpawnOutcome::Running(tokio::spawn(async move {
        server
            .serve(handler, async move { shutdown.cancelled().await })
            .await
    }))
}

fn run_partial_backfill(db_path: &std::path::Path, kind: &str) {
    let result = vidcull_db::open_file(db_path)
        .and_then(|mut db| enqueue_partial_backfill(&mut db, kind, unix_now()));
    match result {
        Ok(count) => tracing::info!(count, "startup: backfilled partial-clip fingerprints"),
        Err(err) => tracing::warn!(error = %err, "startup partial-clip backfill failed"),
    }
}

fn merge_scan_roots(
    env_value: Option<std::ffi::OsString>,
    scan_folders: Vec<String>,
) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = env_value
        .map(|raw| std::env::split_paths(&raw).collect())
        .unwrap_or_default();
    for folder in scan_folders {
        let path = PathBuf::from(folder);
        if !path.as_os_str().is_empty() && !roots.contains(&path) {
            roots.push(path);
        }
    }
    roots
}

fn spawn_watcher(
    db_path: &std::path::Path,
    roots: &[PathBuf],
    kind: &str,
    exclude_rules: &[String],
    shutdown: ShutdownToken,
    throttle_control: Arc<vidcull_daemon::ThrottleControl>,
) -> Option<tokio::task::JoinHandle<Result<vidcull_daemon::WatchStats>>> {
    let config = WatchConfig {
        task_kind: kind.to_owned(),
        options: vidcull_scanner::ScanOptions::default().with_excludes(exclude_rules),
        throttle_control: Some(throttle_control),
        ..WatchConfig::default()
    };
    let mut watcher = match FileWatcher::new(config) {
        Ok(watcher) => watcher,
        Err(err) => {
            tracing::warn!(error = %err, "could not create file watcher; running without it");
            return None;
        }
    };
    for root in roots {
        if let Err(err) = watcher.watch(root) {
            tracing::warn!(root = %redact_fs_path(root), error = %err, "could not watch path; skipping it");
        } else {
            tracing::info!(root = %redact_fs_path(root), "watching for changes");
        }
    }

    let watch_db_path = db_path.to_owned();
    Some(tokio::task::spawn_blocking(move || {
        let mut db = vidcull_db::open_file(&watch_db_path)?;
        watcher.run(&mut db, &shutdown)
    }))
}

#[cfg(test)]
mod tests {
    use super::{
        EXIT_LISTENER_FATAL, MonitorAction, decide_monitor_action, install_panic_hook,
        merge_scan_roots, spawn_worker_monitor, vendored_ffmpeg,
    };
    use std::ffi::OsString;
    use std::path::PathBuf;
    use vidcull_daemon::worker_health::WorkerHealth;

    #[allow(clippy::type_complexity)]
    struct PanicHookGuard {
        saved: Option<
            Box<dyn for<'a, 'b> Fn(&'a std::panic::PanicHookInfo<'b>) + Sync + Send + 'static>,
        >,
    }

    impl PanicHookGuard {
        fn new() -> Self {
            Self {
                saved: Some(std::panic::take_hook()),
            }
        }
    }

    impl Drop for PanicHookGuard {
        fn drop(&mut self) {
            if let Some(hook) = self.saved.take() {
                std::panic::set_hook(hook);
            }
        }
    }

    #[test]
    fn panic_hook_wiring() {
        let health = WorkerHealth::new();
        {
            let _guard = PanicHookGuard::new();
            install_panic_hook(health.clone());
            let _ = std::thread::spawn(|| panic!("ac5.1 test panic")).join();
            assert_eq!(
                health.panic_count(),
                1,
                "panic hook must increment panic_count by 1"
            );
        }

        let health2 = WorkerHealth::new();
        let _ = std::thread::spawn(|| panic!("ac5.4 restoration test")).join();
        assert_eq!(
            health2.panic_count(),
            0,
            "after RAII restore, panic must not increment the recording health counter",
        );
    }

    #[tokio::test]
    async fn worker_monitor_triggers_shutdown_on_worker_error() {
        let health = WorkerHealth::new();
        let shutdown = vidcull_daemon::ShutdownToken::new();

        let handle: tokio::task::JoinHandle<vidcull_core::Result<()>> = tokio::spawn(async {
            Err(vidcull_core::Error::Io(std::io::Error::other(
                "fake worker error",
            )))
        });

        let monitor = spawn_worker_monitor(
            "test-worker",
            handle,
            health.clone(),
            shutdown.clone(),
            false,
            |()| {},
        );
        monitor.await.expect("monitor task must not panic");

        assert_eq!(
            health.dead_workers(),
            1,
            "monitor must record one worker death"
        );
        assert!(
            shutdown.is_triggered(),
            "monitor must trigger shutdown after worker error"
        );
    }

    #[test]
    fn ipc_listener_death_decides_fatal_exit() {
        assert_eq!(
            decide_monitor_action(true, true, EXIT_LISTENER_FATAL),
            Some(MonitorAction::FatalExit(EXIT_LISTENER_FATAL)),
            "a fatal_exit worker's death must decide FatalExit with the \
             configured exit code, never TriggerShutdown"
        );
    }

    #[test]
    fn non_fatal_worker_death_decides_trigger_shutdown() {
        assert_eq!(
            decide_monitor_action(true, false, EXIT_LISTENER_FATAL),
            Some(MonitorAction::TriggerShutdown),
            "a non-fatal worker's death must decide TriggerShutdown, not exit"
        );
    }

    #[test]
    fn no_death_decides_nothing_even_when_fatal_exit_is_configured() {
        assert_eq!(
            decide_monitor_action(false, true, EXIT_LISTENER_FATAL),
            None
        );
        assert_eq!(
            decide_monitor_action(false, false, EXIT_LISTENER_FATAL),
            None
        );
    }

    #[test]
    fn no_env_no_folders_is_empty() {
        assert!(merge_scan_roots(None, Vec::new()).is_empty());
    }

    #[test]
    fn persisted_folders_are_used_when_env_is_unset() {
        let roots = merge_scan_roots(None, vec!["D:/videos".into(), "E:/archive".into()]);
        assert_eq!(
            roots,
            vec![PathBuf::from("D:/videos"), PathBuf::from("E:/archive")],
        );
    }

    #[test]
    fn env_and_persisted_folders_are_unioned_without_duplicates() {
        let env = OsString::from("D:/videos");
        let roots = merge_scan_roots(Some(env), vec!["D:/videos".into(), "E:/new".into()]);
        assert_eq!(
            roots,
            vec![PathBuf::from("D:/videos"), PathBuf::from("E:/new")],
        );
    }

    #[test]
    fn empty_folder_strings_are_dropped() {
        let roots = merge_scan_roots(None, vec![String::new(), "F:/keep".into()]);
        assert_eq!(roots, vec![PathBuf::from("F:/keep")]);
    }

    #[test]
    fn vendored_ffmpeg_is_found_by_walking_up_from_the_exe_dir() {
        let base = tempfile::tempdir().expect("tempdir");
        let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
        let vendor = base.path().join("vendor").join("ffmpeg").join(&platform);
        std::fs::create_dir_all(&vendor).expect("mkdir vendor");
        let suffix = std::env::consts::EXE_SUFFIX;
        std::fs::write(vendor.join(format!("ffmpeg{suffix}")), b"").expect("ffmpeg");
        std::fs::write(vendor.join(format!("ffprobe{suffix}")), b"").expect("ffprobe");

        let exe_dir = base.path().join("target").join("debug");
        std::fs::create_dir_all(&exe_dir).expect("mkdir exe");

        let bins = vendored_ffmpeg(Some(&exe_dir)).expect("vendored ffmpeg found");
        assert!(
            bins.ffmpeg().starts_with(&vendor),
            "resolved under vendor dir"
        );
        assert!(bins.ffprobe().is_file());
    }

    #[test]
    fn vendored_ffmpeg_is_none_when_only_one_binary_is_present() {
        let base = tempfile::tempdir().expect("tempdir");
        let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
        let vendor = base.path().join("vendor").join("ffmpeg").join(&platform);
        std::fs::create_dir_all(&vendor).expect("mkdir vendor");
        let suffix = std::env::consts::EXE_SUFFIX;
        std::fs::write(vendor.join(format!("ffmpeg{suffix}")), b"").expect("ffmpeg");

        assert!(vendored_ffmpeg(Some(base.path())).is_none());
    }
}
