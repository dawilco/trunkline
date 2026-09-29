//! Pure-Rust radio engine for the Trunkline P25 receiver.

#![forbid(unsafe_code)]

#[cfg(feature = "hardware")]
pub mod capture;
pub mod config;
pub mod decode;
pub mod device;
pub mod dsp;
#[cfg(feature = "hardware")]
pub mod engine;
pub mod patch;
pub mod replay;
pub mod state;

#[cfg(feature = "hardware")]
pub use capture::{CaptureOptions, CaptureResult, capture_sigmf};
pub use config::{
    AppConfig, ArchiveConfig, RadioConfig, ServerConfig, ServiceKind, SiteConfig, TalkgroupConfig,
    TranscriptionConfig,
};
pub use device::{DeviceInventory, SdrDevice};
#[cfg(feature = "hardware")]
pub use engine::{RadioEngine, RadioHandle};
pub use replay::{ReplaySummary, replay_cu8};
pub use state::{
    ActiveCall, ArchiveEvent, AudioFrame, DeviceRole, DeviceState, RadioEvent, ReceiverMode,
    ReceiverSnapshot,
};
