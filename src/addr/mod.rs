//! Address → transaction index (`docs/address-index.md`).

pub mod build;
pub mod detect;
pub mod export;
pub mod fetch;
pub mod import;
pub mod io;
pub mod layout;
pub mod lookup;
pub mod slice;
pub mod stamps;
pub mod stats;
#[cfg(test)]
mod vectors;
pub mod verify;
