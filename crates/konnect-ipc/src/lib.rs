#[allow(clippy::all, warnings)]
pub mod gen;

pub mod builders;
pub mod client;
pub mod socket;
pub mod transform;
pub mod types;

pub use client::{is_transport_unreachable, IpcFailure, KiCadIpcClient, TransportUnreachable};
pub use socket::{candidate_socket_paths, detect_ipc_address};
pub use types::*;
