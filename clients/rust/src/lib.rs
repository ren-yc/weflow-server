//! Typed Rust client for weflow-server.
//!
//! Two layers with different ownership:
//!
//! - [`generated`] - request/response types and an operation client, produced from the
//!   server's OpenAPI description by a generator. Never edit by hand; rerun the
//!   generator (`cargo run -p weflow-regen`) and let CI's no-diff check pin the output.
//! - [`client`] - the handwritten behavior layer: readiness polling, cursor draining,
//!   SSE watching with Last-Event-ID reconnect, media retries. Behavior cannot be
//!   generated from a description of shapes; that is why this layer exists, and why it,
//!   not the generated face, is the API most downstreams should use.

pub mod client;
pub mod generated;
