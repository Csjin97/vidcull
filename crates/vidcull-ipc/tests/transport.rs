use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use vidcull_ipc::protocol::PROTOCOL_VERSION;
use vidcull_ipc::{
    Action, ActionResult, BindOutcome, GroupSummary, IpcClient, IpcServer, LogLevel, LogRecord,
    ProgressSnapshot, Reply, Request, RequestHandler, Response, TrustLevel,
};

struct TestHandler;

impl RequestHandler for TestHandler {
    #[allow(clippy::too_many_lines)]
    fn handle(&self, request: Request) -> Reply {
        match request {
            Request::Ping => Reply::single(Response::Pong {
                protocol_version: PROTOCOL_VERSION,
            }),
            Request::Progress => Reply::single(Response::Progress(ProgressSnapshot {
                pending: 2,
                running: 1,
                done: 5,
                failed: 0,
                ..Default::default()
            })),
            Request::ListGroups { limit, .. } => {
                let count = i64::from(limit.min(3));
                let groups = (0..count)
                    .map(|i| GroupSummary {
                        group_id: i + 1,
                        trust: TrustLevel::Exact,
                        best_file_id: Some(i + 10),
                        member_count: 2,
                        intro_outro: false,
                    })
                    .collect();
                Reply::single(Response::Groups(groups))
            }
            Request::Action(Action::Shutdown) => Reply::single(Response::Action(ActionResult {
                accepted: true,
                detail: "shutting down".to_owned(),
            })),
            Request::Action(Action::Rescan { path }) => {
                Reply::single(Response::Action(ActionResult {
                    accepted: true,
                    detail: format!("rescan {path}"),
                }))
            }
            Request::StreamLogs { max_records } => {
                let logs = (0..max_records.min(3))
                    .map(|i| {
                        Response::Log(LogRecord {
                            timestamp_ms: i64::from(i),
                            level: LogLevel::Info,
                            target: "test".to_owned(),
                            message: format!("line {i}"),
                        })
                    })
                    .collect();
                Reply::stream(logs)
            }
            Request::Action(Action::MoveToTrash(_) | Action::DeletePermanent(_)) => {
                Reply::single(Response::Delete(vidcull_ipc::DeleteResult {
                    ok: false,
                    removed_file_ids: Vec::new(),
                    reclaimed_bytes: 0,
                    detail: "test handler does not delete".to_owned(),
                    reject_code: None,
                }))
            }
            Request::GroupDetail { .. } => Reply::single(Response::GroupDetail(Vec::new())),
            Request::GroupStats { .. } => {
                Reply::single(Response::GroupStats(vidcull_ipc::GroupStats::default()))
            }
            Request::PartialOverlaps { .. } => Reply::single(Response::PartialOverlaps(Vec::new())),
            Request::GetSettings | Request::Action(Action::SetSettings(_)) => {
                Reply::single(Response::Settings(vidcull_ipc::DaemonSettings::default()))
            }
            Request::ClusterSummaries { .. } => {
                Reply::single(Response::ClusterSummaries(Vec::new()))
            }
            Request::ClusterDetail { .. } => Reply::single(Response::ClusterDetail(Vec::new())),
            Request::ClusterStats { .. } => {
                Reply::single(Response::ClusterStats(vidcull_ipc::ClusterStats::default()))
            }
            Request::FailedTasks { .. } => Reply::single(Response::FailedTasks(Vec::new())),
            Request::CrossGroupConflicts { .. } => {
                Reply::single(Response::CrossGroupConflicts(Vec::new()))
            }
            Request::Thumbnail { .. } => Reply::single(Response::Thumbnail(None)),
            Request::Action(Action::UndoLastDelete) => {
                Reply::single(Response::Undo(vidcull_ipc::UndoResult {
                    ok: false,
                    group_id: None,
                    restored_file_ids: Vec::new(),
                    missing_paths: Vec::new(),
                    detail: "test handler does not undo".to_owned(),
                }))
            }
            Request::Action(Action::ForceRescan { path }) => {
                Reply::single(Response::Action(ActionResult {
                    accepted: true,
                    detail: format!("force rescan {path}"),
                }))
            }
            Request::Action(Action::SetLogLevel(level)) => {
                Reply::single(Response::Action(ActionResult {
                    accepted: true,
                    detail: format!("set log level {level:?}"),
                }))
            }
            Request::Action(Action::ExportDiagnostics { dest }) => {
                Reply::single(Response::Action(ActionResult {
                    accepted: true,
                    detail: format!("export diagnostics to {dest}"),
                }))
            }
        }
    }
}

fn unique_endpoint() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\vidcull-test-{pid}-{n}")
    }
    #[cfg(unix)]
    {
        std::env::temp_dir()
            .join(format!("vidcull-test-{pid}-{n}.sock"))
            .to_string_lossy()
            .into_owned()
    }
}

fn spawn_server() -> (String, oneshot::Sender<()>, JoinHandle<()>) {
    let server = IpcServer::bind(&unique_endpoint()).expect("bind server");
    let address = server.address().to_owned();
    let (tx, rx) = oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        let shutdown = async {
            let _ = rx.await;
        };
        let _ = server.serve(Arc::new(TestHandler), shutdown).await;
    });
    (address, tx, handle)
}

#[tokio::test]
async fn ping_round_trips() {
    let (address, shutdown, handle) = spawn_server();
    let mut client = IpcClient::connect(&address).await.expect("connect");
    let response = client.request(&Request::Ping).await.expect("request");
    assert_eq!(
        response,
        Response::Pong {
            protocol_version: PROTOCOL_VERSION
        }
    );
    drop(shutdown);
    let _ = handle.await;
}

#[tokio::test]
async fn progress_round_trips() {
    let (address, shutdown, handle) = spawn_server();
    let mut client = IpcClient::connect(&address).await.expect("connect");
    let response = client.request(&Request::Progress).await.expect("request");
    assert_eq!(
        response,
        Response::Progress(ProgressSnapshot {
            pending: 2,
            running: 1,
            done: 5,
            failed: 0,
            ..Default::default()
        })
    );
    drop(shutdown);
    let _ = handle.await;
}

#[tokio::test]
async fn list_groups_round_trips() {
    let (address, shutdown, handle) = spawn_server();
    let mut client = IpcClient::connect(&address).await.expect("connect");
    let response = client
        .request(&Request::ListGroups {
            trust: Some(TrustLevel::Exact),
            limit: 10,
            offset: 0,
        })
        .await
        .expect("request");
    let Response::Groups(groups) = response else {
        panic!("expected Groups, got {response:?}");
    };
    assert_eq!(groups.len(), 3);
    assert_eq!(groups[0].group_id, 1);
    assert_eq!(groups[0].trust, TrustLevel::Exact);
    drop(shutdown);
    let _ = handle.await;
}

#[tokio::test]
async fn action_round_trips() {
    let (address, shutdown, handle) = spawn_server();
    let mut client = IpcClient::connect(&address).await.expect("connect");
    let response = client
        .request(&Request::Action(Action::Rescan {
            path: "/lib".to_owned(),
        }))
        .await
        .expect("request");
    assert_eq!(
        response,
        Response::Action(ActionResult {
            accepted: true,
            detail: "rescan /lib".to_owned(),
        })
    );
    drop(shutdown);
    let _ = handle.await;
}

#[tokio::test]
async fn stream_logs_yields_multiple_frames_then_ends() {
    let (address, shutdown, handle) = spawn_server();
    let mut client = IpcClient::connect(&address).await.expect("connect");
    let frames = client
        .request_stream(&Request::StreamLogs { max_records: 3 })
        .await
        .expect("request stream");
    assert_eq!(frames.len(), 3, "StreamEnd must be consumed, not returned");
    assert!(matches!(frames[0], Response::Log(_)));
    drop(shutdown);
    let _ = handle.await;
}

#[tokio::test]
async fn multiple_requests_share_one_connection() {
    let (address, shutdown, handle) = spawn_server();
    let mut client = IpcClient::connect(&address).await.expect("connect");
    assert!(matches!(
        client.request(&Request::Ping).await.expect("ping"),
        Response::Pong { .. }
    ));
    assert!(matches!(
        client.request(&Request::Progress).await.expect("progress"),
        Response::Progress(_)
    ));
    assert!(matches!(
        client.request(&Request::Ping).await.expect("ping again"),
        Response::Pong { .. }
    ));
    drop(shutdown);
    let _ = handle.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n_concurrent_clients_are_stable() {
    const N: usize = 16;
    let (address, shutdown, handle) = spawn_server();

    let mut clients = Vec::with_capacity(N);
    for _ in 0..N {
        let address = address.clone();
        clients.push(tokio::spawn(async move {
            let mut client = IpcClient::connect(&address).await.expect("connect");
            client.request(&Request::Progress).await.expect("request")
        }));
    }

    for client in clients {
        let response = client.await.expect("join client task");
        assert_eq!(
            response,
            Response::Progress(ProgressSnapshot {
                pending: 2,
                running: 1,
                done: 5,
                failed: 0,
                ..Default::default()
            })
        );
    }

    drop(shutdown);
    let _ = handle.await;
}

#[tokio::test]
async fn framing_round_trips_over_a_duplex_pair() {
    let (mut a, mut b) = tokio::io::duplex(1024);
    let request = Request::ListGroups {
        trust: None,
        limit: 7,
        offset: 3,
    };
    vidcull_ipc::write_message(&mut a, &request)
        .await
        .expect("write");
    let decoded: Option<Request> = vidcull_ipc::read_message(&mut b).await.expect("read");
    assert_eq!(decoded, Some(request));
}

#[tokio::test]
async fn read_message_reports_clean_eof_as_none() {
    let (mut a, b) = tokio::io::duplex(64);
    drop(b);
    let decoded: Option<Request> = vidcull_ipc::read_message(&mut a).await.expect("read");
    assert!(decoded.is_none(), "a clean close must read back as None");
}

#[tokio::test]
async fn poll_progress_pattern_reuses_one_connection() {
    let (address, shutdown, handle) = spawn_server();
    let mut client = IpcClient::connect(&address).await.expect("connect");

    for i in 0..20 {
        let progress = client
            .request(&Request::Progress)
            .await
            .unwrap_or_else(|err| panic!("progress #{i} failed: {err}"));
        assert!(
            matches!(progress, Response::Progress(_)),
            "expected Progress, got {progress:?} at iteration {i}",
        );

        let failed = client
            .request(&Request::FailedTasks { limit: 100 })
            .await
            .unwrap_or_else(|err| panic!("failedTasks #{i} failed: {err}"));
        assert!(
            matches!(failed, Response::FailedTasks(_)),
            "expected FailedTasks, got {failed:?} at iteration {i}",
        );
    }

    drop(shutdown);
    let _ = handle.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_poll_progress_stress() {
    const N: usize = 8;
    const POLLS_PER_CLIENT: usize = 10;
    let (address, shutdown, handle) = spawn_server();

    let mut tasks = Vec::with_capacity(N);
    for client_id in 0..N {
        let address = address.clone();
        tasks.push(tokio::spawn(async move {
            let mut client = IpcClient::connect(&address)
                .await
                .unwrap_or_else(|err| panic!("client {client_id} connect: {err}"));
            for poll in 0..POLLS_PER_CLIENT {
                client
                    .request(&Request::Progress)
                    .await
                    .unwrap_or_else(|err| panic!("client {client_id} progress #{poll}: {err}"));
                client
                    .request(&Request::FailedTasks { limit: 100 })
                    .await
                    .unwrap_or_else(|err| panic!("client {client_id} failedTasks #{poll}: {err}"));
            }
        }));
    }

    for task in tasks {
        task.await.expect("join client task");
    }

    drop(shutdown);
    let _ = handle.await;
}

#[tokio::test]
async fn try_bind_on_free_endpoint_returns_bound() {
    let addr = unique_endpoint();
    let outcome = IpcServer::try_bind(&addr);
    assert!(
        matches!(outcome, BindOutcome::Bound(_)),
        "a free endpoint must yield BindOutcome::Bound"
    );
}

#[tokio::test]
async fn try_bind_on_occupied_endpoint_returns_already_running() {
    let addr = unique_endpoint();

    let first = match IpcServer::try_bind(&addr) {
        BindOutcome::Bound(s) => s,
        other => panic!(
            "first bind must succeed; got {}",
            match other {
                BindOutcome::AlreadyRunning => "AlreadyRunning",
                BindOutcome::Failed(_) => "Failed",
                BindOutcome::Bound(_) => unreachable!(),
            }
        ),
    };

    let outcome = IpcServer::try_bind(&addr);
    assert!(
        matches!(outcome, BindOutcome::AlreadyRunning),
        "second bind on a live endpoint must yield BindOutcome::AlreadyRunning"
    );

    drop(first);
}

#[tokio::test]
async fn try_bind_after_first_exits_returns_bound() {
    let addr = unique_endpoint();

    {
        let BindOutcome::Bound(first) = IpcServer::try_bind(&addr) else {
            panic!("first bind must succeed")
        };
        drop(first);
    }

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let outcome = IpcServer::try_bind(&addr);
    assert!(
        matches!(outcome, BindOutcome::Bound(_)),
        "re-binding after first instance exits must yield BindOutcome::Bound (no false AlreadyRunning)"
    );
}
