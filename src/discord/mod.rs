//! Discord: the REST API, the live gateway connection, its data types, what
//! is known about servers and channels, and the QR-code login.

pub mod api;
pub mod gateway;
pub mod model;
#[cfg(feature = "discord")]
pub mod remote_auth;
pub mod state;
