#![forbid(unsafe_code)]

use std::{path::PathBuf, sync::Arc};

mod archive;

use anyhow::Result;
use axum::{
    Json, Router,
    body::Body,
    extract::{
        Path as AxumPath, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{StatusCode, header},
    response::Response,
    routing::get,
};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use radio_core::{AppConfig, RadioEvent, RadioHandle, ReceiverMode, ServiceKind, TalkgroupConfig};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tower_http::{
    services::{ServeDir, ServeFile},
    trace::TraceLayer,
};

use crate::archive::{
    ArchiveRecord, ArchiveRecordStatus, ArchiveService, TranscriptSegment, TranscriptionStatus,
};

#[derive(Debug, Parser)]
#[command(
    name = "trunkline",
    version,
    about = "Pure-Rust P25 trunked radio receiver"
)]
struct Cli {
    #[arg(
        long,
        env = "TRUNKLINE_CONFIG",
        default_value = "config/trunkline.toml"
    )]
    config: PathBuf,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start the receiver and browser server.
    Serve,
    /// List RTL-SDR devices visible to the process.
    Probe,
    /// Record a reproducible raw-IQ SigMF capture.
    Capture {
        #[arg(long, default_value_t = 0)]
        device: usize,
        #[arg(long)]
        frequency: u32,
        #[arg(long, default_value_t = 10)]
        seconds: u64,
        #[arg(long)]
        gain: Option<f32>,
        #[arg(long, default_value_t = 0)]
        ppm: i32,
        #[arg(long)]
        output: PathBuf,
    },
    /// Replay a CU8 SigMF data file through the P25 decoder.
    Replay {
        #[arg(long)]
        input: PathBuf,
    },
}

#[derive(Clone)]
struct AppState {
    radio: RadioHandle,
    archive: ArchiveService,
    config: Arc<ConfigView>,
}

/// The subset of the configuration that is safe and useful to publish to
/// browsers and API clients. Paths, bind addresses, and tuner settings stay
/// server-side.
#[derive(Debug, Clone, Serialize)]
struct ConfigView {
    version: &'static str,
    site: SiteView,
    monitor_unlisted: bool,
    unlisted_priority: u8,
    talkgroups: Vec<TalkgroupView>,
    archive_enabled: bool,
    transcription_enabled: bool,
    transcription_model: Option<String>,
    transcription_language: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SiteView {
    system: String,
    name: String,
    rfss: u8,
    site: u8,
    nac: u16,
    nac_hex: String,
    modulation: radio_core::config::Modulation,
    control_frequencies_hz: Vec<u32>,
}

#[derive(Debug, Clone, Serialize)]
struct TalkgroupView {
    id: u16,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    service: &'static str,
    priority: u8,
    enabled: bool,
}

impl ConfigView {
    fn from_config(config: &AppConfig) -> Self {
        let mut talkgroups: Vec<TalkgroupView> = config
            .talkgroups
            .iter()
            .map(TalkgroupView::from_config)
            .collect();
        // Highest priority first, then by id, so clients can render the
        // scan list without re-sorting.
        talkgroups.sort_by(|left, right| {
            right
                .enabled
                .cmp(&left.enabled)
                .then_with(|| right.priority.cmp(&left.priority))
                .then_with(|| left.id.cmp(&right.id))
        });

        Self {
            version: env!("CARGO_PKG_VERSION"),
            site: SiteView {
                system: config.site.system.clone(),
                name: config.site.name.clone(),
                rfss: config.site.rfss,
                site: config.site.site,
                nac: config.site.nac,
                nac_hex: format!("0x{:03X}", config.site.nac),
                modulation: config.site.modulation,
                control_frequencies_hz: config.site.control_frequencies_hz.clone(),
            },
            monitor_unlisted: config.radio.monitor_unlisted,
            unlisted_priority: config.radio.unlisted_priority,
            talkgroups,
            archive_enabled: config.archive.enabled,
            transcription_enabled: config.archive.enabled && config.transcription.enabled,
            transcription_model: (config.archive.enabled && config.transcription.enabled)
                .then(|| config.transcription.model_name.clone()),
            transcription_language: (config.archive.enabled && config.transcription.enabled)
                .then(|| config.transcription.language.clone()),
        }
    }
}

impl TalkgroupView {
    fn from_config(talkgroup: &TalkgroupConfig) -> Self {
        Self {
            id: talkgroup.id,
            name: talkgroup.name.clone(),
            description: talkgroup.description.clone(),
            service: service_name(talkgroup.service),
            priority: talkgroup.priority,
            enabled: talkgroup.enabled,
        }
    }
}

fn service_name(service: ServiceKind) -> &'static str {
    match service {
        ServiceKind::Police => "police",
        ServiceKind::Fire => "fire",
        ServiceKind::Ems => "ems",
        ServiceKind::Other => "other",
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "trunkline_server=info,radio_core=info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Serve) {
        Command::Probe => {
            let devices = radio_core::DeviceInventory::discover()
                .map_err(|error| anyhow::anyhow!("RTL-SDR discovery failed: {error}"))?;
            println!("{}", serde_json::to_string_pretty(&devices)?);
        }
        Command::Capture {
            device,
            frequency,
            seconds,
            gain,
            ppm,
            output,
        } => {
            let result = radio_core::capture_sigmf(&radio_core::CaptureOptions {
                device_index: device,
                frequency_hz: frequency,
                sample_rate_hz: radio_core::dsp::SDR_SAMPLE_RATE_HZ,
                seconds,
                gain_db: gain,
                ppm,
                output_base: output,
            })?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::Replay { input } => {
            let summary = radio_core::replay_cu8(input)?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
        Command::Serve => {
            let config = AppConfig::load(&cli.config)?;
            let bind = config.server.bind;
            let web_root = PathBuf::from(&config.server.web_root);
            let archive_config = config.archive.clone();
            let transcription_config = config.transcription.clone();
            let config_view = Arc::new(ConfigView::from_config(&config));
            let (archive_sender, archive_receiver) = tokio::sync::mpsc::unbounded_channel();
            let archive =
                ArchiveService::start(archive_config, transcription_config, archive_receiver)?;
            let radio = radio_core::RadioEngine::start_with_archive(config, archive_sender)?;
            let app = router(radio.clone(), archive.clone(), config_view, web_root);
            let listener = tokio::net::TcpListener::bind(bind).await?;

            tracing::info!(
                address = %bind,
                "Trunkline browser receiver listening"
            );
            let result = axum::serve(listener, app)
                .with_graceful_shutdown(shutdown_signal(radio.clone()))
                .await;
            radio.shutdown().await;
            archive.shutdown().await?;
            result?;
        }
    }

    Ok(())
}

fn router(
    radio: RadioHandle,
    archive: ArchiveService,
    config: Arc<ConfigView>,
    web_root: PathBuf,
) -> Router {
    let index = web_root.join("index.html");
    let static_files = ServeDir::new(web_root).not_found_service(ServeFile::new(index));

    Router::new()
        .route("/api/health", get(health))
        .route("/api/config", get(config_view))
        .route("/api/state", get(state))
        .route("/api/transmissions", get(transmissions))
        .route("/api/transmissions/{id}", get(transmission))
        .route("/api/transmissions/{id}/audio", get(transmission_audio))
        .route("/api/events", get(events_upgrade))
        .route("/api/audio", get(audio_upgrade))
        .fallback_service(static_files)
        .layer(TraceLayer::new_for_http())
        .with_state(AppState {
            radio,
            archive,
            config,
        })
}

async fn config_view(State(app): State<AppState>) -> Json<ConfigView> {
    Json((*app.config).clone())
}

async fn health(State(app): State<AppState>) -> Json<Value> {
    let snapshot = app.radio.snapshot();
    let connected_devices = snapshot
        .devices
        .iter()
        .filter(|device| device.connected)
        .count();
    let healthy = snapshot.mode != ReceiverMode::Degraded
        && snapshot.mode != ReceiverMode::Stopped
        && connected_devices == snapshot.devices.len();
    let archive = app.archive.status();

    Json(json!({
        "status": if healthy { "ok" } else { "degraded" },
        "mode": snapshot.mode,
        "control_locked": snapshot.control_locked,
        "connected_devices": connected_devices,
        "required_devices": snapshot.devices.len(),
        "last_error": snapshot.last_error,
        "updated_at": snapshot.updated_at,
        "archive": archive,
    }))
}

async fn state(State(app): State<AppState>) -> Json<radio_core::ReceiverSnapshot> {
    Json(app.radio.snapshot())
}

#[derive(Debug, Deserialize)]
struct TransmissionQuery {
    limit: Option<usize>,
    tgid: Option<u16>,
    has_audio: Option<bool>,
}

#[derive(Debug, Serialize)]
struct TransmissionList {
    items: Vec<TransmissionView>,
    total: usize,
    model: Option<String>,
    model_status: String,
}

#[derive(Debug, Serialize)]
struct TransmissionView {
    id: String,
    schema_version: u32,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    duration_ms: u64,
    talkgroup_id: u16,
    traffic_talkgroup_id: u16,
    talkgroup_name: String,
    service: &'static str,
    frequency_hz: u32,
    source_unit: Option<u32>,
    encrypted: bool,
    recording_status: &'static str,
    end_reason: Option<String>,
    recording_error: Option<String>,
    audio_url: Option<String>,
    audio_samples: u64,
    audio_frames: u64,
    audio_duration_ms: u64,
    audio_sample_rate_hz: u32,
    audio_channels: u16,
    audio_bits_per_sample: u16,
    first_sequence: Option<u64>,
    last_sequence: Option<u64>,
    sequence_gaps: u64,
    transcription_status: &'static str,
    transcription_model: Option<String>,
    transcription_language: String,
    transcription_started_at: Option<DateTime<Utc>>,
    transcription_completed_at: Option<DateTime<Utc>>,
    transcription_duration_ms: Option<u64>,
    transcript: String,
    transcript_segments: Vec<TranscriptSegment>,
    transcription_skip_reason: Option<String>,
    transcription_error: Option<String>,
}

async fn transmissions(
    State(app): State<AppState>,
    Query(query): Query<TransmissionQuery>,
) -> Json<TransmissionList> {
    let limit = query.limit.unwrap_or(20).clamp(1, 200);
    let status = app.archive.status();
    let (records, total) =
        app.archive
            .list_filtered(limit, query.tgid, query.has_audio.unwrap_or(false));
    let model_status = if !status.transcription_enabled {
        "disabled"
    } else if status.worker_error.is_some() {
        "failed"
    } else if status.transcription_model_loaded {
        "ready"
    } else {
        "loading"
    };
    let items = records
        .into_iter()
        .map(|record| transmission_view(&app.archive, record))
        .collect();

    Json(TransmissionList {
        items,
        total,
        model: status.transcription_model,
        model_status: model_status.to_owned(),
    })
}

async fn transmission(
    State(app): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<TransmissionView>, StatusCode> {
    let record = app.archive.get(&id).ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(transmission_view(&app.archive, record)))
}

async fn transmission_audio(
    State(app): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Response, StatusCode> {
    let path = app.archive.audio_path(&id).ok_or(StatusCode::NOT_FOUND)?;
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let mut response = Response::new(Body::from(bytes));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("audio/wav"),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_static("inline"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("private, no-store"),
    );
    Ok(response)
}

fn transmission_view(archive: &ArchiveService, record: ArchiveRecord) -> TransmissionView {
    let id = record.transmission_id.clone();
    let audio_url = archive
        .has_playable_audio(&record)
        .then(|| format!("/api/transmissions/{id}/audio"));
    let wall_duration_ms = record
        .ended_at
        .and_then(|ended| {
            ended
                .signed_duration_since(record.started_at)
                .num_milliseconds()
                .try_into()
                .ok()
        })
        .unwrap_or(record.audio.duration_ms)
        .max(record.audio.duration_ms);
    let transcription_duration_ms = record.transcription.processing_ms.or(
        match (
            record.transcription.started_at,
            record.transcription.completed_at,
        ) {
            (Some(started), Some(completed)) => completed
                .signed_duration_since(started)
                .num_milliseconds()
                .try_into()
                .ok(),
            _ => None,
        },
    );
    let recording_status = match record.status {
        ArchiveRecordStatus::Recording => "recording",
        ArchiveRecordStatus::Complete if record.audio.samples == 0 => "empty",
        ArchiveRecordStatus::Complete => "complete",
        ArchiveRecordStatus::Failed => "failed",
        ArchiveRecordStatus::Interrupted => "interrupted",
    };
    let transcription_status = match record.transcription.status {
        TranscriptionStatus::Disabled => "disabled",
        TranscriptionStatus::Recording | TranscriptionStatus::Queued => "queued",
        TranscriptionStatus::Running => "processing",
        TranscriptionStatus::Complete => "complete",
        TranscriptionStatus::Skipped => "skipped_short_audio",
        TranscriptionStatus::Failed => "failed",
    };
    let service = service_name(record.call.service);

    TransmissionView {
        id,
        schema_version: record.schema_version,
        started_at: record.started_at,
        ended_at: record.ended_at,
        created_at: record.created_at,
        updated_at: record.updated_at,
        duration_ms: wall_duration_ms,
        talkgroup_id: record.call.talkgroup_id,
        traffic_talkgroup_id: record.call.traffic_talkgroup_id,
        talkgroup_name: record.call.talkgroup_name,
        service,
        frequency_hz: record.call.frequency_hz,
        source_unit: record.call.source_unit,
        encrypted: record.call.encrypted,
        recording_status,
        end_reason: record.end_reason,
        recording_error: record.audio.error,
        audio_url,
        audio_samples: record.audio.samples,
        audio_frames: record.audio.samples / 160,
        audio_duration_ms: record.audio.duration_ms,
        audio_sample_rate_hz: record.audio.sample_rate_hz,
        audio_channels: record.audio.channels,
        audio_bits_per_sample: record.audio.bits_per_sample,
        first_sequence: record.audio.first_sequence,
        last_sequence: record.audio.last_sequence,
        sequence_gaps: record.audio.sequence_gaps,
        transcription_status,
        transcription_model: record.transcription.model,
        transcription_language: record.transcription.language,
        transcription_started_at: record.transcription.started_at,
        transcription_completed_at: record.transcription.completed_at,
        transcription_duration_ms,
        transcript: record.transcription.text,
        transcript_segments: record.transcription.segments,
        transcription_skip_reason: record.transcription.skip_reason,
        transcription_error: record.transcription.error,
    }
}

async fn events_upgrade(ws: WebSocketUpgrade, State(app): State<AppState>) -> Response {
    let initial = app.radio.snapshot();
    let receiver = app.radio.subscribe_events();
    let state = app.radio.subscribe_state();
    ws.on_upgrade(move |socket| events_socket(socket, receiver, state, initial))
}

async fn events_socket(
    mut socket: WebSocket,
    mut receiver: broadcast::Receiver<RadioEvent>,
    mut state: tokio::sync::watch::Receiver<radio_core::ReceiverSnapshot>,
    initial: radio_core::ReceiverSnapshot,
) {
    let initially_stopped = initial.mode == ReceiverMode::Stopped;
    if !send_json(&mut socket, &RadioEvent::Snapshot(initial)).await {
        return;
    }
    if initially_stopped {
        let _ = socket.send(Message::Close(None)).await;
        return;
    }

    loop {
        tokio::select! {
            event = receiver.recv() => {
                match event {
                    Ok(event) => {
                        let stopped = matches!(
                            &event,
                            RadioEvent::Snapshot(snapshot) if snapshot.mode == ReceiverMode::Stopped
                        );
                        if !send_json(&mut socket, &event).await {
                            return;
                        }
                        if stopped {
                            let _ = socket.send(Message::Close(None)).await;
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
            changed = state.changed() => {
                if changed.is_err() {
                    return;
                }
                let snapshot = state.borrow().clone();
                if snapshot.mode == ReceiverMode::Stopped {
                    let _ = send_json(&mut socket, &RadioEvent::Snapshot(snapshot)).await;
                    let _ = socket.send(Message::Close(None)).await;
                    return;
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                    _ => {}
                }
            }
        }
    }
}

async fn send_json(socket: &mut WebSocket, event: &RadioEvent) -> bool {
    let Ok(payload) = serde_json::to_string(event) else {
        return true;
    };
    socket.send(Message::Text(payload.into())).await.is_ok()
}

async fn audio_upgrade(ws: WebSocketUpgrade, State(app): State<AppState>) -> Response {
    let receiver = app.radio.subscribe_audio();
    let state = app.radio.subscribe_state();
    ws.on_upgrade(move |socket| audio_socket(socket, receiver, state))
}

async fn audio_socket(
    mut socket: WebSocket,
    mut receiver: broadcast::Receiver<radio_core::AudioFrame>,
    mut state: tokio::sync::watch::Receiver<radio_core::ReceiverSnapshot>,
) {
    if state.borrow().mode == ReceiverMode::Stopped {
        let _ = socket.send(Message::Close(None)).await;
        return;
    }

    loop {
        tokio::select! {
            frame = receiver.recv() => {
                match frame {
                    Ok(frame) => {
                        let payload = encode_audio_frame(&frame);
                        if socket.send(Message::Binary(payload.into())).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::debug!(skipped, "slow browser audio client dropped frames");
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
            changed = state.changed() => {
                if changed.is_err() || state.borrow().mode == ReceiverMode::Stopped {
                    let _ = socket.send(Message::Close(None)).await;
                    return;
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                    _ => {}
                }
            }
        }
    }
}

fn encode_audio_frame(frame: &radio_core::AudioFrame) -> Vec<u8> {
    const SAMPLE_RATE_HZ: u16 = 8_000;
    const FLAGS_CLEAR_AUDIO: u32 = 1;

    let mut payload = Vec::with_capacity(16 + frame.samples.len() * 2);
    payload.extend_from_slice(&frame.sequence.to_le_bytes());
    payload.extend_from_slice(&frame.talkgroup_id.to_le_bytes());
    payload.extend_from_slice(&SAMPLE_RATE_HZ.to_le_bytes());
    payload.extend_from_slice(&FLAGS_CLEAR_AUDIO.to_le_bytes());
    for sample in frame.samples {
        payload.extend_from_slice(&sample.to_le_bytes());
    }
    payload
}

async fn shutdown_signal(radio: RadioHandle) {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::warn!(%error, "failed to install Ctrl-C signal handler");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    {
        let terminate = async {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut signal) => {
                    signal.recv().await;
                }
                Err(error) => {
                    tracing::warn!(%error, "failed to install SIGTERM signal handler");
                    std::future::pending::<()>().await;
                }
            }
        };

        tokio::select! {
            _ = ctrl_c => tracing::info!("Ctrl-C received, shutting down"),
            _ = terminate => tracing::info!("SIGTERM received, shutting down"),
        }
    }

    #[cfg(not(unix))]
    ctrl_c.await;

    radio.shutdown().await;
    tracing::info!("radio receiver stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_view_orders_talkgroups_by_priority_and_hides_paths() {
        let config: AppConfig = toml::from_str(
            r#"
                [server]
                bind = "127.0.0.1:8097"
                web_root = "/app/web"

                [site]
                system = "Example 800"
                name = "Example Simulcast"
                rfss = 1
                site = 1
                nac = 475
                modulation = "cqpsk"
                control_frequencies_hz = [853862500]

                [radio]
                control_device = 0
                voice_device = 1
                monitor_unlisted = true

                [transcription]
                model_path = "/models/secret.bin"

                [[talkgroups]]
                id = 200
                name = "Low"
                service = "other"
                priority = 1

                [[talkgroups]]
                id = 100
                name = "High"
                description = "Primary dispatch"
                service = "police"
                priority = 50

                [[talkgroups]]
                id = 300
                name = "Off"
                service = "fire"
                priority = 99
                enabled = false
            "#,
        )
        .unwrap();

        let view = ConfigView::from_config(&config);
        let ids: Vec<u16> = view
            .talkgroups
            .iter()
            .map(|talkgroup| talkgroup.id)
            .collect();
        assert_eq!(ids, vec![100, 200, 300]);
        assert_eq!(
            view.talkgroups[0].description.as_deref(),
            Some("Primary dispatch")
        );
        assert_eq!(view.site.nac_hex, "0x1DB");
        assert!(view.monitor_unlisted);

        let json = serde_json::to_string(&view).unwrap();
        assert!(!json.contains("secret.bin"));
        assert!(!json.contains("web_root"));
        assert!(!json.contains("127.0.0.1"));
    }

    #[test]
    fn audio_wire_frame_is_little_endian_and_336_bytes() {
        let mut samples = [0_i16; 160];
        samples[0] = -1234;
        let frame = radio_core::AudioFrame {
            sequence: 42,
            talkgroup_id: 44455,
            samples,
        };

        let payload = encode_audio_frame(&frame);
        assert_eq!(payload.len(), 336);
        assert_eq!(u64::from_le_bytes(payload[0..8].try_into().unwrap()), 42);
        assert_eq!(
            u16::from_le_bytes(payload[8..10].try_into().unwrap()),
            44455
        );
        assert_eq!(
            u16::from_le_bytes(payload[10..12].try_into().unwrap()),
            8_000
        );
        assert_eq!(
            i16::from_le_bytes(payload[16..18].try_into().unwrap()),
            -1234
        );
    }
}
