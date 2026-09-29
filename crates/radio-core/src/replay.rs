//! Offline CU8 replay through the exact production DSP and P25 decoder.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufReader, Read},
    path::Path,
    time::Instant,
};

use anyhow::{Context, Result};
use p25::trunking::{
    fields::{
        Channel, ChannelParamsMap, ChannelParamsUpdate, GroupTrafficUpdate, NetworkStatusBroadcast,
        RfssStatusBroadcast, TalkGroup,
    },
    tsbk::{
        GroupVoiceGrant, MFID_MOTOROLA, MOTOROLA_PATCH_GROUP_ADD, MOTOROLA_PATCH_GROUP_DELETE,
        MOTOROLA_PATCH_GROUP_GRANT, MOTOROLA_PATCH_GROUP_UPDATE, MotorolaPatchDelete,
        MotorolaPatchGroup, TsbkOpcode,
    },
};
use serde::Serialize;

use crate::{
    decode::{P25Decoder, P25Event},
    dsp::{P25Channel, SDR_SAMPLE_RATE_HZ},
    patch::PatchRegistry,
};

#[derive(Debug, Clone, Serialize)]
pub struct ReplaySummary {
    pub input_samples: u64,
    pub radio_seconds: f64,
    pub processing_seconds: f64,
    pub average_power_dbfs: f32,
    pub peak_power_dbfs: f32,
    pub network_ids: u64,
    pub nac_counts: BTreeMap<String, u64>,
    pub duid_counts: BTreeMap<String, u64>,
    pub trunking_blocks: u64,
    pub valid_trunking_blocks: u64,
    pub trunking_opcodes: BTreeMap<String, u64>,
    pub trunking_samples: BTreeMap<String, Vec<String>>,
    pub wacn_counts: BTreeMap<String, u64>,
    pub system_id_counts: BTreeMap<String, u64>,
    pub rfss_site_counts: BTreeMap<String, u64>,
    pub motorola_patch_groups: BTreeMap<String, Vec<u16>>,
    pub talkgroup_grants: BTreeMap<String, TalkgroupGrantSummary>,
    pub voice_headers: u64,
    pub link_control_words: u64,
    pub audio_frames: u64,
    pub error_counts: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TalkgroupGrantSummary {
    pub count: u64,
    pub encrypted_count: u64,
    pub frequencies_hz: BTreeMap<String, u64>,
}

impl ReplaySummary {
    fn empty() -> Self {
        Self {
            input_samples: 0,
            radio_seconds: 0.0,
            processing_seconds: 0.0,
            average_power_dbfs: -120.0,
            peak_power_dbfs: -120.0,
            network_ids: 0,
            nac_counts: BTreeMap::new(),
            duid_counts: BTreeMap::new(),
            trunking_blocks: 0,
            valid_trunking_blocks: 0,
            trunking_opcodes: BTreeMap::new(),
            trunking_samples: BTreeMap::new(),
            wacn_counts: BTreeMap::new(),
            system_id_counts: BTreeMap::new(),
            rfss_site_counts: BTreeMap::new(),
            motorola_patch_groups: BTreeMap::new(),
            talkgroup_grants: BTreeMap::new(),
            voice_headers: 0,
            link_control_words: 0,
            audio_frames: 0,
            error_counts: BTreeMap::new(),
        }
    }
}

pub fn replay_cu8(path: impl AsRef<Path>) -> Result<ReplaySummary> {
    let path = path.as_ref();
    let file =
        File::open(path).with_context(|| format!("failed to open replay {}", path.display()))?;
    let mut reader = BufReader::with_capacity(1024 * 1024, file);
    let mut raw = vec![0_u8; 256 * 1024];
    let mut baseband = Vec::with_capacity(raw.len() / 10);
    let mut channel = P25Channel::new();
    let mut decoder = P25Decoder::new();
    let mut channel_params = ChannelParamsMap::default();
    let mut patches = PatchRegistry::default();
    let mut summary = ReplaySummary::empty();
    let mut linear_power_sum = 0.0_f64;
    let mut power_samples = 0_u64;
    let started = Instant::now();

    loop {
        let bytes = reader.read(&mut raw)?;
        if bytes == 0 {
            break;
        }
        let even_bytes = bytes & !1;
        let metrics = channel.process_cu8(&raw[..even_bytes], &mut baseband);
        summary.input_samples += metrics.input_samples as u64;
        summary.peak_power_dbfs = summary.peak_power_dbfs.max(metrics.power_dbfs);
        linear_power_sum +=
            10.0_f64.powf(metrics.power_dbfs as f64 / 10.0) * metrics.output_samples as f64;
        power_samples += metrics.output_samples as u64;

        decoder.feed(&baseband, |event| match event {
            P25Event::NetworkId(nid) => {
                summary.network_ids += 1;
                increment(
                    &mut summary.nac_counts,
                    format!("0x{:03X}", nid.access_code.to_bits()),
                );
                increment(&mut summary.duid_counts, format!("{:?}", nid.data_unit));
            }
            P25Event::Trunking(tsbk) => {
                summary.trunking_blocks += 1;
                if tsbk.crc_valid() {
                    summary.valid_trunking_blocks += 1;
                    let opcode = tsbk.opcode();
                    let opcode_name = opcode
                        .map(|opcode| format!("{opcode:?}"))
                        .unwrap_or_else(|| "Unknown".to_owned());
                    increment(&mut summary.trunking_opcodes, opcode_name.clone());
                    record_sample(
                        &mut summary.trunking_samples,
                        opcode_name,
                        tsbk.mfg(),
                        tsbk.payload(),
                    );

                    match tsbk.mfg() {
                        0 => match opcode {
                            Some(TsbkOpcode::ChannelParamsUpdate) => {
                                channel_params.update(&ChannelParamsUpdate::new(tsbk.payload()));
                            }
                            Some(
                                TsbkOpcode::GroupVoiceGrant | TsbkOpcode::GroupVoiceUpdateExplicit,
                            ) => {
                                let grant = GroupVoiceGrant::new(tsbk);
                                record_grant(
                                    &mut summary,
                                    grant.talkgroup(),
                                    grant.channel(),
                                    grant.opts().protected(),
                                    &channel_params,
                                );
                            }
                            Some(TsbkOpcode::GroupVoiceUpdate) => {
                                for (channel, talkgroup) in
                                    GroupTrafficUpdate::new(tsbk.payload()).updates()
                                {
                                    record_grant(
                                        &mut summary,
                                        talkgroup,
                                        channel,
                                        false,
                                        &channel_params,
                                    );
                                }
                            }
                            Some(TsbkOpcode::NetworkStatusBroadcast) => {
                                let status = NetworkStatusBroadcast::new(tsbk.payload());
                                increment(
                                    &mut summary.wacn_counts,
                                    format!("0x{:05X}", status.wacn()),
                                );
                                increment(
                                    &mut summary.system_id_counts,
                                    format!("0x{:03X}", status.system()),
                                );
                            }
                            Some(TsbkOpcode::RfssStatusBroadcast) => {
                                let status = RfssStatusBroadcast::new(tsbk.payload());
                                increment(
                                    &mut summary.rfss_site_counts,
                                    format!("{}:{}", status.rfss(), status.site()),
                                );
                                increment(
                                    &mut summary.system_id_counts,
                                    format!("0x{:03X}", status.system()),
                                );
                            }
                            _ => {}
                        },
                        MFID_MOTOROLA => match tsbk.opcode_bits() {
                            MOTOROLA_PATCH_GROUP_ADD => {
                                let patch = MotorolaPatchGroup::new(tsbk.payload());
                                let supergroup = patch.supergroup();
                                let members = patch.members();
                                patches.add(supergroup, members.iter().copied());
                                let recorded = summary
                                    .motorola_patch_groups
                                    .entry(supergroup.to_string())
                                    .or_default();
                                for member in members {
                                    if !recorded.contains(&member) {
                                        recorded.push(member);
                                    }
                                }
                                recorded.sort_unstable();
                            }
                            MOTOROLA_PATCH_GROUP_DELETE => {
                                patches
                                    .delete(MotorolaPatchDelete::new(tsbk.payload()).supergroup());
                            }
                            MOTOROLA_PATCH_GROUP_GRANT => {
                                let grant = GroupVoiceGrant::new(tsbk);
                                record_patch_grants(
                                    &mut summary,
                                    grant.talkgroup(),
                                    grant.channel(),
                                    grant.opts().protected(),
                                    &patches,
                                    &channel_params,
                                );
                            }
                            MOTOROLA_PATCH_GROUP_UPDATE => {
                                for (channel, supergroup) in
                                    GroupTrafficUpdate::new(tsbk.payload()).updates()
                                {
                                    record_patch_grants(
                                        &mut summary,
                                        supergroup,
                                        channel,
                                        false,
                                        &patches,
                                        &channel_params,
                                    );
                                }
                            }
                            _ => {}
                        },
                        _ => {}
                    }
                }
            }
            P25Event::VoiceHeader(_) => summary.voice_headers += 1,
            P25Event::LinkControl(_) | P25Event::VoiceTerm(_) => {
                summary.link_control_words += 1;
            }
            P25Event::Audio(_) => summary.audio_frames += 1,
            P25Event::Error(error) => {
                increment(&mut summary.error_counts, format!("{error:?}"));
            }
            P25Event::Crypto(_) => {}
        });
    }

    summary.radio_seconds = summary.input_samples as f64 / SDR_SAMPLE_RATE_HZ as f64;
    summary.processing_seconds = started.elapsed().as_secs_f64();
    if power_samples > 0 && linear_power_sum > 0.0 {
        summary.average_power_dbfs =
            (10.0 * (linear_power_sum / power_samples as f64).log10()) as f32;
    }
    Ok(summary)
}

fn increment(map: &mut BTreeMap<String, u64>, key: String) {
    *map.entry(key).or_default() += 1;
}

fn record_sample(
    samples: &mut BTreeMap<String, Vec<String>>,
    opcode: String,
    manufacturer: u8,
    payload: &[u8],
) {
    let examples = samples.entry(opcode).or_default();
    if examples.len() >= 3 {
        return;
    }
    let encoded = format!(
        "mfg={manufacturer:02X} payload={}",
        payload
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<String>()
    );
    if !examples.contains(&encoded) {
        examples.push(encoded);
    }
}

fn record_grant(
    summary: &mut ReplaySummary,
    talkgroup: TalkGroup,
    channel: Channel,
    encrypted: bool,
    channel_params: &ChannelParamsMap,
) {
    let TalkGroup::Other(talkgroup_id) = talkgroup else {
        return;
    };
    record_grant_id(summary, talkgroup_id, channel, encrypted, channel_params);
}

fn record_patch_grants(
    summary: &mut ReplaySummary,
    supergroup: TalkGroup,
    channel: Channel,
    encrypted: bool,
    patches: &PatchRegistry,
    channel_params: &ChannelParamsMap,
) {
    let TalkGroup::Other(supergroup_id) = supergroup else {
        return;
    };
    for talkgroup_id in patches.grant_targets(supergroup_id) {
        record_grant_id(summary, talkgroup_id, channel, encrypted, channel_params);
    }
}

fn record_grant_id(
    summary: &mut ReplaySummary,
    talkgroup_id: u16,
    channel: Channel,
    encrypted: bool,
    channel_params: &ChannelParamsMap,
) {
    let grant = summary
        .talkgroup_grants
        .entry(talkgroup_id.to_string())
        .or_default();
    grant.count += 1;
    grant.encrypted_count += u64::from(encrypted);
    if let Some(params) = channel_params.lookup(channel.id()) {
        increment(
            &mut grant.frequencies_hz,
            params.rx_freq(channel.number()).to_string(),
        );
    }
}
