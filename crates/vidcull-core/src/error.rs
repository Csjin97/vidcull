use std::io;

use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("container parse error: {0}")]
    Parse(String),

    #[error("decode error: {0}")]
    Decode(String),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("invalid hash: {0}")]
    InvalidHash(String),

    #[error("unsupported: {0}")]
    Unsupported(String),

    #[error("database error: {0}")]
    Database(String),

    #[error("resource busy or locked: {0}")]
    Busy(String),

    #[error("decode cancelled")]
    Cancelled,
}

impl From<postcard::Error> for Error {
    fn from(value: postcard::Error) -> Self {
        Self::Serialization(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, ErrorKind};

    use super::*;

    #[test]
    fn io_error_converts_via_from() {
        let io_err = io::Error::new(ErrorKind::NotFound, "missing fixture");
        let err: Error = io_err.into();
        match err {
            Error::Io(inner) => assert_eq!(inner.kind(), ErrorKind::NotFound),
            other => panic!("expected Io variant, got {other:?}"),
        }
    }

    #[test]
    fn io_error_display_preserves_message() {
        let io_err = io::Error::new(ErrorKind::PermissionDenied, "no perms");
        let err: Error = io_err.into();
        let rendered = err.to_string();
        assert!(rendered.starts_with("I/O error: "), "got: {rendered}");
        assert!(rendered.contains("no perms"), "got: {rendered}");
    }

    #[test]
    fn postcard_error_converts_via_from() {
        let pc_err = postcard::Error::SerializeBufferFull;
        let err: Error = pc_err.into();
        match err {
            Error::Serialization(_) => {}
            other => panic!("expected Serialization variant, got {other:?}"),
        }
    }

    #[test]
    fn coarse_variants_render_their_payload() {
        assert!(
            Error::Parse("bad box header".into())
                .to_string()
                .contains("bad box header")
        );
        assert!(
            Error::Decode("av1 unsupported on fast path".into())
                .to_string()
                .contains("av1")
        );
        assert!(
            Error::InvalidHash("expected 64 hex chars".into())
                .to_string()
                .contains("hex")
        );
        assert!(
            Error::Unsupported("vp9 container variant".into())
                .to_string()
                .contains("vp9")
        );
        assert!(
            Error::Database("WAL checkpoint failed".into())
                .to_string()
                .contains("WAL")
        );
    }

    #[test]
    fn error_is_send_sync_static() {
        fn assert_bounds<T: Send + Sync + 'static>() {}
        assert_bounds::<Error>();
    }

    #[test]
    fn result_alias_defaults_to_crate_error() {
        fn produce() -> Result<u8> {
            Err(Error::Parse("nope".into()))
        }
        assert!(produce().is_err());
    }
}
