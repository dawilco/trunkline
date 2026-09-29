# Third-party Rust source

Trunkline contains small, locally maintained compatibility copies of dormant
pure-Rust radio crates. The original copyright notices and licenses are in
`licenses/`.

| Component | Upstream | Revision/source | License | Local change |
| --- | --- | --- | --- | --- |
| `p25-protocol` | `kchmck/p25.rs` | `a96c564` | MIT | Removed obsolete nightly feature gates; crate-level clippy allow; tests retained |
| `imbe-vocoder` | `kchmck/imbe.rs` | `2e17f5a` | MIT | Removed obsolete nightly feature gate; crate-level clippy allow |
| `p25-filters` | `kchmck/p25_filts.rs` | `0d34fc2` | MIT | Workspace packaging; crate-level clippy allow |
| `static_fir` | crates.io `static_fir 0.2.0` | published crate | MIT | Removed obsolete `conservative_impl_trait` feature gate |

These are Rust implementations, not wrappers around SDRTrunk, OP25, DSD, Java,
or an external decoder.
