use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    hash::{Hash, Hasher},
    io::{BufWriter, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Instant,
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Datelike, Utc};
use radio_core::{ActiveCall, ArchiveConfig, ArchiveEvent, TranscriptionConfig};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedReceiver;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

const ARCHIVE_SCHEMA_VERSION: u32 = 1;
const AUDIO_SAMPLE_RATE_HZ: u32 = 8_000;
const AUDIO_CHANNELS: u16 = 1;
const AUDIO_BITS_PER_SAMPLE: u16 = 16;
const WHISPER_TIMESTAMP_MS: u64 = 10;

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveRecordStatus {
    Recording,
    Complete,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptionStatus {
    Disabled,
    Recording,
    Queued,
    Running,
    Complete,
    Skipped,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranscriptSegment {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchivedAudio {
    /// Path relative to the configured archive root.
    pub path: String,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub samples: u64,
    pub duration_ms: u64,
    pub first_sequence: Option<u64>,
    pub last_sequence: Option<u64>,
    pub sequence_gaps: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptionRecord {
    pub status: TranscriptionStatus,
    pub model: Option<String>,
    pub language: String,
    pub audio_duration_ms: u64,
    #[serde(default)]
    pub processing_ms: Option<u64>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub segments: Vec<TranscriptSegment>,
    pub text: String,
    pub skip_reason: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveRecord {
    pub schema_version: u32,
    pub transmission_id: String,
    pub call: ActiveCall,
    pub status: ArchiveRecordStatus,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub end_reason: Option<String>,
    pub audio: ArchivedAudio,
    pub transcription: TranscriptionRecord,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArchiveStatus {
    pub enabled: bool,
    pub directory: String,
    pub records: usize,
    pub active_recordings: usize,
    pub transcription_enabled: bool,
    pub transcription_model: Option<String>,
    pub transcription_model_loaded: bool,
    pub transcription_queued: usize,
    pub transcription_running: bool,
    pub transcription_complete: usize,
    pub transcription_skipped: usize,
    pub transcription_failed: usize,
    pub worker_error: Option<String>,
}

#[derive(Clone)]
pub struct ArchiveService {
    inner: Arc<Inner>,
}

struct Inner {
    enabled: bool,
    root: PathBuf,
    transcription_enabled: bool,
    transcription_model: Option<String>,
    records: RwLock<HashMap<String, ArchiveRecord>>,
    persist_lock: Mutex<()>,
    handles: Mutex<Option<WorkerHandles>>,
    transcription_model_loaded: AtomicBool,
    transcription_queued: AtomicUsize,
    transcription_running: AtomicBool,
    worker_error: RwLock<Option<String>>,
}

struct WorkerHandles {
    archive: thread::JoinHandle<()>,
    transcription: Option<thread::JoinHandle<()>>,
}

struct OpenRecording {
    id: String,
    writer: Option<Pcm16WavWriter>,
    samples_for_transcription: Option<Vec<i16>>,
    sample_count: u64,
    first_sequence: Option<u64>,
    last_sequence: Option<u64>,
    sequence_gaps: u64,
    audio_error: Option<String>,
}

struct TranscriptionJob {
    transmission_id: String,
    samples_8khz: Vec<i16>,
}

impl ArchiveService {
    pub fn start(
        config: ArchiveConfig,
        transcription: TranscriptionConfig,
        receiver: UnboundedReceiver<ArchiveEvent>,
    ) -> Result<Self> {
        if config.enabled && config.directory.trim().is_empty() {
            bail!("archive directory cannot be empty when archiving is enabled");
        }
        if config.enabled && transcription.enabled && transcription.model_path.trim().is_empty() {
            bail!("transcription model_path cannot be empty when transcription is enabled");
        }

        let root = PathBuf::from(&config.directory);
        if config.enabled {
            fs::create_dir_all(&root)
                .with_context(|| format!("failed to create archive root {}", root.display()))?;
        }

        let (records, recovered_ids) = load_records(&root)?;
        let inner = Arc::new(Inner {
            enabled: config.enabled,
            root,
            transcription_enabled: config.enabled && transcription.enabled,
            transcription_model: (config.enabled && transcription.enabled)
                .then(|| transcription.model_name.clone()),
            records: RwLock::new(records),
            persist_lock: Mutex::new(()),
            handles: Mutex::new(None),
            transcription_model_loaded: AtomicBool::new(false),
            transcription_queued: AtomicUsize::new(0),
            transcription_running: AtomicBool::new(false),
            worker_error: RwLock::new(None),
        });

        for id in recovered_ids {
            if let Err(error) = inner.persist_record(&id) {
                tracing::warn!(
                    transmission_id = %id,
                    error = %error,
                    "failed to persist recovered archive record"
                );
            }
        }

        let (transcription_sender, transcription_handle) = if inner.transcription_enabled {
            let (sender, jobs) = mpsc::channel();
            let worker_inner = Arc::clone(&inner);
            let worker_config = transcription.clone();
            let handle = thread::Builder::new()
                .name("trunkline-asr".to_owned())
                .spawn(move || transcription_worker(worker_inner, worker_config, jobs))
                .context("failed to spawn transcription worker")?;
            (Some(sender), Some(handle))
        } else {
            (None, None)
        };

        let archive_inner = Arc::clone(&inner);
        let archive_config = transcription;
        let archive_handle = match thread::Builder::new()
            .name("trunkline-archive".to_owned())
            .spawn(move || {
                archive_worker(
                    archive_inner,
                    archive_config,
                    receiver,
                    transcription_sender,
                )
            }) {
            Ok(handle) => handle,
            Err(error) => {
                drop(transcription_handle);
                return Err(error).context("failed to spawn archive worker");
            }
        };

        *lock_mutex(&inner.handles) = Some(WorkerHandles {
            archive: archive_handle,
            transcription: transcription_handle,
        });

        Ok(Self { inner })
    }

    /// Return newest-first archive records and the total number matching the
    /// filters before `limit` is applied.
    ///
    /// When `has_audio` is true, only finalized records with at least one
    /// sample and an existing WAV file are included. This matches the
    /// condition used by the HTTP API to expose an `audio_url`.
    pub fn list_filtered(
        &self,
        limit: usize,
        tgid: Option<u16>,
        has_audio: bool,
    ) -> (Vec<ArchiveRecord>, usize) {
        let records = read_lock(&self.inner.records);
        let mut result: Vec<_> = records
            .values()
            .filter(|record| {
                tgid.is_none_or(|talkgroup_id| record.call.talkgroup_id == talkgroup_id)
            })
            .filter(|record| !has_audio || has_playable_audio_at(&self.inner.root, record))
            .cloned()
            .collect();
        let total = result.len();
        result.sort_by(|left, right| {
            right
                .started_at
                .cmp(&left.started_at)
                .then_with(|| right.transmission_id.cmp(&left.transmission_id))
        });
        result.truncate(limit);
        (result, total)
    }

    pub fn get(&self, transmission_id: &str) -> Option<ArchiveRecord> {
        read_lock(&self.inner.records).get(transmission_id).cloned()
    }

    pub fn audio_path(&self, transmission_id: &str) -> Option<PathBuf> {
        let relative = {
            let records = read_lock(&self.inner.records);
            PathBuf::from(records.get(transmission_id)?.audio.path.clone())
        };
        if !is_safe_relative_path(&relative) {
            tracing::warn!(
                transmission_id,
                path = %relative.display(),
                "refusing unsafe archived audio path"
            );
            return None;
        }

        let path = self.inner.root.join(relative);
        path.is_file().then_some(path)
    }

    pub fn has_playable_audio(&self, record: &ArchiveRecord) -> bool {
        has_playable_audio_at(&self.inner.root, record)
    }

    pub fn status(&self) -> ArchiveStatus {
        let records = read_lock(&self.inner.records);
        let mut active_recordings = 0;
        let mut transcription_complete = 0;
        let mut transcription_skipped = 0;
        let mut transcription_failed = 0;

        for record in records.values() {
            if record.status == ArchiveRecordStatus::Recording {
                active_recordings += 1;
            }
            match record.transcription.status {
                TranscriptionStatus::Complete => transcription_complete += 1,
                TranscriptionStatus::Skipped => transcription_skipped += 1,
                TranscriptionStatus::Failed => transcription_failed += 1,
                _ => {}
            }
        }

        ArchiveStatus {
            enabled: self.inner.enabled,
            directory: self.inner.root.display().to_string(),
            records: records.len(),
            active_recordings,
            transcription_enabled: self.inner.transcription_enabled,
            transcription_model: self.inner.transcription_model.clone(),
            transcription_model_loaded: self
                .inner
                .transcription_model_loaded
                .load(Ordering::Acquire),
            transcription_queued: self.inner.transcription_queued.load(Ordering::Acquire),
            transcription_running: self.inner.transcription_running.load(Ordering::Acquire),
            transcription_complete,
            transcription_skipped,
            transcription_failed,
            worker_error: read_lock(&self.inner.worker_error).clone(),
        }
    }

    /// Wait for the archive event channel to close, finalize all WAV files,
    /// drain queued transcription jobs, and stop both worker threads.
    ///
    /// The radio engine must drop its final `ArchiveEvent` sender before this
    /// method is awaited.
    pub async fn shutdown(&self) -> Result<()> {
        let Some(handles) = lock_mutex(&self.inner.handles).take() else {
            return Ok(());
        };

        tokio::task::spawn_blocking(move || {
            handles
                .archive
                .join()
                .map_err(|panic| thread_panic_error("archive", panic))?;
            if let Some(handle) = handles.transcription {
                handle
                    .join()
                    .map_err(|panic| thread_panic_error("transcription", panic))?;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("archive shutdown join task failed")?
    }
}

impl Inner {
    fn update_record(
        &self,
        transmission_id: &str,
        update: impl FnOnce(&mut ArchiveRecord),
    ) -> bool {
        let mut records = write_lock(&self.records);
        let Some(record) = records.get_mut(transmission_id) else {
            return false;
        };
        update(record);
        record.updated_at = Utc::now();
        true
    }

    fn persist_record(&self, transmission_id: &str) -> Result<()> {
        let _persist_guard = lock_mutex(&self.persist_lock);
        // Re-read after taking the serialization lock. That prevents a stale
        // transcription snapshot from overwriting a newer recorder snapshot.
        let record = read_lock(&self.records)
            .get(transmission_id)
            .cloned()
            .ok_or_else(|| anyhow!("archive record {transmission_id} disappeared"))?;
        let path = sidecar_path(&self.root, &record)?;
        atomic_write_json(&path, &record)
    }

    fn set_worker_error(&self, error: impl Into<String>) {
        *write_lock(&self.worker_error) = Some(error.into());
    }
}

fn archive_worker(
    inner: Arc<Inner>,
    transcription: TranscriptionConfig,
    mut receiver: UnboundedReceiver<ArchiveEvent>,
    transcription_sender: Option<mpsc::Sender<TranscriptionJob>>,
) {
    if !inner.enabled {
        while receiver.blocking_recv().is_some() {}
        return;
    }

    let mut recordings = HashMap::<String, OpenRecording>::new();
    while let Some(event) = receiver.blocking_recv() {
        match event {
            ArchiveEvent::CallStarted(call) => {
                if let Err(error) = start_recording(&inner, &transcription, call, &mut recordings) {
                    inner.set_worker_error(error.to_string());
                    tracing::error!(error = %error, "failed to start call archive");
                }
            }
            ArchiveEvent::CallUpdated(call) => {
                let transmission_id = call.transmission_id.clone();
                if inner.update_record(&transmission_id, |record| record.call = call)
                    && let Err(error) = inner.persist_record(&transmission_id)
                {
                    tracing::warn!(
                        transmission_id,
                        error = %error,
                        "failed to persist updated call metadata"
                    );
                }
            }
            ArchiveEvent::Audio {
                transmission_id,
                sequence,
                samples,
            } => {
                append_audio(
                    &inner,
                    &mut recordings,
                    &transmission_id,
                    sequence,
                    &samples,
                );
            }
            ArchiveEvent::CallEnded {
                transmission_id,
                ended_at,
                reason,
            } => {
                if let Some(recording) = recordings.remove(&transmission_id) {
                    finish_recording(
                        &inner,
                        &transcription,
                        recording,
                        ended_at,
                        reason,
                        transcription_sender.as_ref(),
                    );
                } else {
                    tracing::warn!(
                        transmission_id,
                        "received archive end event without an active recording"
                    );
                }
            }
        }
    }

    let interrupted_at = Utc::now();
    for (_, recording) in recordings {
        finish_recording(
            &inner,
            &transcription,
            recording,
            interrupted_at,
            "archive_event_channel_closed".to_owned(),
            transcription_sender.as_ref(),
        );
    }
    // Dropping the only sender closes the ASR queue after all final jobs have
    // been enqueued, allowing the transcription thread to drain and exit.
    drop(transcription_sender);
}

fn start_recording(
    inner: &Arc<Inner>,
    transcription: &TranscriptionConfig,
    call: ActiveCall,
    recordings: &mut HashMap<String, OpenRecording>,
) -> Result<()> {
    let transmission_id = call.transmission_id.clone();
    if let Some(record) = recordings.get(&transmission_id) {
        tracing::debug!(
            transmission_id = %record.id,
            "duplicate call-start event treated as a metadata update"
        );
        inner.update_record(&transmission_id, |archive| archive.call = call);
        inner.persist_record(&transmission_id)?;
        return Ok(());
    }
    if read_lock(&inner.records).contains_key(&transmission_id) {
        bail!("duplicate archived transmission id {transmission_id}");
    }

    let relative_wav = relative_wav_path(&call);
    let absolute_wav = inner.root.join(&relative_wav);
    let parent = absolute_wav
        .parent()
        .ok_or_else(|| anyhow!("archive WAV has no parent directory"))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;

    let (writer, audio_error) = match Pcm16WavWriter::create(&absolute_wav) {
        Ok(writer) => (Some(writer), None),
        Err(error) => {
            let message = format!(
                "failed to create archive WAV {}: {error}",
                absolute_wav.display()
            );
            tracing::error!(transmission_id, error = %message);
            (None, Some(message))
        }
    };

    let now = Utc::now();
    let started_at = call.started_at;
    let transcription_record = if inner.transcription_enabled {
        TranscriptionRecord {
            status: TranscriptionStatus::Recording,
            model: Some(transcription.model_name.clone()),
            language: transcription.language.clone(),
            audio_duration_ms: 0,
            processing_ms: None,
            started_at: None,
            completed_at: None,
            segments: Vec::new(),
            text: String::new(),
            skip_reason: None,
            error: None,
        }
    } else {
        TranscriptionRecord {
            status: TranscriptionStatus::Disabled,
            model: None,
            language: transcription.language.clone(),
            audio_duration_ms: 0,
            processing_ms: None,
            started_at: None,
            completed_at: None,
            segments: Vec::new(),
            text: String::new(),
            skip_reason: None,
            error: None,
        }
    };

    let record = ArchiveRecord {
        schema_version: ARCHIVE_SCHEMA_VERSION,
        transmission_id: transmission_id.clone(),
        call,
        status: ArchiveRecordStatus::Recording,
        started_at,
        ended_at: None,
        end_reason: None,
        audio: ArchivedAudio {
            path: relative_path_string(&relative_wav),
            sample_rate_hz: AUDIO_SAMPLE_RATE_HZ,
            channels: AUDIO_CHANNELS,
            bits_per_sample: AUDIO_BITS_PER_SAMPLE,
            samples: 0,
            duration_ms: 0,
            first_sequence: None,
            last_sequence: None,
            sequence_gaps: 0,
            error: audio_error.clone(),
        },
        transcription: transcription_record,
        created_at: now,
        updated_at: now,
    };

    recordings.insert(
        transmission_id.clone(),
        OpenRecording {
            id: transmission_id.clone(),
            writer,
            samples_for_transcription: inner.transcription_enabled.then(Vec::new),
            sample_count: 0,
            first_sequence: None,
            last_sequence: None,
            sequence_gaps: 0,
            audio_error,
        },
    );
    write_lock(&inner.records).insert(transmission_id.clone(), record);
    inner.persist_record(&transmission_id)
}

fn append_audio(
    inner: &Inner,
    recordings: &mut HashMap<String, OpenRecording>,
    transmission_id: &str,
    sequence: u64,
    samples: &[i16; 160],
) {
    let Some(recording) = recordings.get_mut(transmission_id) else {
        tracing::warn!(
            transmission_id,
            sequence,
            "received archive audio without an active recording"
        );
        return;
    };

    if recording
        .last_sequence
        .is_some_and(|last_sequence| sequence <= last_sequence)
    {
        tracing::debug!(
            transmission_id,
            sequence,
            last_sequence = ?recording.last_sequence,
            "ignored duplicate or out-of-order archive audio frame"
        );
        return;
    }

    if let Some(last_sequence) = recording.last_sequence
        && sequence > last_sequence.saturating_add(1)
    {
        recording.sequence_gaps = recording
            .sequence_gaps
            .saturating_add(sequence - last_sequence - 1);
    }
    recording.first_sequence.get_or_insert(sequence);
    recording.last_sequence = Some(sequence);

    if let Some(writer) = recording.writer.as_mut()
        && let Err(error) = writer.write_samples(samples)
    {
        let message = format!("failed to write archive PCM: {error}");
        tracing::error!(transmission_id, error = %message);
        recording.audio_error = Some(message);
        recording.writer = None;
    }
    if let Some(buffer) = recording.samples_for_transcription.as_mut() {
        buffer.extend_from_slice(samples);
    }
    recording.sample_count = recording.sample_count.saturating_add(samples.len() as u64);
    let duration_ms = samples_to_duration_ms(recording.sample_count);
    let audio_error = recording.audio_error.clone();
    inner.update_record(transmission_id, |record| {
        record.audio.samples = recording.sample_count;
        record.audio.duration_ms = duration_ms;
        record.audio.first_sequence = recording.first_sequence;
        record.audio.last_sequence = recording.last_sequence;
        record.audio.sequence_gaps = recording.sequence_gaps;
        record.audio.error = audio_error;
        record.transcription.audio_duration_ms = duration_ms;
    });
}

fn finish_recording(
    inner: &Arc<Inner>,
    transcription: &TranscriptionConfig,
    mut recording: OpenRecording,
    ended_at: DateTime<Utc>,
    reason: String,
    transcription_sender: Option<&mpsc::Sender<TranscriptionJob>>,
) {
    if let Some(writer) = recording.writer.take()
        && let Err(error) = writer.finalize()
    {
        let message = format!("failed to finalize archive WAV: {error}");
        tracing::error!(
            transmission_id = %recording.id,
            error = %message
        );
        recording.audio_error = Some(message);
    }

    let duration_ms = samples_to_duration_ms(recording.sample_count);
    let audio_error = recording.audio_error.clone();
    let eligible = inner.transcription_enabled && duration_ms >= transcription.minimum_audio_ms;
    inner.update_record(&recording.id, |record| {
        record.status = if audio_error.is_some() {
            ArchiveRecordStatus::Failed
        } else {
            ArchiveRecordStatus::Complete
        };
        record.ended_at = Some(ended_at);
        record.end_reason = Some(reason);
        record.audio.samples = recording.sample_count;
        record.audio.duration_ms = duration_ms;
        record.audio.first_sequence = recording.first_sequence;
        record.audio.last_sequence = recording.last_sequence;
        record.audio.sequence_gaps = recording.sequence_gaps;
        record.audio.error = audio_error;
        record.transcription.audio_duration_ms = duration_ms;
        if inner.transcription_enabled {
            if eligible {
                record.transcription.status = TranscriptionStatus::Queued;
            } else {
                record.transcription.status = TranscriptionStatus::Skipped;
                record.transcription.completed_at = Some(Utc::now());
                record.transcription.skip_reason = Some(format!(
                    "audio duration {duration_ms} ms is below minimum {} ms",
                    transcription.minimum_audio_ms
                ));
            }
        }
    });

    if eligible {
        let job = TranscriptionJob {
            transmission_id: recording.id.clone(),
            samples_8khz: recording
                .samples_for_transcription
                .take()
                .unwrap_or_default(),
        };
        inner.transcription_queued.fetch_add(1, Ordering::AcqRel);
        let sent = transcription_sender.is_some_and(|sender| sender.send(job).is_ok());
        if !sent {
            inner
                .transcription_queued
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                    Some(queued.saturating_sub(1))
                })
                .ok();
            inner.update_record(&recording.id, |record| {
                record.transcription.status = TranscriptionStatus::Failed;
                record.transcription.completed_at = Some(Utc::now());
                record.transcription.error = Some("transcription worker is unavailable".to_owned());
            });
        }
    }

    if let Err(error) = inner.persist_record(&recording.id) {
        inner.set_worker_error(error.to_string());
        tracing::error!(
            transmission_id = %recording.id,
            error = %error,
            "failed to persist completed archive record"
        );
    }
}

fn transcription_worker(
    inner: Arc<Inner>,
    config: TranscriptionConfig,
    jobs: mpsc::Receiver<TranscriptionJob>,
) {
    // The context owns the model and remains on this dedicated OS thread for
    // the entire service lifetime.
    let mut context_parameters = WhisperContextParameters::default();
    context_parameters.use_gpu(false);
    let context = WhisperContext::new_with_params(&config.model_path, context_parameters);

    match context {
        Ok(context) => {
            inner
                .transcription_model_loaded
                .store(true, Ordering::Release);
            for job in jobs {
                inner
                    .transcription_queued
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                        Some(queued.saturating_sub(1))
                    })
                    .ok();
                inner.transcription_running.store(true, Ordering::Release);
                transcribe_job(&inner, &config, &context, job);
                inner.transcription_running.store(false, Ordering::Release);
            }
        }
        Err(error) => {
            let message = format!(
                "failed to load Whisper model {}: {error}",
                config.model_path
            );
            inner.set_worker_error(message.clone());
            tracing::error!(error = %message);
            for job in jobs {
                inner
                    .transcription_queued
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                        Some(queued.saturating_sub(1))
                    })
                    .ok();
                fail_transcription(&inner, &job.transmission_id, message.clone(), None);
            }
        }
    }
}

fn transcribe_job(
    inner: &Inner,
    config: &TranscriptionConfig,
    context: &WhisperContext,
    job: TranscriptionJob,
) {
    let started_at = Utc::now();
    inner.update_record(&job.transmission_id, |record| {
        record.transcription.status = TranscriptionStatus::Running;
        record.transcription.started_at = Some(started_at);
        record.transcription.completed_at = None;
        record.transcription.processing_ms = None;
        record.transcription.error = None;
    });
    if let Err(error) = inner.persist_record(&job.transmission_id) {
        tracing::warn!(
            transmission_id = %job.transmission_id,
            error = %error,
            "failed to persist running transcription status"
        );
    }

    let processing_started = Instant::now();
    match run_whisper(context, config, &job.samples_8khz) {
        Ok(segments) => {
            let processing_ms = processing_started.elapsed().as_millis() as u64;
            let text = segments
                .iter()
                .map(|segment| segment.text.trim())
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            inner.update_record(&job.transmission_id, |record| {
                record.transcription.status = TranscriptionStatus::Complete;
                record.transcription.completed_at = Some(Utc::now());
                record.transcription.processing_ms = Some(processing_ms);
                record.transcription.segments = segments;
                record.transcription.text = text;
                record.transcription.error = None;
            });
            if let Err(error) = inner.persist_record(&job.transmission_id) {
                inner.set_worker_error(error.to_string());
                tracing::error!(
                    transmission_id = %job.transmission_id,
                    error = %error,
                    "failed to persist completed transcription"
                );
            }
        }
        Err(error) => {
            let processing_ms = processing_started.elapsed().as_millis() as u64;
            fail_transcription(
                inner,
                &job.transmission_id,
                format!("Whisper inference failed: {error:#}"),
                Some(processing_ms),
            );
        }
    }
}

fn fail_transcription(
    inner: &Inner,
    transmission_id: &str,
    error: String,
    processing_ms: Option<u64>,
) {
    inner.update_record(transmission_id, |record| {
        record.transcription.status = TranscriptionStatus::Failed;
        record.transcription.completed_at = Some(Utc::now());
        record.transcription.processing_ms = processing_ms;
        record.transcription.error = Some(error.clone());
    });
    if let Err(persist_error) = inner.persist_record(transmission_id) {
        inner.set_worker_error(persist_error.to_string());
        tracing::error!(
            transmission_id,
            error = %persist_error,
            "failed to persist transcription failure"
        );
    }
}

fn run_whisper(
    context: &WhisperContext,
    config: &TranscriptionConfig,
    samples_8khz: &[i16],
) -> Result<Vec<TranscriptSegment>> {
    let samples_16khz = interpolate_8khz_to_16khz(samples_8khz);
    let mut state = context
        .create_state()
        .context("failed to create Whisper state")?;
    let mut params = FullParams::new(SamplingStrategy::BeamSearch {
        beam_size: 5,
        patience: -1.0,
    });
    params.set_language(Some(&config.language));
    params.set_translate(false);
    params.set_no_context(true);
    params.set_n_threads(config.threads.clamp(1, i32::MAX as usize) as i32);
    params.set_temperature(0.0);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    let prompt = config.initial_prompt.replace('\0', " ");
    if !prompt.trim().is_empty() {
        params.set_initial_prompt(&prompt);
    }

    state
        .full(params, &samples_16khz)
        .context("Whisper decoding failed")?;
    let mut segments = Vec::with_capacity(state.full_n_segments().max(0) as usize);
    for segment in state.as_iter() {
        let start = segment.start_timestamp();
        let end = segment.end_timestamp();
        segments.push(TranscriptSegment {
            start_ms: u64::try_from(start)
                .unwrap_or_default()
                .saturating_mul(WHISPER_TIMESTAMP_MS),
            end_ms: u64::try_from(end)
                .unwrap_or_default()
                .saturating_mul(WHISPER_TIMESTAMP_MS),
            text: segment.to_string().trim().to_owned(),
        });
    }
    Ok(segments)
}

/// Deterministic two-times linear interpolation. Even output samples preserve
/// the source values; odd samples are the integer midpoint to the next source
/// value. The final value is held for the last half-sample.
fn interpolate_8khz_to_16khz(samples: &[i16]) -> Vec<f32> {
    let mut output = Vec::with_capacity(samples.len().saturating_mul(2));
    for (index, &sample) in samples.iter().enumerate() {
        let next = samples.get(index + 1).copied().unwrap_or(sample);
        let midpoint = (i32::from(sample) + i32::from(next)) / 2;
        output.push(f32::from(sample) / 32_768.0);
        output.push(midpoint as f32 / 32_768.0);
    }
    output
}

fn load_records(root: &Path) -> Result<(HashMap<String, ArchiveRecord>, Vec<String>)> {
    if !root.is_dir() {
        return Ok((HashMap::new(), Vec::new()));
    }

    let mut sidecars = Vec::new();
    collect_json_files(root, &mut sidecars)?;
    let mut records = HashMap::<String, ArchiveRecord>::new();
    let mut recovered = Vec::new();
    for path in sidecars {
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "failed to read archive sidecar"
                );
                continue;
            }
        };
        let mut record: ArchiveRecord = match serde_json::from_slice(&bytes) {
            Ok(record) => record,
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "ignored invalid archive sidecar"
                );
                continue;
            }
        };
        if record.schema_version != ARCHIVE_SCHEMA_VERSION {
            tracing::warn!(
                path = %path.display(),
                schema_version = record.schema_version,
                "ignored unsupported archive sidecar schema"
            );
            continue;
        }
        if !is_safe_relative_path(Path::new(&record.audio.path)) {
            tracing::warn!(
                path = %path.display(),
                audio_path = %record.audio.path,
                "ignored archive sidecar containing unsafe audio path"
            );
            continue;
        }

        let mut was_recovered = false;
        if record.status == ArchiveRecordStatus::Recording {
            record.status = ArchiveRecordStatus::Interrupted;
            record.ended_at.get_or_insert(record.updated_at);
            record.end_reason = Some("archive interrupted by previous process exit".to_owned());
            was_recovered = true;
        }
        if matches!(
            record.transcription.status,
            TranscriptionStatus::Recording
                | TranscriptionStatus::Queued
                | TranscriptionStatus::Running
        ) {
            record.transcription.status = TranscriptionStatus::Failed;
            record.transcription.completed_at = Some(Utc::now());
            record.transcription.error =
                Some("transcription interrupted by previous process exit".to_owned());
            was_recovered = true;
        }
        if was_recovered {
            record.updated_at = Utc::now();
            recovered.push(record.transmission_id.clone());
        }

        let replace = records
            .get(&record.transmission_id)
            .is_none_or(|existing| record.updated_at > existing.updated_at);
        if replace {
            records.insert(record.transmission_id.clone(), record);
        }
    }
    Ok((records, recovered))
}

fn collect_json_files(directory: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(directory)
        .with_context(|| format!("failed to list {}", directory.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read entry below {}", directory.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if file_type.is_dir() {
            collect_json_files(&path, output)?;
        } else if file_type.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension == "json")
        {
            output.push(path);
        }
    }
    Ok(())
}

fn relative_wav_path(call: &ActiveCall) -> PathBuf {
    let stem = safe_file_stem(&call.transmission_id);
    PathBuf::from(format!(
        "{:04}/{:02}/{:02}/{stem}.wav",
        call.started_at.year(),
        call.started_at.month(),
        call.started_at.day()
    ))
}

fn sidecar_path(root: &Path, record: &ArchiveRecord) -> Result<PathBuf> {
    let relative_wav = Path::new(&record.audio.path);
    if !is_safe_relative_path(relative_wav) {
        bail!(
            "refusing unsafe audio path in archive record: {}",
            relative_wav.display()
        );
    }
    Ok(root.join(relative_wav).with_extension("json"))
}

fn safe_file_stem(transmission_id: &str) -> String {
    let mut readable: String = transmission_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .take(80)
        .collect();
    if readable.is_empty() {
        readable.push_str("transmission");
    }

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    transmission_id.hash(&mut hasher);
    format!("{readable}-{:016x}", hasher.finish())
}

fn is_safe_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn has_playable_audio_at(root: &Path, record: &ArchiveRecord) -> bool {
    if record.status == ArchiveRecordStatus::Recording || record.audio.samples == 0 {
        return false;
    }

    let relative = Path::new(&record.audio.path);
    is_safe_relative_path(relative) && root.join(relative).is_file()
}

fn relative_path_string(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn samples_to_duration_ms(samples: u64) -> u64 {
    samples.saturating_mul(1_000) / u64::from(AUDIO_SAMPLE_RATE_HZ)
}

fn atomic_write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("JSON sidecar has no parent directory"))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("JSON sidecar has no filename"))?
        .to_string_lossy();
    let temporary = parent.join(format!(".{name}.tmp-{}-{counter}", std::process::id()));

    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .with_context(|| {
                format!("failed to create temporary sidecar {}", temporary.display())
            })?;
        serde_json::to_writer_pretty(&mut file, value)
            .context("failed to serialize archive sidecar")?;
        file.write_all(b"\n")
            .context("failed to terminate archive sidecar")?;
        file.sync_all()
            .with_context(|| format!("failed to sync {}", temporary.display()))?;
        fs::rename(&temporary, path).with_context(|| {
            format!(
                "failed to atomically replace {} with {}",
                path.display(),
                temporary.display()
            )
        })?;
        // Best effort: not every supported filesystem permits syncing a
        // directory handle, while the file and rename remain valid without it.
        if let Ok(directory) = File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

struct Pcm16WavWriter {
    writer: BufWriter<File>,
    data_bytes: u64,
}

impl Pcm16WavWriter {
    fn create(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        let mut writer = BufWriter::new(file);
        write_wav_header(&mut writer, 0)?;
        Ok(Self {
            writer,
            data_bytes: 0,
        })
    }

    fn write_samples(&mut self, samples: &[i16]) -> Result<()> {
        let additional = (samples.len() as u64)
            .checked_mul(2)
            .ok_or_else(|| anyhow!("WAV sample count overflow"))?;
        let new_size = self
            .data_bytes
            .checked_add(additional)
            .ok_or_else(|| anyhow!("WAV data size overflow"))?;
        if new_size > u64::from(u32::MAX) {
            bail!("WAV exceeds RIFF 32-bit data limit");
        }
        for sample in samples {
            self.writer
                .write_all(&sample.to_le_bytes())
                .context("failed to write PCM sample")?;
        }
        self.data_bytes = new_size;
        Ok(())
    }

    fn finalize(mut self) -> Result<()> {
        let data_bytes = u32::try_from(self.data_bytes).context("WAV data exceeds RIFF limit")?;
        self.writer.flush().context("failed to flush archive WAV")?;
        self.writer
            .seek(SeekFrom::Start(0))
            .context("failed to seek archive WAV header")?;
        write_wav_header(&mut self.writer, data_bytes)?;
        self.writer
            .flush()
            .context("failed to flush finalized archive WAV")?;
        self.writer
            .get_ref()
            .sync_all()
            .context("failed to sync finalized archive WAV")
    }
}

fn write_wav_header(writer: &mut (impl Write + ?Sized), data_bytes: u32) -> Result<()> {
    let byte_rate =
        AUDIO_SAMPLE_RATE_HZ * u32::from(AUDIO_CHANNELS) * u32::from(AUDIO_BITS_PER_SAMPLE / 8);
    let block_align = AUDIO_CHANNELS * (AUDIO_BITS_PER_SAMPLE / 8);
    let riff_size = 36_u32
        .checked_add(data_bytes)
        .ok_or_else(|| anyhow!("RIFF size overflow"))?;

    writer.write_all(b"RIFF")?;
    writer.write_all(&riff_size.to_le_bytes())?;
    writer.write_all(b"WAVE")?;
    writer.write_all(b"fmt ")?;
    writer.write_all(&16_u32.to_le_bytes())?;
    writer.write_all(&1_u16.to_le_bytes())?;
    writer.write_all(&AUDIO_CHANNELS.to_le_bytes())?;
    writer.write_all(&AUDIO_SAMPLE_RATE_HZ.to_le_bytes())?;
    writer.write_all(&byte_rate.to_le_bytes())?;
    writer.write_all(&block_align.to_le_bytes())?;
    writer.write_all(&AUDIO_BITS_PER_SAMPLE.to_le_bytes())?;
    writer.write_all(b"data")?;
    writer.write_all(&data_bytes.to_le_bytes())?;
    Ok(())
}

fn thread_panic_error(
    worker: &str,
    panic: Box<dyn std::any::Any + Send + 'static>,
) -> anyhow::Error {
    if let Some(message) = panic.downcast_ref::<&str>() {
        anyhow!("{worker} worker panicked: {message}")
    } else if let Some(message) = panic.downcast_ref::<String>() {
        anyhow!("{worker} worker panicked: {message}")
    } else {
        anyhow!("{worker} worker panicked")
    }
}

fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_mutex<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::{fs, time::Duration};

    use chrono::TimeZone;
    use radio_core::{ServiceKind, TranscriptionConfig};
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn interpolation_is_deterministic_and_holds_the_final_sample() {
        let converted = interpolate_8khz_to_16khz(&[-32_768, 0, 32_767]);
        assert_eq!(converted.len(), 6);
        assert_eq!(converted[0], -1.0);
        assert_eq!(converted[1], -0.5);
        assert_eq!(converted[2], 0.0);
        assert_eq!(converted[3], 16_383.0 / 32_768.0);
        assert_eq!(converted[4], 32_767.0 / 32_768.0);
        assert_eq!(converted[5], 32_767.0 / 32_768.0);
    }

    #[test]
    fn unsafe_transmission_ids_cannot_escape_archive_root() {
        let stem = safe_file_stem("../../incident/one");
        assert!(!stem.contains('/'));
        assert!(!stem.contains(".."));
        assert!(is_safe_relative_path(Path::new("2026/07/29/call.wav")));
        assert!(!is_safe_relative_path(Path::new("../call.wav")));
        assert!(!is_safe_relative_path(Path::new("/tmp/call.wav")));
    }

    #[tokio::test]
    async fn records_short_and_empty_calls_and_loads_them_on_restart() {
        let temporary = tempdir().unwrap();
        let archive = ArchiveConfig {
            enabled: true,
            directory: temporary.path().display().to_string(),
        };
        let transcription = disabled_transcription();
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let service =
            ArchiveService::start(archive.clone(), transcription.clone(), receiver).unwrap();

        let first = call("call-one", 44_455, 1);
        sender.send(ArchiveEvent::CallStarted(first)).unwrap();
        sender
            .send(ArchiveEvent::Audio {
                transmission_id: "call-one".to_owned(),
                sequence: 7,
                samples: [123; 160],
            })
            .unwrap();
        sender
            .send(ArchiveEvent::CallEnded {
                transmission_id: "call-one".to_owned(),
                ended_at: Utc.with_ymd_and_hms(2026, 7, 29, 12, 0, 1).unwrap(),
                reason: "test".to_owned(),
            })
            .unwrap();

        let second = call("call-empty", 44_458, 2);
        sender.send(ArchiveEvent::CallStarted(second)).unwrap();
        sender
            .send(ArchiveEvent::CallEnded {
                transmission_id: "call-empty".to_owned(),
                ended_at: Utc.with_ymd_and_hms(2026, 7, 29, 12, 0, 2).unwrap(),
                reason: "test".to_owned(),
            })
            .unwrap();
        drop(sender);
        service.shutdown().await.unwrap();

        let first_record = service.get("call-one").unwrap();
        assert_eq!(first_record.audio.samples, 160);
        assert_eq!(first_record.audio.duration_ms, 20);
        assert_eq!(
            first_record.transcription.status,
            TranscriptionStatus::Disabled
        );
        assert_wav(service.audio_path("call-one").unwrap(), 320);

        let empty_record = service.get("call-empty").unwrap();
        assert_eq!(empty_record.audio.samples, 0);
        assert_wav(service.audio_path("call-empty").unwrap(), 0);

        let json_path = service
            .audio_path("call-one")
            .unwrap()
            .with_extension("json");
        let disk_record: ArchiveRecord =
            serde_json::from_slice(&fs::read(json_path).unwrap()).unwrap();
        assert_eq!(disk_record.transmission_id, "call-one");

        let (restart_sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let restarted = ArchiveService::start(archive, transcription, receiver).unwrap();
        assert_eq!(restarted.list_filtered(10, None, false).0.len(), 2);
        assert_eq!(restarted.list_filtered(10, Some(44_455), false).0.len(), 1);
        let (with_audio, total) = restarted.list_filtered(1, None, true);
        assert_eq!(total, 1);
        assert_eq!(with_audio.len(), 1);
        assert_eq!(with_audio[0].transmission_id, "call-one");
        let (matching_tgid, total) = restarted.list_filtered(10, Some(44_455), true);
        assert_eq!(total, 1);
        assert_eq!(matching_tgid[0].transmission_id, "call-one");
        let (empty_tgid, total) = restarted.list_filtered(10, Some(44_458), true);
        assert_eq!(total, 0);
        assert!(empty_tgid.is_empty());
        drop(restart_sender);
        restarted.shutdown().await.unwrap();
    }

    fn disabled_transcription() -> TranscriptionConfig {
        TranscriptionConfig {
            enabled: false,
            model_path: String::new(),
            model_name: "disabled".to_owned(),
            language: "en".to_owned(),
            threads: 1,
            minimum_audio_ms: 750,
            initial_prompt: String::new(),
        }
    }

    fn call(id: &str, talkgroup_id: u16, second: u32) -> ActiveCall {
        ActiveCall {
            transmission_id: id.to_owned(),
            talkgroup_id,
            traffic_talkgroup_id: talkgroup_id,
            talkgroup_name: format!("Talkgroup {talkgroup_id}"),
            service: ServiceKind::Police,
            frequency_hz: 853_862_500,
            source_unit: Some(100),
            encrypted: false,
            started_at: Utc.with_ymd_and_hms(2026, 7, 29, 12, 0, second).unwrap(),
        }
    }

    fn assert_wav(path: PathBuf, expected_data_bytes: u32) {
        for _ in 0..100 {
            if path.is_file() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let bytes = fs::read(path).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(
            u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
            expected_data_bytes
        );
        assert_eq!(bytes.len(), 44 + expected_data_bytes as usize);
    }
}
