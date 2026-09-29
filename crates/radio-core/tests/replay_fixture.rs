//! End-to-end regression test: run a real RTL-SDR control-channel capture
//! through the production DSP chain and P25 decoder without any hardware.

use radio_core::replay_cu8;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/control-channel.sigmf-data"
);

#[test]
fn decodes_trunking_control_channel_from_sigmf_fixture() {
    let summary = replay_cu8(FIXTURE).expect("fixture replays");

    // Five seconds of 240 kS/s CU8 IQ.
    assert_eq!(summary.input_samples, 1_200_000);
    assert!((summary.radio_seconds - 5.0).abs() < 1e-6);

    // The site NAC (0x1DB) must dominate the decoded network identifiers.
    let nac_hits = summary.nac_counts.get("0x1DB").copied().unwrap_or(0);
    assert!(
        nac_hits > 0,
        "no frames decoded for NAC 0x1DB: {summary:#?}"
    );

    // A control channel carries a steady stream of trunking signalling blocks.
    // The capture holds roughly 190 CRC-valid TSBKs; leave headroom so a
    // change in sync sensitivity does not flake the test.
    assert!(
        summary.valid_trunking_blocks >= 150,
        "too few CRC-valid TSBKs: {summary:#?}"
    );
    assert!(
        summary.duid_counts.contains_key("TrunkingSignaling"),
        "no trunking signalling data units: {summary:#?}"
    );

    // Site identity decoded from RFSS and network status broadcasts.
    assert!(summary.rfss_site_counts.get("2:23").copied().unwrap_or(0) > 0);
    assert!(summary.system_id_counts.contains_key("0x1D9"));
    assert!(summary.wacn_counts.contains_key("0xBEE00"));

    // Voice grants resolve to real frequencies through channel-parameter
    // updates, and Motorola patch groups are tracked.
    for opcode in ["GroupVoiceGrant", "GroupVoiceUpdate", "ChannelParamsUpdate"] {
        assert!(
            summary.trunking_opcodes.contains_key(opcode),
            "missing {opcode}: {summary:#?}"
        );
    }
    assert!(!summary.talkgroup_grants.is_empty());
    assert!(
        summary
            .talkgroup_grants
            .values()
            .any(|grant| !grant.frequencies_hz.is_empty()),
        "no grant resolved to a frequency: {summary:#?}"
    );
    assert!(!summary.motorola_patch_groups.is_empty());
}
