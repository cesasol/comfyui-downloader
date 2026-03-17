pub mod client;
pub mod protocol;
pub mod server;

pub use client::IpcClient;
pub use protocol::{EnrichedModel, Request, Response};
pub use server::IpcServer;
