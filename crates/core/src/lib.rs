//! proxy-fire-core — domain logic for proxy-fire.
//! Config, catalog, model classification, strategies, rate limits, reliability.
//! No HTTP clients here; the network lives in providers/proxy.

pub mod capabilities;
pub mod catalog;
pub mod config;
pub mod dotenv;
pub mod enriched;
pub mod gemini;
pub mod gitsync;
pub mod headers;
pub mod models;
pub mod paths;
pub mod ratelimit;
pub mod reliability;
pub mod strategy;
pub mod watch;
