//! Reusable local query engine: immutable view binding, evidence queries, cursor
//! budgets, capture, refresh and language adapters. Transports belong to the host.
mod capture_cache;
pub mod config;
pub mod dependencies;
pub mod engine;
pub mod frontend;
pub mod git;
mod go;
mod implementations;
pub mod lsp;
mod process;
pub mod query;
mod scip;
mod structure;
mod tokens;
pub mod web;
pub mod workspace;

pub use engine::Engine;
pub use query::QueryService;
