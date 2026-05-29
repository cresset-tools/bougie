use std::{error::Error, fmt, io};

#[derive(Debug)]
pub enum SandboxError {
    Io(io::Error),
    #[cfg(target_os = "macos")]
    MacOS(String),
    #[cfg(target_os = "linux")]
    Landlock(landlock::RulesetError),
    /// Landlock ran but applied no enforcement (kernel < 5.13 or
    /// Landlock disabled) — the service would run completely
    /// unconfined, so we fail closed instead.
    #[cfg(target_os = "linux")]
    NotEnforced(String),
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
            #[cfg(target_os = "linux")]
            SandboxError::NotEnforced(msg) => write!(f, "sandbox not enforced: {}", msg),
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
