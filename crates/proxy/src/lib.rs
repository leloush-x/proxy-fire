//! proxy-fire — OpenAI-compatible HTTP proxy with intelligent routing.

pub mod cache;
pub mod errors;
pub mod fallback;
pub mod middleware;
pub mod responses;
pub mod server;
pub mod sse;

pub use server::{AppState, Server};
