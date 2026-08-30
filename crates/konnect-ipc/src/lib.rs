#[allow(clippy::all, warnings)]
pub mod gen;

pub mod builders;
pub mod client;
mod endpoint;
pub mod socket;
pub mod transform;
pub mod types;

pub use client::{
    is_transport_unreachable, ApiStatusError, BoardTargetError, IpcDocumentObservationError,
    IpcFailure, KiCadIpcClient, TransportUnreachable,
};
pub use endpoint::redact_endpoint;
pub use socket::{candidate_socket_paths, detect_ipc_address};
pub use types::*;
