use std::future::Future;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use vidcull_core::{Error, Result};

use crate::protocol::{MAX_FRAME_LEN, Request, Response};
use crate::{Reply, RequestHandler};

pub const MAX_CONNECTIONS: usize = 64;

const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

pub const EXIT_ALREADY_RUNNING: i32 = 2;

pub const EXIT_LISTENER_FATAL: i32 = 3;

pub enum BindOutcome {
    Bound(IpcServer),
    AlreadyRunning,
    Failed(Error),
}

pub async fn write_message<W, M>(writer: &mut W, message: &M) -> Result<()>
where
    W: AsyncWrite + Unpin,
    M: Serialize,
{
    let bytes = vidcull_core::encode(message)?;
    let len = u32::try_from(bytes.len()).map_err(|_| frame_too_large(bytes.len()))?;
    if len > MAX_FRAME_LEN {
        return Err(frame_too_large(bytes.len()));
    }
    writer.write_u32(len).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_message<R, M>(reader: &mut R) -> Result<Option<M>>
where
    R: AsyncRead + Unpin,
    M: DeserializeOwned,
{
    let len = match reader.read_u32().await {
        Ok(len) => len,
        Err(err) if is_clean_eof(&err) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    if len > MAX_FRAME_LEN {
        return Err(Error::Serialization(format!(
            "incoming frame of {len} bytes exceeds the {MAX_FRAME_LEN}-byte limit"
        )));
    }
    let mut buf = vec![0u8; len as usize];
    reader.read_exact(&mut buf).await?;
    Ok(Some(vidcull_core::decode(&buf)?))
}

fn is_clean_eof(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
    )
}

fn frame_too_large(len: usize) -> Error {
    Error::Serialization(format!(
        "outgoing frame of {len} bytes exceeds the {MAX_FRAME_LEN}-byte limit"
    ))
}

#[must_use]
pub fn default_endpoint() -> String {
    #[cfg(windows)]
    {
        r"\\.\pipe\vidcull".to_owned()
    }
    #[cfg(unix)]
    {
        std::env::var("XDG_RUNTIME_DIR")
            .map(|dir| format!("{dir}/vidcull.sock"))
            .unwrap_or_else(|_| "/tmp/vidcull.sock".to_owned())
    }
}

pub struct IpcServer {
    listener: imp::Listener,
    address: String,
    semaphore: Arc<Semaphore>,
}

impl IpcServer {
    pub fn bind(address: &str) -> Result<Self> {
        Self::bind_with_limit(address, MAX_CONNECTIONS)
    }

    pub fn bind_with_limit(address: &str, max_connections: usize) -> Result<Self> {
        Ok(Self {
            listener: imp::Listener::bind(address)?,
            address: address.to_owned(),
            semaphore: Arc::new(Semaphore::new(max_connections)),
        })
    }

    #[must_use]
    pub fn try_bind(address: &str) -> BindOutcome {
        match imp::Listener::bind(address) {
            Ok(listener) => BindOutcome::Bound(Self {
                listener,
                address: address.to_owned(),
                semaphore: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            }),
            Err(err) if imp::is_already_running_error(&err) => BindOutcome::AlreadyRunning,
            Err(err) => BindOutcome::Failed(err),
        }
    }

    #[must_use]
    pub fn address(&self) -> &str {
        &self.address
    }

    #[cfg(all(test, windows))]
    pub(crate) fn arm_fail_next_replacements(&mut self, n: usize) {
        self.listener.arm_fail_next_replacements(n);
    }

    pub async fn serve<H, F>(mut self, handler: Arc<H>, shutdown: F) -> Result<()>
    where
        H: RequestHandler,
        F: Future<Output = ()> + Send,
    {
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                () = &mut shutdown => break,
                accepted = self.listener.accept() => {
                    let stream = accepted?;
                    let handler = Arc::clone(&handler);
                    let permit = Arc::clone(&self.semaphore)
                        .acquire_owned()
                        .await
                        .expect("IPC semaphore closed unexpectedly");
                    tokio::spawn(async move {
                        let _permit: OwnedSemaphorePermit = permit;
                        if let Err(err) = serve_connection(stream, handler).await {
                            tracing::warn!(error = %err, "IPC connection ended with error");
                        }
                    });
                }
            }
        }
        Ok(())
    }
}

async fn serve_connection<S, H>(mut stream: S, handler: Arc<H>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    H: RequestHandler,
{
    let mut served_ping = false;
    let mut served_data = false;
    loop {
        let read_result =
            tokio::time::timeout(READ_IDLE_TIMEOUT, read_message::<_, Request>(&mut stream)).await;
        let request = match read_result {
            Ok(Ok(Some(req))) => req,
            Ok(Ok(None)) => break,
            Ok(Err(err)) => return Err(err),
            Err(_elapsed) => {
                return Err(Error::Io(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "IPC connection idle: no request received within the deadline",
                )));
            }
        };
        if matches!(request, Request::Ping) {
            served_ping = true;
        } else {
            served_data = true;
        }
        let h = Arc::clone(&handler);
        let reply = tokio::task::spawn_blocking(move || h.handle(request))
            .await
            .map_err(|e| Error::Io(io::Error::other(e)))?;
        match reply {
            Reply::Single(response) => {
                tokio::time::timeout(WRITE_TIMEOUT, write_message(&mut stream, &response))
                    .await
                    .map_err(|_| {
                        Error::Io(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "IPC write timed out: client not reading responses",
                        ))
                    })??;
            }
            Reply::Stream(items) => {
                for item in &items {
                    tokio::time::timeout(WRITE_TIMEOUT, write_message(&mut stream, item))
                        .await
                        .map_err(|_| {
                            Error::Io(io::Error::new(
                                io::ErrorKind::TimedOut,
                                "IPC stream write timed out: client not reading responses",
                            ))
                        })??;
                }
                tokio::time::timeout(
                    WRITE_TIMEOUT,
                    write_message(&mut stream, &Response::StreamEnd),
                )
                .await
                .map_err(|_| {
                    Error::Io(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "IPC stream-end write timed out: client not reading responses",
                    ))
                })??;
            }
        }
    }
    if served_ping && !served_data {
        tracing::warn!(
            daemon_protocol_version = crate::protocol::PROTOCOL_VERSION,
            "client disconnected after the version handshake without issuing a data request \
             — likely a protocol-version mismatch (client refused this daemon)"
        );
    }
    Ok(())
}

pub struct IpcClient {
    stream: imp::ClientStream,
}

impl IpcClient {
    pub async fn connect(address: &str) -> Result<Self> {
        Self::connect_timeout(address, Duration::from_secs(5)).await
    }

    pub async fn connect_timeout(address: &str, timeout: Duration) -> Result<Self> {
        let deadline = Instant::now() + timeout;
        Ok(Self {
            stream: imp::connect(address, deadline).await?,
        })
    }

    pub async fn connect_negotiated(address: &str) -> Result<(Self, u32)> {
        let mut client = Self::connect(address).await?;
        match client.request(&Request::Ping).await? {
            Response::Pong { protocol_version } => Ok((client, protocol_version)),
            other => Err(Error::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected Pong from the version handshake, got {other:?}"),
            ))),
        }
    }

    pub async fn connect_negotiated_probe(address: &str, timeout: Duration) -> Result<(Self, u32)> {
        let deadline = Instant::now() + timeout;
        let mut client = Self {
            stream: imp::connect_probe(address, deadline).await?,
        };
        match client.request(&Request::Ping).await? {
            Response::Pong { protocol_version } => Ok((client, protocol_version)),
            other => Err(Error::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected Pong from the version handshake, got {other:?}"),
            ))),
        }
    }

    pub async fn request(&mut self, request: &Request) -> Result<Response> {
        write_message(&mut self.stream, request).await?;
        read_message::<_, Response>(&mut self.stream)
            .await?
            .ok_or_else(|| {
                Error::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "daemon closed the connection without responding",
                ))
            })
    }

    pub async fn request_stream(&mut self, request: &Request) -> Result<Vec<Response>> {
        write_message(&mut self.stream, request).await?;
        let mut out = Vec::new();
        loop {
            match read_message::<_, Response>(&mut self.stream).await? {
                None | Some(Response::StreamEnd) => break,
                Some(response) => out.push(response),
            }
        }
        Ok(out)
    }
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::time::Instant;

    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };
    use tokio::time::{Duration, sleep};
    use vidcull_core::{Error, Result};

    const ERROR_FILE_NOT_FOUND: i32 = 2;
    const ERROR_PIPE_BUSY: i32 = 231;

    const ERROR_ACCESS_DENIED: i32 = 5;

    pub fn is_already_running_error(err: &Error) -> bool {
        match err {
            Error::Io(io_err) => io_err.raw_os_error() == Some(ERROR_ACCESS_DENIED),
            _ => false,
        }
    }

    pub type ServerStream = NamedPipeServer;
    pub type ClientStream = NamedPipeClient;

    pub(crate) const REPLACEMENT_CREATE_ATTEMPTS: usize = 4;
    const REPLACEMENT_CREATE_BACKOFF: std::time::Duration = std::time::Duration::from_millis(25);

    pub(crate) const CONNECT_ATTEMPTS: usize = 4;
    const CONNECT_BACKOFF: std::time::Duration = std::time::Duration::from_millis(25);

    pub struct Listener {
        address: String,
        next: Option<NamedPipeServer>,
        #[cfg(test)]
        forced_replacement_failures: usize,
        #[cfg(test)]
        forced_connect_failures: usize,
    }

    impl Listener {
        pub fn bind(address: &str) -> Result<Self> {
            let next = ServerOptions::new()
                .first_pipe_instance(true)
                .create(address)
                .map_err(Error::Io)?;
            Ok(Self {
                address: address.to_owned(),
                next: Some(next),
                #[cfg(test)]
                forced_replacement_failures: 0,
                #[cfg(test)]
                forced_connect_failures: 0,
            })
        }

        #[cfg(test)]
        pub fn arm_fail_next_replacements(&mut self, n: usize) {
            self.forced_replacement_failures = n;
        }

        #[cfg(test)]
        pub fn arm_fail_next_connects(&mut self, n: usize) {
            self.forced_connect_failures = n;
        }

        pub async fn accept(&mut self) -> Result<ServerStream> {
            let server = self.next.take().ok_or_else(|| {
                Error::Io(io::Error::other(
                    "IPC listener has no pending pipe instance \
                     (replacement creation previously failed)",
                ))
            })?;
            let mut last_connect_err: Option<io::Error> = None;
            let mut connected = false;
            for attempt in 1..=CONNECT_ATTEMPTS {
                #[cfg(test)]
                if self.forced_connect_failures > 0 {
                    self.forced_connect_failures -= 1;
                    last_connect_err = Some(io::Error::other("GATE-injected connect failure"));
                    if attempt < CONNECT_ATTEMPTS {
                        tokio::time::sleep(CONNECT_BACKOFF).await;
                    }
                    continue;
                }
                match server.connect().await {
                    Ok(()) => {
                        connected = true;
                        break;
                    }
                    Err(io_err) => {
                        last_connect_err = Some(io_err);
                        if attempt < CONNECT_ATTEMPTS {
                            tokio::time::sleep(CONNECT_BACKOFF).await;
                        }
                    }
                }
            }
            if !connected {
                let io_err =
                    last_connect_err.unwrap_or_else(|| io::Error::other("pipe connect failed"));
                tracing::error!(
                    address = %self.address,
                    os_error = ?io_err.raw_os_error(),
                    error = %io_err,
                    attempts = CONNECT_ATTEMPTS,
                    "IPC listener connect() failed after retries; no pipe \
                     instance remains for this address",
                );
                return Err(Error::Io(io_err));
            }
            let mut last_err: Option<io::Error> = None;
            for attempt in 1..=REPLACEMENT_CREATE_ATTEMPTS {
                #[cfg(test)]
                if self.forced_replacement_failures > 0 {
                    self.forced_replacement_failures -= 1;
                    last_err = Some(io::Error::other(
                        "GATE-injected replacement creation failure",
                    ));
                    if attempt < REPLACEMENT_CREATE_ATTEMPTS {
                        tokio::time::sleep(REPLACEMENT_CREATE_BACKOFF).await;
                    }
                    continue;
                }
                match ServerOptions::new().create(&self.address) {
                    Ok(replacement) => {
                        self.next = Some(replacement);
                        return Ok(server);
                    }
                    Err(io_err) => {
                        last_err = Some(io_err);
                        if attempt < REPLACEMENT_CREATE_ATTEMPTS {
                            tokio::time::sleep(REPLACEMENT_CREATE_BACKOFF).await;
                        }
                    }
                }
            }
            let io_err =
                last_err.unwrap_or_else(|| io::Error::other("replacement creation failed"));
            tracing::error!(
                address = %self.address,
                os_error = ?io_err.raw_os_error(),
                error = %io_err,
                attempts = REPLACEMENT_CREATE_ATTEMPTS,
                "IPC listener replacement-instance creation failed after retries; \
                 no pipe instance remains for this address",
            );
            Err(Error::Io(io_err))
        }
    }

    async fn connect_inner(
        address: &str,
        deadline: Instant,
        retry_not_found: bool,
    ) -> Result<ClientStream> {
        loop {
            match ClientOptions::new().open(address) {
                Ok(client) => return Ok(client),
                Err(err)
                    if err.raw_os_error() == Some(ERROR_PIPE_BUSY) && Instant::now() < deadline =>
                {
                    sleep(Duration::from_millis(20)).await;
                }
                Err(err)
                    if retry_not_found
                        && err.raw_os_error() == Some(ERROR_FILE_NOT_FOUND)
                        && Instant::now() < deadline =>
                {
                    sleep(Duration::from_millis(20)).await;
                }
                Err(err) => return Err(Error::Io(err)),
            }
        }
    }

    pub async fn connect(address: &str, deadline: Instant) -> Result<ClientStream> {
        connect_inner(address, deadline, true).await
    }

    pub async fn connect_probe(address: &str, deadline: Instant) -> Result<ClientStream> {
        connect_inner(address, deadline, false).await
    }
}

#[cfg(unix)]
mod imp {
    use std::io::ErrorKind;
    use std::time::Instant;

    use tokio::net::{UnixListener, UnixStream};
    use tokio::time::{Duration, sleep};
    use vidcull_core::{Error, Result};

    pub type ServerStream = UnixStream;
    pub type ClientStream = UnixStream;

    pub struct Listener {
        listener: UnixListener,
    }

    impl Listener {
        pub fn bind(address: &str) -> Result<Self> {
            let _ = std::fs::remove_file(address);
            Ok(Self {
                listener: UnixListener::bind(address)?,
            })
        }

        pub async fn accept(&mut self) -> Result<ServerStream> {
            let (stream, _addr) = self.listener.accept().await?;
            Ok(stream)
        }
    }

    pub fn is_already_running_error(err: &Error) -> bool {
        match err {
            Error::Io(io_err) => io_err.kind() == ErrorKind::AddrInUse,
            _ => false,
        }
    }

    async fn connect_inner(
        address: &str,
        deadline: Instant,
        retry_not_found: bool,
    ) -> Result<ClientStream> {
        loop {
            match UnixStream::connect(address).await {
                Ok(stream) => return Ok(stream),
                Err(err)
                    if err.kind() == ErrorKind::ConnectionRefused && Instant::now() < deadline =>
                {
                    sleep(Duration::from_millis(20)).await;
                }
                Err(err)
                    if retry_not_found
                        && err.kind() == ErrorKind::NotFound
                        && Instant::now() < deadline =>
                {
                    sleep(Duration::from_millis(20)).await;
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    pub async fn connect(address: &str, deadline: Instant) -> Result<ClientStream> {
        connect_inner(address, deadline, true).await
    }

    pub async fn connect_probe(address: &str, deadline: Instant) -> Result<ClientStream> {
        connect_inner(address, deadline, false).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn write_then_read_message_round_trips() {
        let (mut client, mut server) = duplex(1024);
        let request = Request::Ping;

        write_message(&mut client, &request).await.unwrap();

        let read: Option<Request> = read_message(&mut server).await.unwrap();
        assert_eq!(read, Some(Request::Ping));
    }

    #[tokio::test]
    async fn read_message_returns_none_on_eof() {
        let (client, mut server) = duplex(1024);
        drop(client);

        let read: Option<Request> = read_message(&mut server).await.unwrap();
        assert_eq!(read, None);
    }

    #[tokio::test]
    async fn read_message_rejects_oversized_frame() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut server) = duplex(1024);

        client.write_u32(MAX_FRAME_LEN + 1).await.unwrap();

        let res: Result<Option<Request>> = read_message(&mut server).await;
        assert!(res.is_err());
    }

    struct PingHandler;
    impl RequestHandler for PingHandler {
        fn handle(&self, request: Request) -> Reply {
            match request {
                Request::Ping => Reply::single(Response::Pong {
                    protocol_version: crate::protocol::PROTOCOL_VERSION,
                }),
                _ => Reply::single(Response::StreamEnd),
            }
        }
    }

    #[tokio::test]
    async fn serve_connection_handles_handshake_only_session() {
        use tokio::io::AsyncWriteExt;

        let (mut client, server) = duplex(1024);
        write_message(&mut client, &Request::Ping).await.unwrap();
        client.shutdown().await.unwrap();

        serve_connection(server, Arc::new(PingHandler))
            .await
            .unwrap();

        drop(client);
    }

    #[tokio::test]
    async fn write_message_rejects_oversized_payload() {
        use crate::protocol::IpcErrorKind;

        let (mut client, _) = duplex(1024);

        let huge_msg = Response::Error(crate::protocol::IpcError::new(
            IpcErrorKind::Internal,
            "A".repeat(MAX_FRAME_LEN as usize + 100),
        ));
        let res = write_message(&mut client, &huge_msg).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn half_open_client_times_out() {
        use tokio::time::timeout;

        let short = Duration::from_millis(200);

        let (_client, mut server_half) = duplex(1024);

        let result: std::result::Result<_, _> =
            timeout(short, read_message::<_, Request>(&mut server_half)).await;

        assert!(result.is_err(), "expected timeout Err but got {result:?}");
    }

    #[tokio::test]
    async fn clean_eof_not_mistaken_for_timeout() {
        use tokio::time::timeout;

        let short = Duration::from_millis(200);
        let (client, mut server_half) = duplex(1024);
        drop(client);

        let result = timeout(short, read_message::<_, Request>(&mut server_half)).await;
        assert!(
            matches!(result, Ok(Ok(None))),
            "expected Ok(Ok(None)) but got {result:?}"
        );
    }

    async fn serve_connection_with_timeout<S, H>(
        mut stream: S,
        handler: Arc<H>,
        read_idle: Duration,
        write_to: Duration,
    ) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
        H: RequestHandler,
    {
        loop {
            let read_result =
                tokio::time::timeout(read_idle, read_message::<_, Request>(&mut stream)).await;
            let request = match read_result {
                Ok(Ok(Some(req))) => req,
                Ok(Ok(None)) => break,
                Ok(Err(err)) => return Err(err),
                Err(_elapsed) => {
                    return Err(Error::Io(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "IPC connection idle: no request received within the deadline",
                    )));
                }
            };
            let h = Arc::clone(&handler);
            let reply = tokio::task::spawn_blocking(move || h.handle(request))
                .await
                .map_err(|e| Error::Io(io::Error::other(e)))?;
            match reply {
                Reply::Single(response) => {
                    tokio::time::timeout(write_to, write_message(&mut stream, &response))
                        .await
                        .map_err(|_| {
                            Error::Io(io::Error::new(
                                io::ErrorKind::TimedOut,
                                "IPC write timed out",
                            ))
                        })??;
                }
                Reply::Stream(items) => {
                    for item in &items {
                        tokio::time::timeout(write_to, write_message(&mut stream, item))
                            .await
                            .map_err(|_| {
                                Error::Io(io::Error::new(
                                    io::ErrorKind::TimedOut,
                                    "IPC stream write timed out",
                                ))
                            })??;
                    }
                    tokio::time::timeout(
                        write_to,
                        write_message(&mut stream, &Response::StreamEnd),
                    )
                    .await
                    .map_err(|_| {
                        Error::Io(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "IPC stream-end write timed out",
                        ))
                    })??;
                }
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn serve_connection_errors_on_idle_client() {
        let short = Duration::from_millis(200);
        let (_client_held_open, server_stream) = duplex(1024);

        let result = tokio::time::timeout(
            short * 5,
            serve_connection_with_timeout(server_stream, Arc::new(PingHandler), short, short),
        )
        .await
        .expect("test outer timeout: serve_connection did not return within budget");

        assert!(
            result.is_err(),
            "expected Err(TimedOut) but serve_connection returned Ok"
        );
    }

    #[tokio::test]
    async fn semaphore_bounds_concurrent_connections() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        const LIMIT: usize = 3;
        const TOTAL: usize = 12;

        let sem = Arc::new(Semaphore::new(LIMIT));
        let peak = Arc::new(AtomicUsize::new(0));
        let in_flight = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::with_capacity(TOTAL);
        for _ in 0..TOTAL {
            let sem = Arc::clone(&sem);
            let peak = Arc::clone(&peak);
            let in_flight = Arc::clone(&in_flight);
            handles.push(tokio::spawn(async move {
                let _permit = Arc::clone(&sem).acquire_owned().await.unwrap();
                let current = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(current, Ordering::SeqCst);
                tokio::task::yield_now().await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let observed_peak = peak.load(Ordering::SeqCst);
        assert!(
            observed_peak <= LIMIT,
            "in-flight peak {observed_peak} exceeded semaphore limit {LIMIT}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn spawn_blocking_handler_does_not_park_runtime() {
        use std::time::{Duration, Instant};

        const SLOW_MS: u64 = 300;
        const PING_BUDGET_MS: u64 = 150;

        struct SlowDataHandler;
        impl RequestHandler for SlowDataHandler {
            fn handle(&self, request: Request) -> Reply {
                if request == Request::Ping {
                    Reply::single(Response::Pong {
                        protocol_version: crate::protocol::PROTOCOL_VERSION,
                    })
                } else {
                    std::thread::sleep(Duration::from_millis(SLOW_MS));
                    Reply::single(Response::StreamEnd)
                }
            }
        }

        let handler = Arc::new(SlowDataHandler);

        let (mut client_a, server_a) = duplex(4096);
        let h_a = Arc::clone(&handler);
        tokio::spawn(async move {
            serve_connection(server_a, h_a).await.ok();
        });

        let (mut client_b, server_b) = duplex(4096);
        let h_b = Arc::clone(&handler);
        tokio::spawn(async move {
            serve_connection(server_b, h_b).await.ok();
        });

        write_message(&mut client_a, &Request::Progress)
            .await
            .unwrap();

        tokio::task::yield_now().await;

        let t0 = Instant::now();
        write_message(&mut client_b, &Request::Ping).await.unwrap();
        let _pong: Option<Response> = read_message(&mut client_b).await.unwrap();
        let elapsed = t0.elapsed();

        assert!(
            elapsed < Duration::from_millis(PING_BUDGET_MS),
            "Ping took {elapsed:?} — handler is parking the runtime worker (expected < {PING_BUDGET_MS} ms)"
        );

        let _: Option<Response> = read_message(&mut client_a).await.unwrap();
    }

    #[cfg(windows)]
    mod gate_218 {
        use super::super::imp::Listener;
        use std::time::{Duration, Instant};

        fn unique_addr(tag: &str) -> String {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let pid = std::process::id();
            format!(r"\\.\pipe\vidcull-gate-218-{tag}-{pid}-{n}")
        }

        #[tokio::test]
        async fn accept_surfaces_err_when_replacement_creation_fails() {
            let addr = unique_addr("surf");
            let mut listener = Listener::bind(&addr).expect("first bind");

            let dial_addr = addr.clone();
            let dialer = tokio::spawn(async move {
                super::super::imp::connect(&dial_addr, Instant::now() + Duration::from_secs(2))
                    .await
                    .expect("client connect")
            });

            listener.arm_fail_next_replacements(super::super::imp::REPLACEMENT_CREATE_ATTEMPTS);
            let result = listener.accept().await;
            assert!(
                result.is_err(),
                "accept() must return Err when the replacement instance's \
                 create() fails persistently — a silent Ok would hide the failure \
                 from spawn_worker_monitor entirely"
            );

            let _client = dialer.await.expect("dialer task");
        }

        #[tokio::test]
        async fn single_transient_replacement_failure_recovers() {
            let addr = unique_addr("recover");
            let mut listener = Listener::bind(&addr).expect("first bind");

            let dial_addr = addr.clone();
            let dialer = tokio::spawn(async move {
                super::super::imp::connect(&dial_addr, Instant::now() + Duration::from_secs(2))
                    .await
                    .expect("client connect")
            });

            listener.arm_fail_next_replacements(1);
            let accepted = listener.accept().await;
            assert!(
                accepted.is_ok(),
                "a single transient replacement-create failure must be retried, \
                 not fatal — got {accepted:?}"
            );
            let _client = dialer.await.expect("dialer task");

            let dial_addr2 = addr.clone();
            let dialer2 = tokio::spawn(async move {
                super::super::imp::connect(&dial_addr2, Instant::now() + Duration::from_secs(2))
                    .await
                    .expect("second client connect")
            });
            let second = listener.accept().await;
            assert!(
                second.is_ok(),
                "listener must keep accepting after recovery — got {second:?}"
            );
            let _client2 = dialer2.await.expect("second dialer task");
        }

        #[tokio::test]
        async fn zombie_window_allows_second_bind_to_succeed() {
            let addr = unique_addr("zombie");
            let mut first = Listener::bind(&addr).expect("first bind");

            let dial_addr = addr.clone();
            let warm_dialer = tokio::spawn(async move {
                super::super::imp::connect(&dial_addr, Instant::now() + Duration::from_secs(2))
                    .await
                    .expect("warm client connect")
            });
            let warm_server = first.accept().await.expect("warm accept");
            let warm_client = warm_dialer.await.expect("warm dialer task");
            drop(warm_client);
            drop(warm_server);
            tokio::time::sleep(Duration::from_millis(50)).await;

            let dial_addr = addr.clone();
            let dialer = tokio::spawn(async move {
                super::super::imp::connect(&dial_addr, Instant::now() + Duration::from_secs(2))
                    .await
                    .expect("second client connect")
            });
            first.arm_fail_next_replacements(super::super::imp::REPLACEMENT_CREATE_ATTEMPTS);
            let accept_result = first.accept().await;
            assert!(accept_result.is_err(), "sanity: injected failure fired");
            let second_client = dialer.await.expect("dialer task");
            drop(second_client);
            tokio::time::sleep(Duration::from_millis(50)).await;

            let second = Listener::bind(&addr);
            assert!(
                second.is_ok(),
                "a second Listener::bind must succeed once the first \
                 listener's replacement instance is gone and no accepted \
                 connection is outstanding — this is the split-brain \
                 reproduction: first daemon still alive, second daemon now \
                 owns the pipe name too"
            );

            drop(second);
            drop(first);
        }

        #[tokio::test]
        async fn second_bind_succeeds_immediately_after_serve_errors() {
            use crate::{IpcServer, Reply, RequestHandler};
            use std::sync::Arc;

            struct NoopHandler;
            impl RequestHandler for NoopHandler {
                fn handle(&self, _request: crate::Request) -> Reply {
                    Reply::single(crate::Response::Pong {
                        protocol_version: crate::protocol::PROTOCOL_VERSION,
                    })
                }
            }

            let addr = unique_addr("serve-err");
            let mut server = IpcServer::bind(&addr).expect("bind");

            server.arm_fail_next_replacements(super::super::imp::REPLACEMENT_CREATE_ATTEMPTS);
            let never_shuts_down = std::future::pending::<()>();
            let serve_handle =
                tokio::spawn(
                    async move { server.serve(Arc::new(NoopHandler), never_shuts_down).await },
                );

            tokio::time::sleep(Duration::from_millis(20)).await;
            let dial_addr = addr.clone();
            let dialer = tokio::spawn(async move {
                super::super::imp::connect(&dial_addr, Instant::now() + Duration::from_secs(2))
                    .await
                    .expect("triggering connect")
            });

            let serve_result = serve_handle.await.expect("serve task");
            let client = dialer.await.expect("dialer task");
            drop(client);

            assert!(
                serve_result.is_err(),
                "serve() must return Err the moment the accept loop's \
                 accepted? sees the injected replacement-creation failure — \
                 this is the only signal spawn_worker_monitor has"
            );

            tokio::time::sleep(Duration::from_millis(50)).await;
            let second = IpcServer::bind(&addr);
            assert!(
                second.is_ok(),
                "a second IpcServer::bind must succeed immediately after \
                 serve() errors out — the pipe name is free the instant the \
                 replacement-creation failure occurs, independent of whether \
                 or when the owning process actually exits"
            );
        }

        #[tokio::test]
        async fn single_transient_connect_failure_recovers() {
            let addr = unique_addr("conn-recover");
            let mut listener = Listener::bind(&addr).expect("first bind");

            let dial_addr = addr.clone();
            let dialer = tokio::spawn(async move {
                super::super::imp::connect(&dial_addr, Instant::now() + Duration::from_secs(2))
                    .await
                    .expect("client connect")
            });

            listener.arm_fail_next_connects(1);
            let accepted = listener.accept().await;
            assert!(
                accepted.is_ok(),
                "a single transient connect() failure must be retried, not \
                 fatal — got {accepted:?}"
            );
            let _client = dialer.await.expect("dialer task");

            let dial_addr2 = addr.clone();
            let dialer2 = tokio::spawn(async move {
                super::super::imp::connect(&dial_addr2, Instant::now() + Duration::from_secs(2))
                    .await
                    .expect("second client connect")
            });
            let second = listener.accept().await;
            assert!(
                second.is_ok(),
                "listener must keep accepting after connect recovery — got {second:?}"
            );
            let _client2 = dialer2.await.expect("second dialer task");
        }

        #[tokio::test]
        async fn persistent_connect_failure_surfaces_err() {
            let addr = unique_addr("conn-fatal");
            let mut listener = Listener::bind(&addr).expect("first bind");

            listener.arm_fail_next_connects(super::super::imp::CONNECT_ATTEMPTS);
            let result = listener.accept().await;
            assert!(
                result.is_err(),
                "accept() must return Err when connect() fails persistently — \
                 the daemon's only fatal-exit signal"
            );
        }
    }

    #[cfg(windows)]
    mod gate_218c {
        use super::super::imp;
        use std::time::{Duration, Instant};

        fn unique_addr(tag: &str) -> String {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let pid = std::process::id();
            format!(r"\\.\pipe\vidcull-gate-218c-{tag}-{pid}-{n}")
        }

        #[tokio::test]
        async fn connect_probe_fails_immediately_when_no_daemon_is_running() {
            let addr = unique_addr("no-daemon");
            let deadline = Instant::now() + Duration::from_secs(2);

            let t0 = Instant::now();
            let result = imp::connect_probe(&addr, deadline).await;
            let elapsed = t0.elapsed();

            assert!(
                result.is_err(),
                "connect_probe must fail when no listener exists for this address"
            );
            assert!(
                elapsed < Duration::from_millis(500),
                "connect_probe took {elapsed:?} to fail on a nonexistent pipe — \
                 expected near-instant (no listener means ERROR_FILE_NOT_FOUND \
                 immediately, not a transient condition worth retrying); a \
                 multi-second elapsed here means the c fast-fail \
                 regressed back to the full-deadline retry"
            );
        }

        #[tokio::test]
        async fn connect_still_retries_not_found_for_the_post_spawn_case() {
            let addr = unique_addr("post-spawn-shape");
            let deadline = Instant::now() + Duration::from_millis(150);

            let t0 = Instant::now();
            let result = imp::connect(&addr, deadline).await;
            let elapsed = t0.elapsed();

            assert!(
                result.is_err(),
                "no listener exists; connect must still fail eventually"
            );
            assert!(
                elapsed >= Duration::from_millis(100),
                "connect() (post-spawn shape) must retry ERROR_FILE_NOT_FOUND \
                 until its deadline, not fast-fail like connect_probe — got \
                 {elapsed:?}, expected it to ride out close to the 150ms deadline"
            );
        }

        #[tokio::test]
        async fn connect_probe_still_finds_a_live_daemon() {
            use tokio::net::windows::named_pipe::ServerOptions;

            let addr = unique_addr("live-daemon");
            let _server = ServerOptions::new()
                .first_pipe_instance(true)
                .create(&addr)
                .expect("bind test pipe");

            let deadline = Instant::now() + Duration::from_secs(2);
            let result = imp::connect_probe(&addr, deadline).await;
            assert!(
                result.is_ok(),
                "connect_probe must succeed against a real, live listener: {result:?}"
            );
        }
    }
}
