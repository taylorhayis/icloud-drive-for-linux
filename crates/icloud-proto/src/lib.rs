//! iCloud.com web API client.
//!
//! This speaks the same unofficial Drive web service pyicloud uses. It is a
//! Rust implementation of that protocol, not a copy of any other project.

mod client;
mod cookies;
mod names;
mod parse;
mod srp_auth;

pub use client::{AuthStep, IcloudClient, SavedSession};
pub use names::local_name;
pub use parse::{RemoteKind, RemoteNode, ROOT_DRIVEWSID};
pub use srp_auth::derive_password;

pub const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
pub const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("iCloud rejected the sign-in")]
    BadCredentials,
    #[error("the transfer was cancelled")]
    Cancelled,
    #[error("iCloud error {status}: {message}")]
    Api { status: u16, message: String },
    #[error("{0}")]
    Message(String),
}

pub type Result<T> = std::result::Result<T, ProtoError>;
