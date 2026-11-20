//! designllm-bridge: the protocol client used by the desktop GUI, by MCP stdio bridges, and by command
//! lines. It holds no document state. Every operation is a request to the workspace host.
//!
//! Public surface:
//! - [`Client`]: attach-only connection to a workspace host. `Client::new(workspace)` is the GUI client.
//!   `call(method, params)` returns the core result or the core/transport `DltError`.
//! - [`frame`] and [`protocol`]: the versioned `IpcMessage` framing (4-byte LE length, 8 MiB payload).
//! - [`run_stdio`]: the MCP 2025-11-25 stdio server (`--mcp`). With a `JobRef`, the connection is bound
//!   to that app job's attempt.
//!
//! Windows only: the transport is a named pipe with a current-user DACL.

#[cfg(not(windows))]
compile_error!("designllm-bridge implements the host transport with Windows named pipes only");

pub mod assets;
pub mod catalog;
pub mod client;
pub mod frame;
pub mod home;
pub mod manual;
pub mod mcp;
pub mod operation_exposure;
pub mod pipe;
pub mod protocol;
pub mod review_gate;

pub use client::{Client, ClientOptions, ClientRole, DEFAULT_CALL_TIMEOUT, HEARTBEAT_INTERVAL};
pub use designllm_contracts::{
    CoreEvent, DltError, DltResult, ErrorCode, IpcMessage, JobRef, RpcRequest, RpcResponse, MAX_FRAME_BYTES,
    PROTOCOL_VERSION,
};
pub use mcp::{run_stdio, McpServer, ToolBackend};
pub use pipe::pipe_name_for;
