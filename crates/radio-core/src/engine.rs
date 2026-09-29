//! Dual-tuner control/voice runtime.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver as CommandReceiver, Sender as CommandSender},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use p25::{
    message::nid::DataUnit,
    trunking::{
        fields::{Channel, ChannelParamsMap, TalkGroup},
        tsbk::{
            self, MFID_MOTOROLA, MOTOROLA_PATCH_GROUP_ADD, MOTOROLA_PATCH_GROUP_DELETE,
            MOTOROLA_PATCH_GROUP_GRANT, MOTOROLA_PATCH_GROUP_UPDATE, MotorolaPatchDelete,
            MotorolaPatchGroup, TsbkOpcode,
        },
    },
    voice::{
        control::{GroupVoiceTraffic, LinkControlOpcode},
        crypto::CryptoAlgorithm,
    },
};
use tokio::sync::{broadcast, mpsc as tokio_mpsc, watch};

use crate::{
    AppConfig,
    config::{RadioConfig, ServiceKind, SiteConfig, TalkgroupConfig},
    decode::{P25Decoder, P25Event},
    device::DeviceInventory,
    dsp::P25Channel,
    patch::PatchRegistry,
    state::{
        ActiveCall, ArchiveEvent, AudioFrame, DeviceRole, DeviceState, RadioEvent, ReceiverMode,
        ReceiverSnapshot,
    },
};

const CONTROL_HUNT_DWELL: Duration = Duration::from_secs(3);
const CONTROL_LOSS_TIMEOUT: Duration = Duration::from_secs(6);
const VOICE_INACTIVITY_TIMEOUT: Duration = Duration::from_millis(2200);
const UPDATE_ONLY_REGRANT_SUPPRESSION: Duration = Duration::from_secs(3);
const POWER_UPDATE_INTERVAL: Duration = Duration::from_millis(500);
const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(500);
const RETUNE_SETTLE_CHUNKS: usize = 3;

#[derive(Clone)]
pub struct RadioHandle {
    state: watch::Receiver<ReceiverSnapshot>,
    events: broadcast::Sender<RadioEvent>,
    audio: broadcast::Sender<AudioFrame>,
    engine: tokio_mpsc::UnboundedSender<EngineEvent>,
    stop: Arc<AtomicBool>,
}

impl RadioHandle {
    pub fn snapshot(&self) -> ReceiverSnapshot {
        self.state.borrow().clone()
    }

    pub fn subscribe_state(&self) -> watch::Receiver<ReceiverSnapshot> {
        self.state.clone()
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<RadioEvent> {
        self.events.subscribe()
    }

    pub fn subscribe_audio(&self) -> broadcast::Receiver<AudioFrame> {
        self.audio.subscribe()
    }

    pub async fn shutdown(&self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.engine.send(EngineEvent::Shutdown);

        let mut state = self.state.clone();
        if state.borrow().mode == ReceiverMode::Stopped {
            return;
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), async move {
            while state.borrow().mode != ReceiverMode::Stopped {
                if state.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
    }
}

pub struct RadioEngine;

impl RadioEngine {
    pub fn start(config: AppConfig) -> Result<RadioHandle> {
        Self::start_inner(config, None)
    }

    pub fn start_with_archive(
        config: AppConfig,
        archive: tokio_mpsc::UnboundedSender<ArchiveEvent>,
    ) -> Result<RadioHandle> {
        Self::start_inner(config, Some(archive))
    }

    fn start_inner(
        config: AppConfig,
        archive: Option<tokio_mpsc::UnboundedSender<ArchiveEvent>>,
    ) -> Result<RadioHandle> {
        config.validate()?;
        let devices = DeviceInventory::discover().context("failed to enumerate RTL-SDR devices")?;
        let required_index = config.radio.control_device.max(config.radio.voice_device);
        if devices.len() <= required_index {
            bail!(
                "configuration needs RTL-SDR index {required_index}, but only {} device(s) are visible",
                devices.len()
            );
        }

        let now = Utc::now();
        let snapshot = ReceiverSnapshot {
            mode: ReceiverMode::Starting,
            site_name: config.site.name.clone(),
            system_name: config.site.system.clone(),
            control_frequency_hz: None,
            control_locked: false,
            active_call: None,
            devices: vec![
                DeviceState {
                    role: DeviceRole::Control,
                    index: config.radio.control_device,
                    connected: false,
                    tuned_hz: None,
                    power_dbfs: None,
                    serial: devices[config.radio.control_device].serial.clone(),
                },
                DeviceState {
                    role: DeviceRole::Voice,
                    index: config.radio.voice_device,
                    connected: false,
                    tuned_hz: None,
                    power_dbfs: None,
                    serial: devices[config.radio.voice_device].serial.clone(),
                },
            ],
            frames_decoded: 0,
            crc_errors: 0,
            started_at: now,
            updated_at: now,
            last_error: None,
        };

        let (state_tx, state_rx) = watch::channel(snapshot);
        let (event_tx, _) = broadcast::channel(256);
        let (audio_tx, _) = broadcast::channel(1024);
        let (engine_tx, engine_rx) = tokio_mpsc::unbounded_channel();
        let (voice_command_tx, voice_command_rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));

        spawn_control_worker(
            config.radio.clone(),
            config.site.clone(),
            engine_tx.clone(),
            stop.clone(),
        )?;
        spawn_voice_worker(
            config.radio.clone(),
            config.site.clone(),
            voice_command_rx,
            engine_tx.clone(),
            stop.clone(),
        )?;

        tokio::spawn(coordinate(
            config,
            engine_rx,
            voice_command_tx,
            state_tx,
            event_tx.clone(),
            audio_tx.clone(),
            archive,
        ));

        Ok(RadioHandle {
            state: state_rx,
            events: event_tx,
            audio: audio_tx,
            engine: engine_tx,
            stop,
        })
    }
}

#[derive(Debug, Clone)]
struct Grant {
    talkgroup_id: u16,
    traffic_talkgroup_id: u16,
    frequency_hz: u32,
    source_unit: Option<u32>,
    encrypted: bool,
}

fn select_grant_target(
    grants: Vec<Grant>,
    configured: &HashMap<u16, TalkgroupConfig>,
    listed: &HashSet<u16>,
    monitor_unlisted: bool,
    unlisted_priority: u8,
) -> Option<(Grant, TalkgroupConfig)> {
    let clear_grants = grants
        .into_iter()
        .filter(|grant| !grant.encrypted)
        .collect::<Vec<_>>();

    if let Some((grant, target)) = clear_grants
        .iter()
        .filter_map(|grant| {
            configured
                .get(&grant.talkgroup_id)
                .map(|target| (grant, target))
        })
        .max_by_key(|(_, target)| target.priority)
    {
        return Some((grant.clone(), target.clone()));
    }

    if !monitor_unlisted
        // A listed-but-disabled group is an explicit opt-out. For a patch
        // grant, suppress the wildcard if any resolved member was listed so
        // that the patch cannot bypass that opt-out.
        || clear_grants
            .iter()
            .any(|grant| listed.contains(&grant.talkgroup_id))
    {
        return None;
    }

    // Patch events contain a candidate for the over-the-air supergroup plus
    // candidates for all resolved members. When none is configured, archive
    // the call under the actual over-the-air TGID instead of inventing one
    // wildcard call per patch member.
    let grant = clear_grants
        .into_iter()
        .find(|grant| grant.talkgroup_id == grant.traffic_talkgroup_id)?;
    let talkgroup_id = grant.talkgroup_id;
    Some((
        grant,
        TalkgroupConfig {
            id: talkgroup_id,
            name: format!("TGID {talkgroup_id}"),
            description: None,
            service: ServiceKind::Other,
            priority: unlisted_priority,
            enabled: true,
        },
    ))
}

#[derive(Debug, Clone)]
struct VoiceAssignment {
    transmission_id: String,
    talkgroup_id: u16,
    traffic_talkgroup_id: u16,
    frequency_hz: u32,
}

enum VoiceCommand {
    Tune(VoiceAssignment),
}

// Audio samples stay inline (see `P25Event`); the channel is unbounded and
// every other variant is rare by comparison.
#[allow(clippy::large_enum_variant)]
enum EngineEvent {
    DeviceReady {
        role: DeviceRole,
        index: usize,
        frequency_hz: u32,
    },
    DevicePower {
        role: DeviceRole,
        power_dbfs: f32,
    },
    DeviceTuned {
        role: DeviceRole,
        frequency_hz: u32,
    },
    ControlLock {
        frequency_hz: u32,
        locked: bool,
    },
    Grants(Vec<Grant>),
    VoiceMetadata {
        transmission_id: String,
        talkgroup_id: u16,
        source_unit: Option<u32>,
        encrypted: bool,
    },
    Audio {
        transmission_id: String,
        talkgroup_id: u16,
        sequence: u64,
        samples: [i16; 160],
    },
    VoiceEnded {
        transmission_id: String,
        talkgroup_id: u16,
        frequency_hz: u32,
        reason: &'static str,
    },
    DecodeError {
        role: DeviceRole,
        message: String,
    },
    Fatal {
        role: DeviceRole,
        message: String,
    },
    Shutdown,
}

fn spawn_control_worker(
    radio: RadioConfig,
    site: SiteConfig,
    events: tokio_mpsc::UnboundedSender<EngineEvent>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    thread::Builder::new()
        .name("trunkline-control".to_owned())
        .spawn(move || {
            if let Err(error) = run_control_worker(&radio, &site, &events, &stop) {
                send_event(
                    &events,
                    EngineEvent::Fatal {
                        role: DeviceRole::Control,
                        message: format!("{error:#}"),
                    },
                );
            }
        })
        .context("failed to start control DSP thread")?;
    Ok(())
}

fn run_control_worker(
    radio: &RadioConfig,
    site: &SiteConfig,
    events: &tokio_mpsc::UnboundedSender<EngineEvent>,
    stop: &AtomicBool,
) -> Result<()> {
    let mut frequency_index = 0_usize;
    let mut frequency = site.control_frequencies_hz[frequency_index];
    let mut sdr = open_sdr(radio.control_device, frequency, radio)?;
    let reader = sdr.start_streaming()?;
    let control = reader.control_handle();
    send_event(
        events,
        EngineEvent::DeviceReady {
            role: DeviceRole::Control,
            index: radio.control_device,
            frequency_hz: frequency,
        },
    );
    send_event(
        events,
        EngineEvent::ControlLock {
            frequency_hz: frequency,
            locked: false,
        },
    );

    let mut channel = P25Channel::new();
    let mut decoder = P25Decoder::new();
    let mut params = ChannelParamsMap::default();
    let mut patches = PatchRegistry::default();
    let mut baseband = Vec::new();
    let mut last_sync = Instant::now();
    let mut last_power = Instant::now();
    let mut locked = false;

    while !stop.load(Ordering::Acquire) {
        let bytes = reader.recv().context("control RTL-SDR stream ended")?;
        let metrics = channel.process_cu8(&bytes, &mut baseband);
        if last_power.elapsed() >= POWER_UPDATE_INTERVAL {
            send_event(
                events,
                EngineEvent::DevicePower {
                    role: DeviceRole::Control,
                    power_dbfs: metrics.power_dbfs,
                },
            );
            last_power = Instant::now();
        }

        decoder.feed(&baseband, |event| match event {
            P25Event::NetworkId(nid)
                if nid.access_code.to_bits() == site.nac
                    && nid.data_unit == DataUnit::TrunkingSignaling =>
            {
                last_sync = Instant::now();
                if !locked {
                    locked = true;
                    send_event(
                        events,
                        EngineEvent::ControlLock {
                            frequency_hz: frequency,
                            locked: true,
                        },
                    );
                }
            }
            P25Event::Trunking(block) if block.crc_valid() => match block.mfg() {
                0 => match block.opcode() {
                    Some(TsbkOpcode::ChannelParamsUpdate) => {
                        params.update(&p25::trunking::fields::ChannelParamsUpdate::new(
                            block.payload(),
                        ));
                    }
                    Some(TsbkOpcode::GroupVoiceGrant | TsbkOpcode::GroupVoiceUpdateExplicit) => {
                        let grant = tsbk::GroupVoiceGrant::new(block);
                        emit_grant(
                            grant.talkgroup(),
                            grant.channel(),
                            Some(grant.src_unit()),
                            grant.opts().protected(),
                            &params,
                            events,
                        );
                    }
                    Some(TsbkOpcode::GroupVoiceUpdate) => {
                        let update =
                            p25::trunking::fields::GroupTrafficUpdate::new(block.payload());
                        for (channel, talkgroup) in update.updates() {
                            emit_grant(talkgroup, channel, None, false, &params, events);
                        }
                    }
                    _ => {}
                },
                MFID_MOTOROLA => match block.opcode_bits() {
                    MOTOROLA_PATCH_GROUP_ADD => {
                        let patch = MotorolaPatchGroup::new(block.payload());
                        patches.add(patch.supergroup(), patch.members());
                    }
                    MOTOROLA_PATCH_GROUP_DELETE => {
                        patches.delete(MotorolaPatchDelete::new(block.payload()).supergroup());
                    }
                    MOTOROLA_PATCH_GROUP_GRANT => {
                        let grant = tsbk::GroupVoiceGrant::new(block);
                        emit_patch_grants(
                            grant.talkgroup(),
                            grant.channel(),
                            Some(grant.src_unit()),
                            grant.opts().protected(),
                            &patches,
                            &params,
                            events,
                        );
                    }
                    MOTOROLA_PATCH_GROUP_UPDATE => {
                        let update =
                            p25::trunking::fields::GroupTrafficUpdate::new(block.payload());
                        for (channel, supergroup) in update.updates() {
                            emit_patch_grants(
                                supergroup, channel, None, false, &patches, &params, events,
                            );
                        }
                    }
                    _ => {}
                },
                _ => {}
            },
            P25Event::Error(error) => send_event(
                events,
                EngineEvent::DecodeError {
                    role: DeviceRole::Control,
                    message: format!("{error:?}"),
                },
            ),
            _ => {}
        });

        let loss_timeout = if locked {
            CONTROL_LOSS_TIMEOUT
        } else {
            CONTROL_HUNT_DWELL
        };
        if last_sync.elapsed() >= loss_timeout {
            locked = false;
            send_event(
                events,
                EngineEvent::ControlLock {
                    frequency_hz: frequency,
                    locked: false,
                },
            );
            frequency_index = (frequency_index + 1) % site.control_frequencies_hz.len();
            frequency = site.control_frequencies_hz[frequency_index];
            control.tune(corrected_frequency(frequency, radio.ppm))?;
            channel.reset();
            decoder.reset();
            params = ChannelParamsMap::default();
            patches = PatchRegistry::default();
            last_sync = Instant::now();
            send_event(
                events,
                EngineEvent::DeviceTuned {
                    role: DeviceRole::Control,
                    frequency_hz: frequency,
                },
            );
        }
    }

    Ok(())
}

fn emit_grant(
    talkgroup: TalkGroup,
    channel: Channel,
    source_unit: Option<u32>,
    encrypted: bool,
    params: &ChannelParamsMap,
    events: &tokio_mpsc::UnboundedSender<EngineEvent>,
) {
    let TalkGroup::Other(talkgroup_id) = talkgroup else {
        return;
    };
    let Some(grant) = resolve_grant(
        talkgroup_id,
        talkgroup_id,
        channel,
        source_unit,
        encrypted,
        params,
    ) else {
        return;
    };
    send_event(events, EngineEvent::Grants(vec![grant]));
}

fn emit_patch_grants(
    supergroup: TalkGroup,
    channel: Channel,
    source_unit: Option<u32>,
    encrypted: bool,
    patches: &PatchRegistry,
    params: &ChannelParamsMap,
    events: &tokio_mpsc::UnboundedSender<EngineEvent>,
) {
    let TalkGroup::Other(traffic_talkgroup_id) = supergroup else {
        return;
    };
    let grants = patches
        .grant_targets(traffic_talkgroup_id)
        .into_iter()
        .filter_map(|talkgroup_id| {
            resolve_grant(
                talkgroup_id,
                traffic_talkgroup_id,
                channel,
                source_unit,
                encrypted,
                params,
            )
        })
        .collect::<Vec<_>>();
    if !grants.is_empty() {
        send_event(events, EngineEvent::Grants(grants));
    }
}

fn resolve_grant(
    talkgroup_id: u16,
    traffic_talkgroup_id: u16,
    channel: Channel,
    source_unit: Option<u32>,
    encrypted: bool,
    params: &ChannelParamsMap,
) -> Option<Grant> {
    let identifier = params.lookup(channel.id())?;
    Some(Grant {
        talkgroup_id,
        traffic_talkgroup_id,
        frequency_hz: identifier.rx_freq(channel.number()),
        source_unit,
        encrypted,
    })
}

fn spawn_voice_worker(
    radio: RadioConfig,
    site: SiteConfig,
    commands: CommandReceiver<VoiceCommand>,
    events: tokio_mpsc::UnboundedSender<EngineEvent>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    thread::Builder::new()
        .name("trunkline-voice".to_owned())
        .spawn(move || {
            if let Err(error) = run_voice_worker(&radio, &site, commands, &events, &stop) {
                send_event(
                    &events,
                    EngineEvent::Fatal {
                        role: DeviceRole::Voice,
                        message: format!("{error:#}"),
                    },
                );
            }
        })
        .context("failed to start voice DSP thread")?;
    Ok(())
}

fn run_voice_worker(
    radio: &RadioConfig,
    site: &SiteConfig,
    commands: CommandReceiver<VoiceCommand>,
    events: &tokio_mpsc::UnboundedSender<EngineEvent>,
    stop: &AtomicBool,
) -> Result<()> {
    let park_frequency = site.control_frequencies_hz[0];
    let mut sdr = open_sdr(radio.voice_device, park_frequency, radio)?;
    let reader = sdr.start_streaming()?;
    let control = reader.control_handle();
    send_event(
        events,
        EngineEvent::DeviceReady {
            role: DeviceRole::Voice,
            index: radio.voice_device,
            frequency_hz: park_frequency,
        },
    );

    let mut assignment: Option<VoiceAssignment> = None;
    let mut channel = P25Channel::new();
    let mut decoder = P25Decoder::new();
    let mut baseband = Vec::new();
    let mut encrypted = false;
    let mut clear_voice_confirmed = false;
    let mut last_activity = Instant::now();
    let mut last_power = Instant::now();
    let mut sequence = 0_u64;
    let mut settle_chunks = 0_usize;

    while !stop.load(Ordering::Acquire) {
        for command in commands.try_iter() {
            match command {
                VoiceCommand::Tune(next) => {
                    control.tune(corrected_frequency(next.frequency_hz, radio.ppm))?;
                    channel.reset();
                    decoder.reset();
                    encrypted = false;
                    clear_voice_confirmed = false;
                    last_activity = Instant::now();
                    sequence = 0;
                    settle_chunks = RETUNE_SETTLE_CHUNKS;
                    send_event(
                        events,
                        EngineEvent::DeviceTuned {
                            role: DeviceRole::Voice,
                            frequency_hz: next.frequency_hz,
                        },
                    );
                    assignment = Some(next);
                }
            }
        }

        let bytes = reader.recv().context("voice RTL-SDR stream ended")?;
        let Some(current) = assignment.clone() else {
            continue;
        };
        if settle_chunks > 0 {
            settle_chunks -= 1;
            continue;
        }

        let metrics = channel.process_cu8(&bytes, &mut baseband);
        if last_power.elapsed() >= POWER_UPDATE_INTERVAL {
            send_event(
                events,
                EngineEvent::DevicePower {
                    role: DeviceRole::Voice,
                    power_dbfs: metrics.power_dbfs,
                },
            );
            last_power = Instant::now();
        }

        let mut end_reason = None;
        decoder.feed(&baseband, |event| match event {
            P25Event::NetworkId(nid) if nid.access_code.to_bits() == site.nac => {
                last_activity = Instant::now();
                if nid.data_unit == DataUnit::VoiceSimpleTerminator {
                    end_reason = Some("simple_terminator");
                }
            }
            P25Event::VoiceHeader(header) => {
                encrypted = header.crypto_alg() != CryptoAlgorithm::Unencrypted;
                clear_voice_confirmed = !encrypted;
                send_event(
                    events,
                    EngineEvent::VoiceMetadata {
                        transmission_id: current.transmission_id.clone(),
                        talkgroup_id: current.talkgroup_id,
                        source_unit: None,
                        encrypted,
                    },
                );
                if encrypted {
                    end_reason = Some("encrypted_voice_header");
                }
            }
            P25Event::Crypto(algorithm) => {
                encrypted = algorithm != CryptoAlgorithm::Unencrypted;
                clear_voice_confirmed = !encrypted;
                send_event(
                    events,
                    EngineEvent::VoiceMetadata {
                        transmission_id: current.transmission_id.clone(),
                        talkgroup_id: current.talkgroup_id,
                        source_unit: None,
                        encrypted,
                    },
                );
                if encrypted {
                    end_reason = Some("encrypted_sync");
                }
            }
            P25Event::LinkControl(link)
                if link.opcode() == Some(LinkControlOpcode::GroupVoiceTraffic) =>
            {
                let traffic = GroupVoiceTraffic::new(link);
                let talkgroup_matches = matches!(
                    traffic.talkgroup(),
                    TalkGroup::Other(talkgroup_id)
                        if talkgroup_id == current.traffic_talkgroup_id
                            || talkgroup_id == current.talkgroup_id
                );
                if talkgroup_matches {
                    last_activity = Instant::now();
                    encrypted |= traffic.opts().protected();
                    clear_voice_confirmed = !encrypted;
                    send_event(
                        events,
                        EngineEvent::VoiceMetadata {
                            transmission_id: current.transmission_id.clone(),
                            talkgroup_id: current.talkgroup_id,
                            source_unit: Some(traffic.src_unit()),
                            encrypted,
                        },
                    );
                    if encrypted {
                        end_reason = Some("encrypted_link_control");
                    }
                }
            }
            P25Event::Audio(samples) if clear_voice_confirmed && !encrypted => {
                last_activity = Instant::now();
                sequence += 1;
                send_event(
                    events,
                    EngineEvent::Audio {
                        transmission_id: current.transmission_id.clone(),
                        talkgroup_id: current.talkgroup_id,
                        sequence,
                        samples,
                    },
                );
            }
            P25Event::VoiceTerm(_) => {
                end_reason = Some("link_control_terminator");
            }
            P25Event::Error(error) => send_event(
                events,
                EngineEvent::DecodeError {
                    role: DeviceRole::Voice,
                    message: format!("{error:?}"),
                },
            ),
            _ => {}
        });

        if end_reason.is_some() || last_activity.elapsed() >= VOICE_INACTIVITY_TIMEOUT {
            send_event(
                events,
                EngineEvent::VoiceEnded {
                    transmission_id: current.transmission_id.clone(),
                    talkgroup_id: current.talkgroup_id,
                    frequency_hz: current.frequency_hz,
                    reason: end_reason.unwrap_or("inactivity_timeout"),
                },
            );
            assignment = None;
            decoder.reset();
        }
    }

    Ok(())
}

fn open_sdr(index: usize, frequency_hz: u32, radio: &RadioConfig) -> Result<rs_rtl::RtlSdr> {
    let mut sdr = rs_rtl::RtlSdr::open(rs_rtl::DeviceId::Index(index))
        .with_context(|| format!("failed to open RTL-SDR index {index}"))?;
    sdr.set_sample_rate(radio.sample_rate_hz)?;
    let _ = sdr.set_bandwidth(radio.sample_rate_hz)?;
    sdr.set_center_freq(corrected_frequency(frequency_hz, radio.ppm))?;
    match radio.gain_db {
        Some(gain) => sdr.set_gain_manual((gain * 10.0).round() as i32)?,
        None => sdr.set_gain_auto()?,
    }
    Ok(sdr)
}

fn corrected_frequency(frequency_hz: u32, ppm: i32) -> u32 {
    let nominal = frequency_hz as i64;
    let adjusted = nominal + nominal * ppm as i64 / 1_000_000;
    adjusted.clamp(0, u32::MAX as i64) as u32
}

fn send_event(sender: &tokio_mpsc::UnboundedSender<EngineEvent>, event: EngineEvent) {
    let _ = sender.send(event);
}

async fn coordinate(
    config: AppConfig,
    mut receiver: tokio_mpsc::UnboundedReceiver<EngineEvent>,
    voice_commands: CommandSender<VoiceCommand>,
    state: watch::Sender<ReceiverSnapshot>,
    events: broadcast::Sender<RadioEvent>,
    audio: broadcast::Sender<AudioFrame>,
    archive: Option<tokio_mpsc::UnboundedSender<ArchiveEvent>>,
) {
    let monitor_unlisted = config.radio.monitor_unlisted;
    let unlisted_priority = config.radio.unlisted_priority;
    let listed_talkgroups = config
        .talkgroups
        .iter()
        .map(|talkgroup| talkgroup.id)
        .collect::<HashSet<_>>();
    let talkgroups: HashMap<u16, TalkgroupConfig> = config
        .talkgroups
        .into_iter()
        .filter(|talkgroup| talkgroup.enabled)
        .map(|talkgroup| (talkgroup.id, talkgroup))
        .collect();
    let mut snapshot = state.borrow().clone();
    let mut active_priority = None;
    let mut recently_ended_call: Option<(u16, u32, Instant)> = None;
    let mut last_snapshot = Instant::now();
    let mut transmission_sequence = 0_u64;

    while let Some(event) = receiver.recv().await {
        let mut important = false;
        match event {
            EngineEvent::DeviceReady {
                role,
                index,
                frequency_hz,
            } => {
                update_device(&mut snapshot, role, |device| {
                    device.index = index;
                    device.connected = true;
                    device.tuned_hz = Some(frequency_hz);
                });
                tracing::info!(?role, index, frequency_hz, "RTL-SDR ready");
                important = true;
            }
            EngineEvent::DevicePower { role, power_dbfs } => {
                update_device(&mut snapshot, role, |device| {
                    device.power_dbfs = Some(power_dbfs);
                });
            }
            EngineEvent::DeviceTuned { role, frequency_hz } => {
                update_device(&mut snapshot, role, |device| {
                    device.tuned_hz = Some(frequency_hz);
                });
                if role == DeviceRole::Control {
                    snapshot.control_frequency_hz = Some(frequency_hz);
                    let _ = events.send(RadioEvent::ControlChannelChanged { frequency_hz });
                }
                important = true;
            }
            EngineEvent::ControlLock {
                frequency_hz,
                locked,
            } => {
                snapshot.control_frequency_hz = Some(frequency_hz);
                snapshot.control_locked = locked;
                if snapshot.active_call.is_none() {
                    snapshot.mode = if locked {
                        ReceiverMode::Control
                    } else {
                        ReceiverMode::Starting
                    };
                }
                tracing::info!(frequency_hz, locked, "control channel lock changed");
                important = true;
            }
            EngineEvent::Grants(grants) => {
                let Some((grant, target)) = select_grant_target(
                    grants,
                    &talkgroups,
                    &listed_talkgroups,
                    monitor_unlisted,
                    unlisted_priority,
                ) else {
                    continue;
                };
                if should_suppress_update_only_regrant(&grant, recently_ended_call) {
                    continue;
                }
                let current_priority = active_priority.unwrap_or(0);
                let same_call = snapshot.active_call.as_ref().is_some_and(|call| {
                    call.talkgroup_id == grant.talkgroup_id
                        && call.frequency_hz == grant.frequency_hz
                });
                if same_call {
                    let mut call_updated = false;
                    if let Some(call) = snapshot.active_call.as_mut() {
                        let original_source = call.source_unit;
                        let original_traffic_talkgroup = call.traffic_talkgroup_id;
                        call.source_unit = call.source_unit.or(grant.source_unit);
                        call.traffic_talkgroup_id = grant.traffic_talkgroup_id;
                        call_updated = call.source_unit != original_source
                            || call.traffic_talkgroup_id != original_traffic_talkgroup;
                        if call_updated {
                            if let Some(archive) = &archive {
                                let _ = archive.send(ArchiveEvent::CallUpdated(call.clone()));
                            }
                        }
                    }
                    if call_updated {
                        snapshot.updated_at = Utc::now();
                        let _ = state.send(snapshot.clone());
                        let _ = events.send(RadioEvent::Snapshot(snapshot.clone()));
                        last_snapshot = Instant::now();
                    }
                    continue;
                }
                if snapshot.active_call.is_some()
                    && !should_preempt(target.priority, current_priority)
                {
                    continue;
                }

                if let Some(previous) = snapshot.active_call.take() {
                    recently_ended_call =
                        Some((previous.talkgroup_id, previous.frequency_hz, Instant::now()));
                    let ended_at = Utc::now();
                    let reason = "preempted".to_owned();
                    if let Some(archive) = &archive {
                        let _ = archive.send(ArchiveEvent::CallEnded {
                            transmission_id: previous.transmission_id.clone(),
                            ended_at,
                            reason: reason.clone(),
                        });
                    }
                    let _ = events.send(RadioEvent::CallEnded {
                        transmission_id: previous.transmission_id,
                        talkgroup_id: previous.talkgroup_id,
                        ended_at,
                        reason,
                    });
                    active_priority = None;
                }
                transmission_sequence = transmission_sequence.wrapping_add(1);
                let started_at = Utc::now();
                let call = ActiveCall {
                    transmission_id: format!(
                        "tx-{}-{}-{}",
                        started_at.timestamp_millis(),
                        target.id,
                        transmission_sequence
                    ),
                    talkgroup_id: target.id,
                    traffic_talkgroup_id: grant.traffic_talkgroup_id,
                    talkgroup_name: target.name.clone(),
                    service: target.service,
                    frequency_hz: grant.frequency_hz,
                    source_unit: grant.source_unit,
                    encrypted: false,
                    started_at,
                };
                if voice_commands
                    .send(VoiceCommand::Tune(VoiceAssignment {
                        transmission_id: call.transmission_id.clone(),
                        talkgroup_id: call.talkgroup_id,
                        traffic_talkgroup_id: call.traffic_talkgroup_id,
                        frequency_hz: call.frequency_hz,
                    }))
                    .is_err()
                {
                    snapshot.last_error = Some("voice worker command channel closed".to_owned());
                    snapshot.mode = ReceiverMode::Degraded;
                    important = true;
                } else {
                    active_priority = Some(target.priority);
                    snapshot.active_call = Some(call.clone());
                    snapshot.mode = ReceiverMode::Voice;
                    tracing::info!(
                        transmission_id = %call.transmission_id,
                        talkgroup_id = call.talkgroup_id,
                        traffic_talkgroup_id = call.traffic_talkgroup_id,
                        talkgroup = %call.talkgroup_name,
                        frequency_hz = call.frequency_hz,
                        source_unit = ?call.source_unit,
                        "following target call"
                    );
                    if let Some(archive) = &archive {
                        let _ = archive.send(ArchiveEvent::CallStarted(call.clone()));
                    }
                    let _ = events.send(RadioEvent::CallStarted(call));
                    important = true;
                }
            }
            EngineEvent::VoiceMetadata {
                transmission_id,
                talkgroup_id,
                source_unit,
                encrypted,
            } => {
                if let Some(call) = snapshot.active_call.as_mut().filter(|call| {
                    call.transmission_id == transmission_id && call.talkgroup_id == talkgroup_id
                }) {
                    let original_source = call.source_unit;
                    let original_encrypted = call.encrypted;
                    call.source_unit = source_unit.or(call.source_unit);
                    call.encrypted = encrypted;
                    if call.source_unit != original_source || call.encrypted != original_encrypted {
                        if let Some(archive) = &archive {
                            let _ = archive.send(ArchiveEvent::CallUpdated(call.clone()));
                        }
                        important = true;
                    }
                }
            }
            EngineEvent::Audio {
                transmission_id,
                talkgroup_id,
                sequence,
                samples,
            } => {
                if snapshot.active_call.as_ref().is_some_and(|call| {
                    call.transmission_id == transmission_id && call.talkgroup_id == talkgroup_id
                }) {
                    snapshot.frames_decoded += 1;
                    if let Some(archive) = &archive {
                        let _ = archive.send(ArchiveEvent::Audio {
                            transmission_id,
                            sequence,
                            samples,
                        });
                    }
                    let _ = audio.send(AudioFrame {
                        sequence,
                        talkgroup_id,
                        samples,
                    });
                }
            }
            EngineEvent::VoiceEnded {
                transmission_id,
                talkgroup_id,
                frequency_hz,
                reason,
            } => {
                if snapshot.active_call.as_ref().is_some_and(|call| {
                    call.transmission_id == transmission_id
                        && call.talkgroup_id == talkgroup_id
                        && call.frequency_hz == frequency_hz
                }) {
                    let call = snapshot
                        .active_call
                        .take()
                        .expect("active call was checked above");
                    recently_ended_call =
                        Some((call.talkgroup_id, call.frequency_hz, Instant::now()));
                    active_priority = None;
                    snapshot.mode = if snapshot.control_locked {
                        ReceiverMode::Control
                    } else {
                        ReceiverMode::Starting
                    };
                    tracing::info!(talkgroup_id, reason, "target call ended");
                    let ended_at = Utc::now();
                    if let Some(archive) = &archive {
                        let _ = archive.send(ArchiveEvent::CallEnded {
                            transmission_id: call.transmission_id.clone(),
                            ended_at,
                            reason: reason.to_owned(),
                        });
                    }
                    let _ = events.send(RadioEvent::CallEnded {
                        transmission_id: call.transmission_id,
                        talkgroup_id,
                        ended_at,
                        reason: reason.to_owned(),
                    });
                    important = true;
                }
            }
            EngineEvent::DecodeError { role, message } => {
                snapshot.crc_errors += 1;
                tracing::debug!(?role, %message, "P25 decode error");
            }
            EngineEvent::Fatal { role, message } => {
                update_device(&mut snapshot, role, |device| {
                    device.connected = false;
                });
                snapshot.mode = ReceiverMode::Degraded;
                snapshot.last_error = Some(message.clone());
                if role == DeviceRole::Voice
                    && let Some(call) = snapshot.active_call.take()
                {
                    let ended_at = Utc::now();
                    let reason = "voice_worker_failed".to_owned();
                    recently_ended_call =
                        Some((call.talkgroup_id, call.frequency_hz, Instant::now()));
                    active_priority = None;
                    if let Some(archive) = &archive {
                        let _ = archive.send(ArchiveEvent::CallEnded {
                            transmission_id: call.transmission_id.clone(),
                            ended_at,
                            reason: reason.clone(),
                        });
                    }
                    let _ = events.send(RadioEvent::CallEnded {
                        transmission_id: call.transmission_id,
                        talkgroup_id: call.talkgroup_id,
                        ended_at,
                        reason,
                    });
                }
                let _ = events.send(RadioEvent::Error { message });
                important = true;
            }
            EngineEvent::Shutdown => break,
        }

        snapshot.updated_at = Utc::now();
        if important || last_snapshot.elapsed() >= SNAPSHOT_INTERVAL {
            let _ = state.send(snapshot.clone());
            let _ = events.send(RadioEvent::Snapshot(snapshot.clone()));
            last_snapshot = Instant::now();
        }
    }

    if let Some(call) = snapshot.active_call.take() {
        let ended_at = Utc::now();
        let reason = "engine_stopped".to_owned();
        if let Some(archive) = &archive {
            let _ = archive.send(ArchiveEvent::CallEnded {
                transmission_id: call.transmission_id.clone(),
                ended_at,
                reason: reason.clone(),
            });
        }
        let _ = events.send(RadioEvent::CallEnded {
            transmission_id: call.transmission_id,
            talkgroup_id: call.talkgroup_id,
            ended_at,
            reason,
        });
    }
    snapshot.mode = ReceiverMode::Stopped;
    snapshot.updated_at = Utc::now();
    let _ = state.send(snapshot.clone());
    let _ = events.send(RadioEvent::Snapshot(snapshot));
}

fn update_device(
    snapshot: &mut ReceiverSnapshot,
    role: DeviceRole,
    update: impl FnOnce(&mut DeviceState),
) {
    if let Some(device) = snapshot
        .devices
        .iter_mut()
        .find(|device| device.role == role)
    {
        update(device);
    }
}

fn should_preempt(candidate_priority: u8, active_priority: u8) -> bool {
    candidate_priority > active_priority
}

fn should_suppress_update_only_regrant(
    grant: &Grant,
    recently_ended: Option<(u16, u32, Instant)>,
) -> bool {
    grant.source_unit.is_none()
        && recently_ended.is_some_and(|(talkgroup_id, frequency_hz, ended_at)| {
            talkgroup_id == grant.talkgroup_id
                && frequency_hz == grant.frequency_hz
                && ended_at.elapsed() < UPDATE_ONLY_REGRANT_SUPPRESSION
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ppm_correction_is_signed_and_bounded() {
        assert_eq!(corrected_frequency(100_000_000, 2), 100_000_200);
        assert_eq!(corrected_frequency(100_000_000, -2), 99_999_800);
    }

    #[test]
    fn only_higher_priority_calls_preempt() {
        assert!(should_preempt(100, 90));
        assert!(!should_preempt(90, 90));
        assert!(!should_preempt(80, 90));
    }

    #[test]
    fn suppresses_only_recent_source_less_regrants() {
        let update = Grant {
            talkgroup_id: 44455,
            traffic_talkgroup_id: 44455,
            frequency_hz: 851_875_000,
            source_unit: None,
            encrypted: false,
        };
        let fresh_grant = Grant {
            source_unit: Some(3_200_001),
            ..update.clone()
        };
        let recent = Some((44455, 851_875_000, Instant::now()));

        assert!(should_suppress_update_only_regrant(&update, recent));
        assert!(!should_suppress_update_only_regrant(&fresh_grant, recent));
        assert!(!should_suppress_update_only_regrant(
            &update,
            Some((44458, 851_875_000, Instant::now()))
        ));
    }

    #[test]
    fn configured_target_wins_patch_and_keeps_its_metadata() {
        let configured_target = TalkgroupConfig {
            id: 44_455,
            name: "City Police Dispatch".to_owned(),
            description: None,
            service: ServiceKind::Police,
            priority: 100,
            enabled: true,
        };
        let configured = HashMap::from([(configured_target.id, configured_target)]);
        let listed = configured.keys().copied().collect();
        let grants = vec![
            Grant {
                talkgroup_id: 50_213,
                traffic_talkgroup_id: 50_213,
                frequency_hz: 851_875_000,
                source_unit: Some(3_200_001),
                encrypted: false,
            },
            Grant {
                talkgroup_id: 44_455,
                traffic_talkgroup_id: 50_213,
                frequency_hz: 851_875_000,
                source_unit: Some(3_200_001),
                encrypted: false,
            },
        ];

        let (grant, target) = select_grant_target(grants, &configured, &listed, true, 0).unwrap();
        assert_eq!(grant.talkgroup_id, 44_455);
        assert_eq!(grant.traffic_talkgroup_id, 50_213);
        assert_eq!(target.name, "City Police Dispatch");
        assert_eq!(target.service, ServiceKind::Police);
        assert_eq!(target.priority, 100);
    }

    #[test]
    fn unlisted_clear_group_gets_neutral_dynamic_metadata() {
        let grants = vec![Grant {
            talkgroup_id: 44_999,
            traffic_talkgroup_id: 44_999,
            frequency_hz: 851_875_000,
            source_unit: Some(3_200_001),
            encrypted: false,
        }];

        let (grant, target) =
            select_grant_target(grants, &HashMap::new(), &HashSet::new(), true, 2).unwrap();
        assert_eq!(grant.talkgroup_id, 44_999);
        assert_eq!(target.id, 44_999);
        assert_eq!(target.name, "TGID 44999");
        assert_eq!(target.service, ServiceKind::Other);
        assert_eq!(target.priority, 2);
    }

    #[test]
    fn encrypted_unlisted_grants_are_never_selected() {
        let grants = vec![Grant {
            talkgroup_id: 44_999,
            traffic_talkgroup_id: 44_999,
            frequency_hz: 851_875_000,
            source_unit: Some(3_200_001),
            encrypted: true,
        }];

        assert!(select_grant_target(grants, &HashMap::new(), &HashSet::new(), true, 0).is_none());
    }

    #[test]
    fn disabled_listed_group_is_not_reenabled_by_wildcard() {
        let grants = vec![Grant {
            talkgroup_id: 44_999,
            traffic_talkgroup_id: 44_999,
            frequency_hz: 851_875_000,
            source_unit: Some(3_200_001),
            encrypted: false,
        }];
        let listed = HashSet::from([44_999]);

        assert!(select_grant_target(grants, &HashMap::new(), &listed, true, 0).is_none());
    }
}
