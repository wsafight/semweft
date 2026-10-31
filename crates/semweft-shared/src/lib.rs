//! Shared infrastructure with no code-analysis domain types or Semweft dependencies.
mod coordinates;
mod hash;

pub use coordinates::*;
pub use hash::{digest, identity};
