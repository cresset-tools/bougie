//! Shared daemon state. Phase 1 ships only the shutdown channel; the
//! supervisor + tenant maps land in Phase 3.

use super::ipc::ShutdownTx;
use crate::Paths;

/// Long-lived state shared by every accepted IPC connection.
#[derive(Debug)]
pub struct DaemonState {
    #[allow(dead_code)] // used by future service.* methods
    pub paths: Paths,
    pub shutdown_tx: ShutdownTx,
}

impl DaemonState {
    pub fn new(paths: Paths, shutdown_tx: ShutdownTx) -> Self {
        Self { paths, shutdown_tx }
    }
}
