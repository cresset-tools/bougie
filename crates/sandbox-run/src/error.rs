use std::{error::Error, fmt, io};

#[derive(Debug)]
pub enum SandboxError {
    Io(io::Error),
    #[cfg(target_os = "macos")]
    MacOS(String),
    #[cfg(target_os = "linux")]
    Landlock(landlock::RulesetError),
    InvalidPath(String),
}

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SandboxError::Io(e) => write!(f, "I/O error: {}", e),
            #[cfg(target_os = "macos")]
            SandboxError::MacOS(msg) => write!(f, "macOS sandbox error: {}", msg),
            #[cfg(target_os = "linux")]
            SandboxError::Landlock(e) => write!(f, "Landlock error: {}", e),
            SandboxError::InvalidPath(msg) => write!(f, "Invalid path: {}", msg),
        }
    }
}

impl Error for SandboxError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            SandboxError::Io(e) => Some(e),
            #[cfg(target_os = "linux")]
            SandboxError::Landlock(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for SandboxError {
    fn from(e: io::Error) -> Self {
        SandboxError::Io(e)
    }
}

#[cfg(target_os = "linux")]
impl From<landlock::RulesetError> for SandboxError {
    fn from(e: landlock::RulesetError) -> Self {
        SandboxError::Landlock(e)
    }
}
