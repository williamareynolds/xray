use thiserror::Error;
use serde_json::Error as JsonError;
use std::io::Error as IOError;

#[derive(Debug, Error)]
pub enum Error {
    /// Returned for general IO errors
    #[error("IO Error")]
    IO(IOError),
    /// Returned for serialization related errors
    #[error("Json Error")]
    Json(JsonError),
}

impl From<JsonError> for Error {
    fn from(err: JsonError) -> Self {
        Error::Json(err)
    }
}

impl From<IOError> for Error {
    fn from(err: IOError) -> Self {
        Error::IO(err)
    }
}
