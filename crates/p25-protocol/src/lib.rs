//! Implements the Project 25 (P25) air interface radio protocol, including baseband frame
//! synchronization, symbol decoding, error correction coding, and packet reconstuction.

// Vendored upstream code (see third-party/README.md). It is kept close to its
// MIT source rather than restyled, so clippy's default lints are not enforced
// on this crate; first-party crates are linted with `-D warnings` in CI.
#![allow(
    clippy::all,
    unused_doc_comments,
    unused_imports,
    dead_code,
    array_into_iter
)]

extern crate binfield_matrix;
extern crate cai_cyclic;
extern crate cai_golay;
extern crate collect_slice;
extern crate moving_avg;
extern crate num_traits;

#[cfg(feature = "ser")]
#[macro_use]
extern crate serde_derive;

#[cfg(feature = "ser")]
extern crate serde;

#[macro_use]
extern crate static_fir;

mod buffer;
mod util;

pub mod baseband;
pub mod bits;
pub mod coding;
pub mod consts;
pub mod data;
pub mod error;
pub mod message;
pub mod stats;
pub mod trunking;
pub mod voice;
