//! Reproducible SigMF IQ capture from an RTL-SDR.

use std::{
    fs::File,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct CaptureOptions {
    pub device_index: usize,
    pub frequency_hz: u32,
    pub sample_rate_hz: u32,
    pub seconds: u64,
    pub gain_db: Option<f32>,
    pub ppm: i32,
    pub output_base: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct CaptureResult {
    pub data_path: PathBuf,
    pub metadata_path: PathBuf,
    pub samples: u64,
    pub bytes: u64,
    pub elapsed_seconds: f64,
}

pub fn capture_sigmf(options: &CaptureOptions) -> Result<CaptureResult> {
    if options.seconds == 0 {
        bail!("capture duration must be at least one second");
    }
    if options.sample_rate_hz != crate::dsp::SDR_SAMPLE_RATE_HZ {
        bail!(
            "P25 captures currently require {} samples/second",
            crate::dsp::SDR_SAMPLE_RATE_HZ
        );
    }

    let (data_path, metadata_path) = sigmf_paths(&options.output_base);
    if let Some(parent) = data_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let nominal = options.frequency_hz as i64;
    let correction = nominal * options.ppm as i64 / 1_000_000;
    let tuned_frequency = (nominal + correction)
        .try_into()
        .context("PPM correction produced an invalid tuning frequency")?;

    let mut sdr = rs_rtl::RtlSdr::open(rs_rtl::DeviceId::Index(options.device_index))
        .with_context(|| format!("failed to open RTL-SDR {}", options.device_index))?;
    sdr.set_sample_rate(options.sample_rate_hz)?;
    let _ = sdr.set_bandwidth(options.sample_rate_hz)?;
    sdr.set_center_freq(tuned_frequency)?;
    match options.gain_db {
        Some(gain) => sdr.set_gain_manual((gain * 10.0).round() as i32)?,
        None => sdr.set_gain_auto()?,
    }

    let target_samples = options.sample_rate_hz as u64 * options.seconds;
    let target_bytes = target_samples * 2;
    let mut written_bytes = 0_u64;
    let started = Instant::now();
    let reader = sdr.start_streaming()?;
    let file = File::create(&data_path)
        .with_context(|| format!("failed to create {}", data_path.display()))?;
    let mut writer = BufWriter::with_capacity(1024 * 1024, file);

    while written_bytes < target_bytes {
        let bytes = reader
            .recv()
            .context("RTL-SDR stream ended before capture completed")?;
        let remaining = (target_bytes - written_bytes) as usize;
        let take = bytes.len().min(remaining);
        writer.write_all(&bytes[..take])?;
        written_bytes += take as u64;
    }
    writer.flush()?;

    let metadata = SigMfDocument {
        global: SigMfGlobal {
            datatype: "cu8",
            sample_rate: options.sample_rate_hz,
            version: "1.0.0",
            recorder: concat!("Trunkline ", env!("CARGO_PKG_VERSION")),
            description: "RTL-SDR IQ capture for the Trunkline P25 receiver",
            num_channels: 1,
        },
        captures: vec![SigMfCapture {
            sample_start: 0,
            frequency: options.frequency_hz,
            datetime: Utc::now().to_rfc3339(),
        }],
        annotations: Vec::new(),
        trunkline: CaptureProvenance {
            device_index: options.device_index,
            tuned_frequency_hz: tuned_frequency,
            gain_db: options.gain_db,
            ppm: options.ppm,
            samples: written_bytes / 2,
        },
    };
    let meta_file = File::create(&metadata_path)
        .with_context(|| format!("failed to create {}", metadata_path.display()))?;
    serde_json::to_writer_pretty(BufWriter::new(meta_file), &metadata)?;

    Ok(CaptureResult {
        data_path,
        metadata_path,
        samples: written_bytes / 2,
        bytes: written_bytes,
        elapsed_seconds: started.elapsed().as_secs_f64(),
    })
}

fn sigmf_paths(base: &Path) -> (PathBuf, PathBuf) {
    let name = base.to_string_lossy();
    if let Some(stem) = name.strip_suffix(".sigmf-data") {
        (
            PathBuf::from(name.as_ref()),
            PathBuf::from(format!("{stem}.sigmf-meta")),
        )
    } else if let Some(stem) = name.strip_suffix(".sigmf-meta") {
        (
            PathBuf::from(format!("{stem}.sigmf-data")),
            PathBuf::from(name.as_ref()),
        )
    } else {
        (
            PathBuf::from(format!("{name}.sigmf-data")),
            PathBuf::from(format!("{name}.sigmf-meta")),
        )
    }
}

#[derive(Serialize)]
struct SigMfDocument<'a> {
    global: SigMfGlobal<'a>,
    captures: Vec<SigMfCapture>,
    annotations: Vec<serde_json::Value>,
    #[serde(rename = "trunkline:provenance")]
    trunkline: CaptureProvenance,
}

#[derive(Serialize)]
struct SigMfGlobal<'a> {
    #[serde(rename = "core:datatype")]
    datatype: &'a str,
    #[serde(rename = "core:sample_rate")]
    sample_rate: u32,
    #[serde(rename = "core:version")]
    version: &'a str,
    #[serde(rename = "core:recorder")]
    recorder: &'a str,
    #[serde(rename = "core:description")]
    description: &'a str,
    #[serde(rename = "core:num_channels")]
    num_channels: u8,
}

#[derive(Serialize)]
struct SigMfCapture {
    #[serde(rename = "core:sample_start")]
    sample_start: u64,
    #[serde(rename = "core:frequency")]
    frequency: u32,
    #[serde(rename = "core:datetime")]
    datetime: String,
}

#[derive(Serialize)]
struct CaptureProvenance {
    device_index: usize,
    tuned_frequency_hz: u32,
    gain_db: Option<f32>,
    ppm: i32,
    samples: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_sigmf_pair_from_base_or_extension() {
        assert_eq!(
            sigmf_paths(Path::new("/tmp/control")),
            (
                PathBuf::from("/tmp/control.sigmf-data"),
                PathBuf::from("/tmp/control.sigmf-meta")
            )
        );
        assert_eq!(
            sigmf_paths(Path::new("/tmp/control.sigmf-data")).1,
            PathBuf::from("/tmp/control.sigmf-meta")
        );
    }
}
