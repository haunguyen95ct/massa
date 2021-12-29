use massa_time::MassaTime;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Max size for channels used with communication with other components.
pub const CHANNEL_SIZE: usize = 256;

/// Execution configuration
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ExecutionSettings {
    /// Initial SCE ledger file
    pub initial_sce_ledger_path: PathBuf,
    /// Thread count
    pub thread_count: u8,
    /// Genesis timestmap
    pub genesis_timestamp: MassaTime,
    /// period duration
    pub t0: MassaTime,
    /// clock compensation in milliseconds
    pub clock_compensation: i64,
}
