use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use vidcull_ipc::protocol::PROTOCOL_VERSION;
use vidcull_ipc::reconnect::{BackoffPolicy, connect_with_backoff};
use vidcull_ipc::{IpcServer, Reply, Request, RequestHandler, Response};

struct PingHandler;

impl RequestHandler for PingHandler {
    fn handle(&self, request: Request) -> Reply {
        match request {
            Request::Ping => Reply::single(Response::Pong {
                protocol_version: PROTOCOL_VERSION,
            }),
            _ => Reply::single(Response::StreamEnd),
        }
    }
}

fn unique_endpoint() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\vidcull-reconnect-{pid}-{n}")
    }
    #[cfg(unix)]
    {
        std::env::temp_dir()
            .join(format!("vidcull-reconnect-{pid}-{n}.sock"))
            .to_string_lossy()
            .into_owned()
    }
}

fn fast_policy() -> BackoffPolicy {
    BackoffPolicy {
        base: Duration::from_millis(5),
        cap: Duration::from_millis(40),
        attempt_timeout: Duration::from_millis(20),
        max_attempts: 200,
    }
}

#[tokio::test]
async fn reconnects_once_the_daemon_comes_up() {
    let address = unique_endpoint();

    let server_address = address.clone();
    let server = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(120)).await;
        let server = IpcServer::bind(&server_address).expect("bind server late");
        server
            .serve(Arc::new(PingHandler), std::future::pending::<()>())
            .await
    });

    let mut client = connect_with_backoff(&address, fast_policy())
        .await
        .expect("reconnect should succeed once the listener appears");
    let response = client.request(&Request::Ping).await.expect("ping");
    assert_eq!(
        response,
        Response::Pong {
            protocol_version: PROTOCOL_VERSION
        }
    );

    server.abort();
}

#[tokio::test]
async fn gives_up_after_max_attempts_when_never_up() {
    let address = unique_endpoint();
    let policy = BackoffPolicy {
        base: Duration::from_millis(1),
        cap: Duration::from_millis(2),
        attempt_timeout: Duration::from_millis(5),
        max_attempts: 3,
    };
    let result = connect_with_backoff(&address, policy).await;
    assert!(
        result.is_err(),
        "connecting to an endpoint that never binds must fail, not hang"
    );
}
