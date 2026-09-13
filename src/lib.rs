// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared library for the collector binary and its offline helper binaries.
//!
//! Everything here is usable from `src/main.rs` and from any `src/bin/*.rs`
//! without the `#[path = "../foo.rs"]` include trick, which duplicated a module
//! into every binary that wanted it.

pub mod build_info;
pub mod cpu;
pub mod export;
pub mod manifest;
pub mod privacy;
pub mod session;
pub mod timing;
