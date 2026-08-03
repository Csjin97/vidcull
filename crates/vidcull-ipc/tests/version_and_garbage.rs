use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::AsyncWriteExt;
use vidcull_ipc::protocol::PROTOCOL_VERSION;
use vidcull_ipc::{IpcClient, IpcServer, Reply, Request, RequestHandler, Response, read_message};

fn unique_endpoint() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\vidcull-defence-{pid}-{n}")
    }
    #[cfg(unix)]
    {
        std::env::temp_dir()
            .join(format!("vidcull-defence-{pid}-{n}.sock"))
            .to_string_lossy()
            .into_owned()
    }
}

struct VersionedPingHandler {
    version: u32,
}

impl RequestHandler for VersionedPingHandler {
    fn handle(&self, request: Request) -> Reply {
        match request {
            Request::Ping => Reply::single(Response::Pong {
                protocol_version: self.version,
            }),
            _ => Reply::single(Response::StreamEnd),
        }
    }
}

struct Server {
    address: String,
    shutdown: Arc<tokio::sync::Notify>,
    serve: tokio::task::JoinHandle<vidcull_core::Result<()>>,
}

fn spawn_server(version: u32) -> Server {
    let server = IpcServer::bind(&unique_endpoint()).expect("bind");
    let address = server.address().to_owned();
    let handler = Arc::new(VersionedPingHandler { version });
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let sd = shutdown.clone();
    let serve = tokio::spawn(async move {
        server
            .serve(handler, async move { sd.notified().await })
            .await
    });
    Server {
        address,
        shutdown,
        serve,
    }
}

#[tokio::test]
async fn a_version_ahead_daemon_still_answers_ping_and_the_mismatch_is_detectable() {
    let server = spawn_server(PROTOCOL_VERSION + 1);
    let mut client = IpcClient::connect(&server.address).await.expect("connect");

    let resp = client.request(&Request::Ping).await.expect("ping");
    match resp {
        Response::Pong { protocol_version } => {
            assert_eq!(
                protocol_version,
                PROTOCOL_VERSION + 1,
                "daemon's version came through"
            );
            assert_ne!(
                protocol_version, PROTOCOL_VERSION,
                "a client comparing against its own constant detects the mismatch"
            );
        }
        other => panic!("expected Pong, got {other:?}"),
    }

    server.shutdown.notify_waiters();
    let _ = server.serve.await;
}

#[tokio::test]
async fn connect_negotiated_surfaces_a_mismatched_daemon_version() {
    let server = spawn_server(PROTOCOL_VERSION + 1);
    let (_client, daemon_version) = IpcClient::connect_negotiated(&server.address)
        .await
        .expect("handshake");
    assert_eq!(daemon_version, PROTOCOL_VERSION + 1);
    assert_ne!(
        daemon_version, PROTOCOL_VERSION,
        "a caller comparing against its own constant must detect the mismatch"
    );

    server.shutdown.notify_waiters();
    let _ = server.serve.await;
}

#[tokio::test]
async fn connect_negotiated_surfaces_an_older_daemon_version() {
    let server = spawn_server(PROTOCOL_VERSION - 1);
    let (_client, daemon_version) = IpcClient::connect_negotiated(&server.address)
        .await
        .expect("handshake");
    assert_eq!(daemon_version, PROTOCOL_VERSION - 1);
    assert_ne!(
        daemon_version, PROTOCOL_VERSION,
        "a newer client comparing against its own constant must detect the older daemon"
    );

    server.shutdown.notify_waiters();
    let _ = server.serve.await;
}

#[tokio::test]
async fn connect_negotiated_passes_a_matching_daemon_through() {
    let server = spawn_server(PROTOCOL_VERSION);
    let (mut client, daemon_version) = IpcClient::connect_negotiated(&server.address)
        .await
        .expect("handshake");
    assert_eq!(daemon_version, PROTOCOL_VERSION);

    let resp = client.request(&Request::Ping).await.expect("reuse");
    assert_eq!(
        resp,
        Response::Pong {
            protocol_version: PROTOCOL_VERSION
        },
        "the negotiated connection stays usable after the handshake"
    );

    server.shutdown.notify_waiters();
    let _ = server.serve.await;
}

#[tokio::test]
async fn reading_a_garbage_response_frame_is_an_error_not_a_panic() {
    let (mut a, mut b) = tokio::io::duplex(1024);
    a.write_u32(3).await.expect("len");
    a.write_all(&[0xFF, 0xFF, 0xFF]).await.expect("payload");
    a.flush().await.expect("flush");

    let result: vidcull_core::Result<Option<Response>> = read_message(&mut b).await;
    assert!(
        result.is_err(),
        "garbage decodes to an error, got {result:?}"
    );
}

#[tokio::test]
async fn server_survives_a_client_that_sends_a_garbage_frame() {
    let server = spawn_server(PROTOCOL_VERSION);

    {
        let mut raw = raw_connect(&server.address).await;
        raw.write_u32(3).await.expect("len");
        raw.write_all(&[0xFF, 0xFF, 0xFF]).await.expect("garbage");
        raw.flush().await.expect("flush");
    }

    let mut client = IpcClient::connect(&server.address)
        .await
        .expect("reconnect");
    let resp = client
        .request(&Request::Ping)
        .await
        .expect("ping after garbage");
    assert_eq!(
        resp,
        Response::Pong {
            protocol_version: PROTOCOL_VERSION
        },
        "server kept serving after the garbage connection"
    );

    server.shutdown.notify_waiters();
    let _ = server.serve.await;
}

#[cfg(unix)]
async fn raw_connect(address: &str) -> tokio::net::UnixStream {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match tokio::net::UnixStream::connect(address).await {
            Ok(stream) => return stream,
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(err) => panic!("raw connect: {err}"),
        }
    }
}

#[cfg(windows)]
async fn raw_connect(address: &str) -> tokio::net::windows::named_pipe::NamedPipeClient {
    use std::time::{Duration, Instant};
    use tokio::net::windows::named_pipe::ClientOptions;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match ClientOptions::new().open(address) {
            Ok(client) => return client,
            Err(err)
                if matches!(err.raw_os_error(), Some(231 | 2)) && Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(err) => panic!("raw connect: {err}"),
        }
    }
}
