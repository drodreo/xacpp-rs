//! Transfer layer: glues the pure reliability components into the
//! connection lifecycle (SPEC §5 session semantics).

mod clock;
mod core;
mod inbound;
mod sink;

pub use core::*;
pub(crate) use sink::open_binary_sink;
