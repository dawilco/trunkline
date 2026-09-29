use std::{collections::HashSet, fs, net::SocketAddr, path::Path};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub site: SiteConfig,
    pub radio: RadioConfig,
    #[serde(default)]
    pub archive: ArchiveConfig,
    #[serde(default)]
    pub transcription: TranscriptionConfig,
    #[serde(default)]
    pub talkgroups: Vec<TalkgroupConfig>,
}

impl AppConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let config: Self = toml::from_str(&raw)
            .with_context(|| format!("failed to parse config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.radio.control_device == self.radio.voice_device {
            bail!("control_device and voice_device must identify different RTL-SDRs");
        }
        if self.site.control_frequencies_hz.is_empty() {
            bail!("at least one control frequency is required");
        }
        if self.site.nac > 0x0fff {
            bail!("site.nac must be a 12-bit value");
        }
        if self.radio.sample_rate_hz != 240_000 {
            bail!("the current P25 receive chain requires a 240000 Hz SDR sample rate");
        }
        if !self.talkgroups.iter().any(|talkgroup| talkgroup.enabled)
            && !self.radio.monitor_unlisted
        {
            bail!("at least one talkgroup must be enabled unless radio.monitor_unlisted is true");
        }
        let mut talkgroup_ids = HashSet::new();
        if let Some(duplicate) = self
            .talkgroups
            .iter()
            .map(|talkgroup| talkgroup.id)
            .find(|id| !talkgroup_ids.insert(*id))
        {
            bail!("duplicate talkgroup id {duplicate}");
        }
        if self.radio.monitor_unlisted
            && let Some(talkgroup) = self
                .talkgroups
                .iter()
                .filter(|talkgroup| talkgroup.enabled)
                .find(|talkgroup| talkgroup.priority <= self.radio.unlisted_priority)
        {
            bail!(
                "radio.unlisted_priority ({}) must be lower than enabled configured talkgroup {} priority ({})",
                self.radio.unlisted_priority,
                talkgroup.id,
                talkgroup.priority
            );
        }
        if self.archive.enabled && self.archive.directory.trim().is_empty() {
            bail!("archive.directory must not be empty when archiving is enabled");
        }
        if self.transcription.enabled {
            if !self.archive.enabled {
                bail!("archiving must be enabled when transcription is enabled");
            }
            if self.transcription.model_path.trim().is_empty() {
                bail!("transcription.model_path must not be empty");
            }
            if self.transcription.model_name.trim().is_empty() {
                bail!("transcription.model_name must not be empty");
            }
            if self.transcription.language.trim().is_empty() {
                bail!("transcription.language must not be empty");
            }
            if self.transcription.initial_prompt.contains('\0') {
                bail!("transcription.initial_prompt must not contain a NUL byte");
            }
            if self.transcription.threads == 0 || self.transcription.threads > i32::MAX as usize {
                bail!("transcription.threads must fit in a positive 32-bit integer");
            }
            if self.transcription.minimum_audio_ms == 0 {
                bail!("transcription.minimum_audio_ms must be greater than zero");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub web_root: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SiteConfig {
    pub system: String,
    pub name: String,
    pub rfss: u8,
    pub site: u8,
    pub nac: u16,
    pub modulation: Modulation,
    pub control_frequencies_hz: Vec<u32>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Modulation {
    C4fm,
    Cqpsk,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RadioConfig {
    pub control_device: usize,
    pub voice_device: usize,
    #[serde(default = "default_sample_rate")]
    pub sample_rate_hz: u32,
    #[serde(default)]
    pub ppm: i32,
    pub gain_db: Option<f32>,
    /// Follow clear group calls whose talkgroup does not appear in
    /// `talkgroups`. Explicitly disabled talkgroups remain excluded.
    #[serde(default)]
    pub monitor_unlisted: bool,
    /// Priority assigned to dynamically discovered talkgroups. Validation
    /// requires this to remain below every enabled configured talkgroup.
    #[serde(default = "default_unlisted_priority")]
    pub unlisted_priority: u8,
}

const fn default_sample_rate() -> u32 {
    240_000
}

const fn default_unlisted_priority() -> u8 {
    0
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ArchiveConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_archive_directory")]
    pub directory: String,
}

impl Default for ArchiveConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            directory: default_archive_directory(),
        }
    }
}

fn default_archive_directory() -> String {
    "/data/transmissions".to_owned()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TranscriptionConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_model_path")]
    pub model_path: String,
    #[serde(default = "default_model_name")]
    pub model_name: String,
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default = "default_transcription_threads")]
    pub threads: usize,
    #[serde(default = "default_minimum_audio_ms")]
    pub minimum_audio_ms: u64,
    #[serde(default = "default_initial_prompt")]
    pub initial_prompt: String,
}

impl Default for TranscriptionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            model_path: default_model_path(),
            model_name: default_model_name(),
            language: default_language(),
            threads: default_transcription_threads(),
            minimum_audio_ms: default_minimum_audio_ms(),
            initial_prompt: default_initial_prompt(),
        }
    }
}

fn default_model_path() -> String {
    "/models/ggml-medium.en-q5_0.bin".to_owned()
}

fn default_model_name() -> String {
    "whisper-medium.en-q5_0".to_owned()
}

fn default_language() -> String {
    "en".to_owned()
}

const fn default_transcription_threads() -> usize {
    6
}

const fn default_minimum_audio_ms() -> u64 {
    750
}

fn default_initial_prompt() -> String {
    concat!(
        "Public safety two-way radio traffic. ",
        "Traffic may include police, sheriff, fire, EMS, mutual aid, public works, ",
        "transportation, utilities, schools, hospitals, security, and emergency operations. ",
        "Transcribe exactly, preserving unit numbers, street names, ten-codes, and dispatch terminology."
    )
    .to_owned()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TalkgroupConfig {
    pub id: u16,
    pub name: String,
    /// Optional one-line description shown on the dashboard card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub service: ServiceKind,
    #[serde(default)]
    pub priority: u8,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ServiceKind {
    Police,
    Fire,
    Ems,
    Other,
}

const fn default_enabled() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_site_and_target_talkgroups() {
        let config: AppConfig = toml::from_str(
            r#"
                [server]
                bind = "0.0.0.0:8097"
                web_root = "/app/web"

                [site]
                system = "Example Statewide 800"
                name = "Example County Simulcast"
                rfss = 2
                site = 23
                nac = 475
                modulation = "cqpsk"
                control_frequencies_hz = [853862500]

                [radio]
                control_device = 0
                voice_device = 1
                sample_rate_hz = 240000
                gain_db = 38.6

                [[talkgroups]]
                id = 44455
                name = "City Police Dispatch"
                service = "police"
                enabled = true
            "#,
        )
        .unwrap();

        config.validate().unwrap();
        assert_eq!(config.site.nac, 0x1db);
        assert_eq!(config.talkgroups[0].id, 44455);
        assert!(config.archive.enabled);
        assert_eq!(config.archive.directory, "/data/transmissions");
        assert!(config.transcription.enabled);
        assert_eq!(
            config.transcription.model_path,
            "/models/ggml-medium.en-q5_0.bin"
        );
        assert_eq!(config.transcription.threads, 6);
        assert!(!config.radio.monitor_unlisted);
        assert_eq!(config.radio.unlisted_priority, 0);
    }

    #[test]
    fn monitor_unlisted_can_run_without_configured_talkgroups() {
        let config: AppConfig = toml::from_str(
            r#"
                [server]
                bind = "0.0.0.0:8097"
                web_root = "/app/web"

                [site]
                system = "Example Statewide 800"
                name = "Example County Simulcast"
                rfss = 2
                site = 23
                nac = 475
                modulation = "cqpsk"
                control_frequencies_hz = [853862500]

                [radio]
                control_device = 0
                voice_device = 1
                monitor_unlisted = true
            "#,
        )
        .unwrap();

        config.validate().unwrap();
        assert!(config.radio.monitor_unlisted);
        assert_eq!(config.radio.unlisted_priority, 0);
        assert!(config.talkgroups.is_empty());
    }

    #[test]
    fn monitor_unlisted_priority_must_stay_below_configured_targets() {
        let config: AppConfig = toml::from_str(
            r#"
                [server]
                bind = "0.0.0.0:8097"
                web_root = "/app/web"

                [site]
                system = "Example Statewide 800"
                name = "Example County Simulcast"
                rfss = 2
                site = 23
                nac = 475
                modulation = "cqpsk"
                control_frequencies_hz = [853862500]

                [radio]
                control_device = 0
                voice_device = 1
                monitor_unlisted = true
                unlisted_priority = 10

                [[talkgroups]]
                id = 44455
                name = "City Police Dispatch"
                service = "police"
                priority = 10
            "#,
        )
        .unwrap();

        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("radio.unlisted_priority (10)"));
        assert!(error.contains("talkgroup 44455 priority (10)"));
    }
}
