//! World sessions: eidos-only arkhipelagos games kept by metaxy
//! (docs/SPEC.md, Part C).

pub mod client;
pub mod frames;
pub mod gen;
pub mod session;
pub mod store;

pub use session::{Joined, Worlds};
