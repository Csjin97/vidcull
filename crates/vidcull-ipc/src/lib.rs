#![forbid(unsafe_code)]
#![allow(missing_docs)]

pub mod protocol;
pub mod reconnect;
pub mod transport;

pub use protocol::{
    Action, ActionResult, BestCopyMode, ClipOverlap, ClusterMemberDetail, ClusterStats,
    ClusterSummary, CpuThrottle, CrossGroupConflict, DaemonSettings, DeleteRequest, DeleteResult,
    FailedTask, FileDetail, GroupRole, GroupStats, GroupSummary, IpcError, IpcErrorKind, LogLevel,
    LogRecord, ProgressSnapshot, Request, Response, TrustLevel, UndoResult,
};
pub use transport::{
    BindOutcome, EXIT_ALREADY_RUNNING, EXIT_LISTENER_FATAL, IpcClient, IpcServer, default_endpoint,
    read_message, write_message,
};

pub trait RequestHandler: Send + Sync + 'static {
    fn handle(&self, request: Request) -> Reply;
}

pub enum Reply {
    Single(Response),
    Stream(Vec<Response>),
}

impl Reply {
    #[must_use]
    pub fn single(response: Response) -> Self {
        Self::Single(response)
    }

    #[must_use]
    pub fn stream(responses: Vec<Response>) -> Self {
        Self::Stream(responses)
    }
}
