use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::ServiceKind;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReceiverMode {
    Starting,
    Control,
    Voice,
    Degraded,
    Stopped,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeviceRole {
    Control,
    Voice,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceState {
    pub role: DeviceRole,
    pub index: usize,
    pub connected: bool,
    pub tuned_hz: Option<u32>,
    pub power_dbfs: Option<f32>,
    pub serial: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveCall {
    pub transmission_id: String,
    pub talkgroup_id: u16,
    /// Talkgroup carried in the over-the-air grant/link control. This differs
    /// from `talkgroup_id` when the configured target is a member of a patch.
    pub traffic_talkgroup_id: u16,
    pub talkgroup_name: String,
    pub service: ServiceKind,
    pub frequency_hz: u32,
    pub source_unit: Option<u32>,
    pub encrypted: bool,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiverSnapshot {
    pub mode: ReceiverMode,
    pub site_name: String,
    pub system_name: String,
    pub control_frequency_hz: Option<u32>,
    pub control_locked: bool,
    pub active_call: Option<ActiveCall>,
    pub devices: Vec<DeviceState>,
    pub frames_decoded: u64,
    pub crc_errors: u64,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RadioEvent {
    Snapshot(ReceiverSnapshot),
    CallStarted(ActiveCall),
    CallEnded {
        transmission_id: String,
        talkgroup_id: u16,
        ended_at: DateTime<Utc>,
        reason: String,
    },
    ControlChannelChanged {
        frequency_hz: u32,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone)]
pub struct AudioFrame {
    pub sequence: u64,
    pub talkgroup_id: u16,
    pub samples: [i16; 160],
}

// Audio samples stay inline (see `P25Event`).
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum ArchiveEvent {
    CallStarted(ActiveCall),
    CallUpdated(ActiveCall),
    Audio {
        transmission_id: String,
        sequence: u64,
        samples: [i16; 160],
    },
    CallEnded {
        transmission_id: String,
        ended_at: DateTime<Utc>,
        reason: String,
    },
}
