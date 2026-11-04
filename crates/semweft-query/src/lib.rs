//! Reusable local query engine: immutable view binding, evidence queries, cursor
//! budgets, capture, refresh and language adapters. Transports belong to the host.
mod api_surface;
pub mod architecture;
mod capture_cache;
pub mod config;
pub mod contracts;
mod cycles;
pub mod dependencies;
pub mod engine;
pub mod frontend;
pub mod git;
mod go;
pub mod guard;
mod impact_report;
mod implementations;
pub mod lsp;
mod module_resolution;
mod process;
pub mod query;
pub mod reviews;
pub mod risk_inputs;
mod scip;
pub mod services;
mod structure;
mod suggestions;
mod sync;
mod tokens;
mod verify;
pub mod web;
pub mod workspace;

pub use engine::Engine;
pub use go::is_build_input as is_go_build_input;
pub use query::QueryService;
