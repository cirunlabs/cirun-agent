use std::fmt;
use std::io;

#[derive(Debug)]
pub enum DockerError {
    Io(io::Error),
    CommandFailed { code: Option<i32>, stderr: String },
}

impl fmt::Display for DockerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DockerError::Io(e) => write!(f, "docker io error: {e}"),
            DockerError::CommandFailed { code, stderr } => {
                write!(f, "docker command failed (code={code:?}): {stderr}")
            }
        }
    }
}

impl std::error::Error for DockerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DockerError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for DockerError {
    fn from(e: io::Error) -> Self {
        DockerError::Io(e)
    }
}
