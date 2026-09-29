//! Decode the Improved Multi-Band Excitation (IMBE) digital voice codec.

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

extern crate arrayvec;
extern crate collect_slice;
extern crate crossbeam;
extern crate iq_osc;
extern crate num;
extern crate rand;
extern crate slice_mip;

pub mod allocs;
pub mod coefs;
pub mod consts;
pub mod decode;
pub mod descramble;
pub mod enhance;
pub mod frame;
pub mod gain;
pub mod params;
pub mod prev;
pub mod scan;
pub mod spectral;
pub mod unvoiced;
pub mod voiced;
pub mod window;

pub use decode::ImbeDecoder;
pub use frame::ReceivedFrame;
