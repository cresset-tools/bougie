//! Shared daemon state. Carries the shutdown channel + the
//! service-supervisor handle so every IPC connection can drive the
//! supervisor without re-resolving paths.

use super::ipc::ShutdownTx;
use super::supervisor::{Shared as SupervisorHandle, Supervisor};
use bougie_paths::Paths;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Long-lived state shared by every accepted IPC connection.
#[derive(Debug)]
pub struct DaemonState {
    pub paths: Paths,
    pub shutdown_tx: ShutdownTx,
    pub supervisor: SupervisorHandle,
}

impl DaemonState {
    pub fn new(paths: Paths, shutdown_tx: ShutdownTx) -> Self {
        let supervisor = Arc::new(Mutex::new(Supervisor::new(paths.clone())));
        Self { paths, shutdown_tx, supervisor }
    }
}
