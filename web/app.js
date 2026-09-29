(() => {
  "use strict";

  // Populated from GET /api/config. Keyed by talkgroup id.
  const configuredTalkgroups = new Map();
  let receiverConfig = null;

  const SERVICE_ICONS = {
    police:
      '<path d="M12 3l7 3v5c0 4.7-2.8 8.1-7 10-4.2-1.9-7-5.3-7-10V6l7-3z"></path>',
    fire:
      '<path d="M13 2s1 4-2 6c-2.5 1.7-4 4-3.5 7 .4 3.1 2.5 5 5.5 5 3.6 0 6-2.7 6-6.2 0-4.8-3.6-7.8-6-11.8z"></path><path d="M12.5 12c-1.5 1.1-2.4 2.3-2.1 4 .2 1.3 1.1 2.2 2.6 2.2 1.7 0 2.8-1.3 2.8-2.9 0-2-1.5-3.3-2.5-4.7 0 .6-.2 1-.8 1.4z"></path>',
    ems: '<path d="M9 4h6v5h5v6h-5v5H9v-5H4V9h5V4z"></path>',
    other:
      '<path d="M5 17a9 9 0 0 1 0-10M8.5 14.5a4.9 4.9 0 0 1 0-5M19 7a9 9 0 0 1 0 10M15.5 9.5a4.9 4.9 0 0 1 0 5"></path><circle cx="12" cy="12" r="1.8"></circle>',
  };

  const SERVICE_LABELS = {
    police: "Law enforcement",
    fire: "Fire",
    ems: "EMS",
    other: "Other",
  };

  const elements = {
    eventStatus: document.querySelector("#eventStatus"),
    reconnectButton: document.querySelector("#reconnectButton"),
    localTime: document.querySelector("#localTime"),
    callStatus: document.querySelector("#callStatus"),
    signalBars: document.querySelector("#signalBars"),
    activeServiceIcon: document.querySelector("#activeServiceIcon"),
    activeTalkgroup: document.querySelector("#activeTalkgroup"),
    activeTalkgroupMeta: document.querySelector("#activeTalkgroupMeta"),
    activeFrequency: document.querySelector("#activeFrequency"),
    activeSource: document.querySelector("#activeSource"),
    activeDuration: document.querySelector("#activeDuration"),
    listenButton: document.querySelector("#listenButton"),
    listenLabel: document.querySelector("#listenButton span"),
    muteButton: document.querySelector("#muteButton"),
    audioStatus: document.querySelector("#audioStatus"),
    player: document.querySelector(".player"),
    receiverMode: document.querySelector("#receiverMode"),
    systemName: document.querySelector("#systemName"),
    siteName: document.querySelector("#siteName"),
    controlFrequency: document.querySelector("#controlFrequency"),
    controlLock: document.querySelector("#controlLock"),
    receiverUptime: document.querySelector("#receiverUptime"),
    framesDecoded: document.querySelector("#framesDecoded"),
    crcErrors: document.querySelector("#crcErrors"),
    lastError: document.querySelector("#lastError"),
    talkgroupCards: [],
    talkgroupGrid: document.querySelector("#talkgroupGrid"),
    talkgroupCount: document.querySelector("#talkgroupCount"),
    scanDescription: document.querySelector("#scanDescription"),
    brandSubtitle: document.querySelector("#brandSubtitle"),
    siteEyebrow: document.querySelector("#siteEyebrow"),
    siteSummary: document.querySelector("#siteSummary"),
    footerVersion: document.querySelector("#footerVersion"),
    sitewideCoverage: document.querySelector("#sitewideCoverage"),
    sitewideCoverageLabel: document.querySelector("#sitewideCoverageLabel"),
    deviceList: document.querySelector("#deviceList"),
    activityList: document.querySelector("#activityList"),
    clearActivityButton: document.querySelector("#clearActivityButton"),
    lastUpdated: document.querySelector("#lastUpdated"),
    archiveModelBadge: document.querySelector("#archiveModelBadge"),
    archiveModelName: document.querySelector("#archiveModelName"),
    transmissionCount: document.querySelector("#transmissionCount"),
    archiveNotice: document.querySelector("#archiveNotice"),
    transmissionList: document.querySelector("#transmissionList"),
  };

  let snapshot = null;
  let eventSocket = null;
  let eventRetryTimer = null;
  let eventRetryAttempt = 0;
  let audioSocket = null;
  let audioRetryTimer = null;
  let audioRetryAttempt = 0;
  let audioContext = null;
  let audioNode = null;
  let audioPort = null;
  let gainNode = null;
  let listening = false;
  let muted = false;
  let callTimer = null;
  let activityCount = 0;
  let audioFramesReceived = 0;
  let archiveRefreshTimer = null;
  let archiveLoading = false;
  let archiveSignature = "";
  let pendingArchivePayload = null;
  let archiveAudioPlaying = false;

  function websocketUrl(path) {
    const protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
    return `${protocol}//${window.location.host}${path}`;
  }

  function formatFrequency(value) {
    const frequency = Number(value);
    return Number.isFinite(frequency) && frequency > 0
      ? `${(frequency / 1_000_000).toFixed(4)} MHz`
      : "—";
  }

  function formatInteger(value) {
    const number = Number(value);
    return Number.isFinite(number) ? Math.max(0, number).toLocaleString() : "0";
  }

  function talkgroupId(value) {
    const id = Number(value);
    return Number.isInteger(id) && id > 0 ? id : null;
  }

  function talkgroupMetadata(value) {
    return configuredTalkgroups.get(talkgroupId(value));
  }

  function isConfiguredTalkgroup(value) {
    const talkgroup = talkgroupMetadata(value);
    return Boolean(talkgroup && talkgroup.enabled !== false);
  }

  function monitoringSummary() {
    if (receiverConfig?.monitor_unlisted) {
      return "Configured talkgroups and all other clear traffic on this site are being monitored.";
    }
    return "Configured talkgroups are being monitored.";
  }

  function talkgroupName(value, suppliedName) {
    const id = talkgroupId(value);
    const name = typeof suppliedName === "string" ? suppliedName.trim() : "";
    const knownName = talkgroupMetadata(id)?.name;
    if (knownName) {
      return knownName;
    }
    if (name && !/^TGID\s+\d+$/i.test(name)) {
      return name;
    }
    return id ? `Unlisted talkgroup ${id}` : "Unlisted talkgroup";
  }

  function serviceTone(value) {
    const service = String(value || "").toLowerCase();
    if (service.includes("fire")) {
      return "fire";
    }
    if (service.includes("ems") || service.includes("medical")) {
      return "ems";
    }
    if (
      service.includes("police") ||
      service.includes("law") ||
      service.includes("sheriff")
    ) {
      return "police";
    }
    return "other";
  }

  function formatDuration(startedAt) {
    const started = Date.parse(startedAt);
    if (!Number.isFinite(started)) {
      return "—";
    }

    const seconds = Math.max(0, Math.floor((Date.now() - started) / 1000));
    if (seconds < 60) {
      return `${seconds}s`;
    }
    const minutes = Math.floor(seconds / 60);
    return `${minutes}m ${seconds % 60}s`;
  }

  function formatSavedDuration(durationMs, audioSamples) {
    const milliseconds = Number(durationMs);
    const derivedMilliseconds = Number(audioSamples) > 0 ? (Number(audioSamples) / 8000) * 1000 : 0;
    const hasDuration =
      durationMs !== null &&
      durationMs !== undefined &&
      durationMs !== "" &&
      Number.isFinite(milliseconds) &&
      milliseconds >= 0;
    const totalSeconds = Math.max(
      0,
      Math.round((hasDuration ? milliseconds : derivedMilliseconds) / 1000),
    );
    if (totalSeconds < 60) {
      return `${totalSeconds}s`;
    }
    const minutes = Math.floor(totalSeconds / 60);
    return `${minutes}m ${String(totalSeconds % 60).padStart(2, "0")}s`;
  }

  function formatTransmissionTime(value) {
    const date = new Date(value);
    if (!Number.isFinite(date.getTime())) {
      return "Time unavailable";
    }
    return date.toLocaleString([], {
      month: "short",
      day: "numeric",
      hour: "numeric",
      minute: "2-digit",
      second: "2-digit",
    });
  }

  function recordingStatus(status) {
    switch (String(status || "").toLowerCase()) {
      case "complete":
      case "completed":
      case "saved":
        return { label: "Saved", tone: "good" };
      case "recording":
      case "open":
        return { label: "Recording", tone: "pending" };
      case "empty":
        return { label: "No audio", tone: "" };
      case "interrupted":
        return { label: "Interrupted", tone: "bad" };
      case "failed":
        return { label: "Recording failed", tone: "bad" };
      default:
        return { label: "Pending", tone: "pending" };
    }
  }

  function endReasonLabel(reason) {
    const labels = {
      simple_terminator: "Normal call end",
      link_control_terminator: "Normal call end",
      inactivity_timeout: "Inactivity timeout",
      preempted: "Priority preemption",
      encrypted_voice_header: "Encryption detected",
      encrypted_sync: "Encryption detected",
      encrypted_link_control: "Encryption detected",
      engine_stopped: "Receiver stopped",
    };
    const normalized = String(reason || "").toLowerCase();
    return labels[normalized] || (normalized ? normalized.replaceAll("_", " ") : "");
  }

  function transcriptText(item) {
    if (typeof item.transcript === "string" && item.transcript.trim()) {
      return item.transcript.trim();
    }
    if (!Array.isArray(item.transcript_segments)) {
      return "";
    }
    return item.transcript_segments
      .map((segment) => (typeof segment === "string" ? segment : segment?.text))
      .filter((text) => typeof text === "string" && text.trim())
      .map((text) => text.trim())
      .join(" ");
  }

  function transcriptPresentation(item) {
    const status = String(item.transcription_status || "").toLowerCase();
    const text = transcriptText(item);

    if (item.encrypted) {
      return {
        label: "Encrypted traffic",
        tone: "bad",
        copy: "No clear audio transcript is available for this transmission.",
        placeholder: true,
      };
    }

    switch (status) {
      case "complete":
        return text
          ? {
              label: "Machine transcript · Unverified",
              tone: "",
              copy: text,
              placeholder: false,
            }
          : {
              label: "Transcript unavailable",
              tone: "",
              copy: "The local model completed without returning transcript text.",
              placeholder: true,
            };
      case "processing":
        return {
          label: "Transcribing locally",
          tone: "pending",
          copy: "The local model is processing this recording.",
          placeholder: true,
        };
      case "queued":
        return {
          label: "Transcript queued",
          tone: "pending",
          copy: "Waiting for the local transcription model.",
          placeholder: true,
        };
      case "failed":
        return {
          label: "Transcription failed",
          tone: "bad",
          copy: item.transcription_error || "The local model could not transcribe this recording.",
          placeholder: true,
        };
      case "skipped_short_audio":
        return {
          label: "No transcript",
          tone: "",
          copy: "The recording was too short for a useful machine transcript.",
          placeholder: true,
        };
      case "disabled":
        return {
          label: "Transcription off",
          tone: "",
          copy: "Local transcription was disabled for this recording.",
          placeholder: true,
        };
      default:
        return {
          label: "Transcript pending",
          tone: "pending",
          copy: "Transcript status has not been reported yet.",
          placeholder: true,
        };
    }
  }

  function updateModelBadge(model, modelStatus) {
    const status = String(modelStatus || "").toLowerCase();
    const ready = ["ready", "available", "loaded", "online", "complete"].includes(status);
    const unavailable = ["disabled", "failed", "error", "unavailable", "offline"].includes(status);
    elements.archiveModelBadge.classList.toggle("ready", ready);
    elements.archiveModelBadge.classList.toggle("unavailable", unavailable);

    if (status === "disabled") {
      elements.archiveModelName.textContent = "Local transcription off";
    } else if (model) {
      elements.archiveModelName.textContent = `Local model · ${model}`;
    } else {
      elements.archiveModelName.textContent = ready ? "Local model ready" : "Local model";
    }
    elements.archiveModelBadge.title = `Local transcription model: ${model || "not reported"} · ${status || "status unknown"}`;
  }

  function appendMeta(container, label) {
    if (!label) {
      return;
    }
    const value = document.createElement("span");
    value.textContent = label;
    container.append(value);
  }

  function syncArchivePlaybackState() {
    archiveAudioPlaying = [...elements.transmissionList.querySelectorAll(".archive-audio")].some(
      (audio) => !audio.paused && !audio.ended,
    );
    if (!archiveAudioPlaying && pendingArchivePayload) {
      const payload = pendingArchivePayload;
      pendingArchivePayload = null;
      renderTransmissionArchive(payload);
    }
  }

  function createTransmissionItem(item) {
    const article = document.createElement("article");
    article.className = "transmission-item";
    article.dataset.transmissionId = String(item.id || "");

    const summary = document.createElement("div");
    summary.className = "transmission-summary";
    const titleRow = document.createElement("div");
    titleRow.className = "transmission-title";
    const serviceDot = document.createElement("span");
    const service = serviceTone(item.service);
    serviceDot.className = `archive-service-dot ${service}`;
    serviceDot.setAttribute("aria-hidden", "true");

    const titleCopy = document.createElement("div");
    titleCopy.className = "transmission-title-copy";
    const title = document.createElement("h3");
    title.textContent = talkgroupName(item.talkgroup_id, item.talkgroup_name);
    const time = document.createElement("time");
    time.dateTime = item.started_at || "";
    time.textContent = formatTransmissionTime(item.started_at);
    titleCopy.append(title, time);

    const badges = document.createElement("div");
    badges.className = "transmission-badges";
    const savedState = recordingStatus(item.recording_status);
    const statusBadge = document.createElement("span");
    statusBadge.className = `archive-status ${savedState.tone}`.trim();
    statusBadge.textContent = savedState.label;
    badges.append(statusBadge);
    if (item.encrypted) {
      const encryptedBadge = document.createElement("span");
      encryptedBadge.className = "archive-status encrypted";
      encryptedBadge.textContent = "Encrypted";
      badges.append(encryptedBadge);
    }
    titleRow.append(serviceDot, titleCopy, badges);

    const meta = document.createElement("div");
    meta.className = "transmission-meta";
    appendMeta(meta, formatSavedDuration(item.duration_ms, item.audio_samples));
    appendMeta(meta, item.talkgroup_id ? `TGID ${item.talkgroup_id}` : "");
    if (
      item.traffic_talkgroup_id &&
      Number(item.traffic_talkgroup_id) !== Number(item.talkgroup_id)
    ) {
      appendMeta(meta, `Patch ${item.traffic_talkgroup_id}`);
    }
    appendMeta(meta, formatFrequency(item.frequency_hz));
    appendMeta(meta, item.source_unit ? `Unit ${item.source_unit}` : "Source unknown");
    appendMeta(meta, endReasonLabel(item.end_reason));
    summary.append(titleRow, meta);

    if (typeof item.audio_url === "string" && item.audio_url) {
      const audio = document.createElement("audio");
      audio.className = "archive-audio";
      audio.controls = true;
      audio.preload = "metadata";
      audio.src = item.audio_url;
      audio.setAttribute("aria-label", `Play ${title.textContent} transmission from ${time.textContent}`);
      audio.addEventListener("play", () => {
        elements.transmissionList.querySelectorAll(".archive-audio").forEach((other) => {
          if (other !== audio) {
            other.pause();
          }
        });
        archiveAudioPlaying = true;
      });
      audio.addEventListener("pause", () => queueMicrotask(syncArchivePlaybackState));
      audio.addEventListener("ended", () => queueMicrotask(syncArchivePlaybackState));
      summary.append(audio);
    } else {
      const unavailable = document.createElement("div");
      unavailable.className = "audio-unavailable";
      unavailable.textContent =
        Number(item.audio_samples) > 0
          ? "Saved audio is currently unavailable."
          : "No clear audio was saved.";
      summary.append(unavailable);
    }

    const transcript = document.createElement("div");
    transcript.className = "transmission-transcript";
    const presentation = transcriptPresentation(item);
    const transcriptLabel = document.createElement("div");
    transcriptLabel.className = `transcript-label ${presentation.tone}`.trim();
    transcriptLabel.textContent = presentation.label;
    const transcriptCopy = document.createElement("p");
    transcriptCopy.className = `transcript-copy${presentation.placeholder ? " placeholder" : ""}`;
    transcriptCopy.textContent = presentation.copy;
    transcript.append(transcriptLabel, transcriptCopy);

    if (item.transcription_status === "complete") {
      const footnote = document.createElement("small");
      footnote.className = "transcript-footnote";
      const model = item.transcription_model ? ` Model: ${item.transcription_model}.` : "";
      const transcriptionMs = Number(item.transcription_duration_ms);
      const timing =
        Number.isFinite(transcriptionMs) && transcriptionMs > 0
          ? ` Processed locally in ${(transcriptionMs / 1000).toFixed(1)}s.`
          : "";
      footnote.textContent = `Machine-generated and unverified—confirm wording against the audio.${model}${timing}`;
      transcript.append(footnote);
    }

    article.append(summary, transcript);
    return article;
  }

  function archivePayloadSignature(payload) {
    return JSON.stringify({
      items: payload.items,
      total: payload.total,
      model: payload.model,
      model_status: payload.model_status,
    });
  }

  function renderTransmissionArchive(payload) {
    const items = Array.isArray(payload.items)
      ? [...payload.items].sort(
          (left, right) =>
            (Date.parse(right.started_at) || 0) - (Date.parse(left.started_at) || 0),
        )
      : [];
    const normalizedPayload = { ...payload, items };
    const nextSignature = archivePayloadSignature(normalizedPayload);

    updateModelBadge(payload.model, payload.model_status);
    const total = Number.isFinite(Number(payload.total)) ? Number(payload.total) : items.length;
    elements.transmissionCount.textContent =
      total > items.length
        ? `Latest ${items.length} of ${total} recordings`
        : `${total} audio recording${total === 1 ? "" : "s"}`;
    elements.transmissionList.setAttribute("aria-busy", "false");

    if (nextSignature === archiveSignature) {
      return;
    }
    if (archiveAudioPlaying) {
      pendingArchivePayload = normalizedPayload;
      return;
    }

    archiveSignature = nextSignature;
    elements.transmissionList.replaceChildren();
    if (items.length === 0) {
      const empty = document.createElement("div");
      empty.className = "archive-state";
      const heading = document.createElement("strong");
      heading.textContent = "No saved transmissions yet";
      const detail = document.createElement("small");
      detail.textContent = "Completed receiver calls will appear here automatically.";
      empty.append(heading, detail);
      elements.transmissionList.append(empty);
      return;
    }

    const fragment = document.createDocumentFragment();
    items.forEach((item) => fragment.append(createTransmissionItem(item)));
    elements.transmissionList.append(fragment);
  }

  function renderArchiveError(message) {
    elements.transmissionList.setAttribute("aria-busy", "false");
    if (archiveSignature) {
      elements.archiveNotice.hidden = false;
      elements.archiveNotice.textContent = `${message} Showing the last available archive data.`;
      return;
    }

    elements.transmissionCount.textContent = "Unavailable";
    elements.transmissionList.replaceChildren();
    const error = document.createElement("div");
    error.className = "archive-state error";
    const heading = document.createElement("strong");
    heading.textContent = "Saved transmissions unavailable";
    const detail = document.createElement("small");
    detail.textContent = message;
    error.append(heading, detail);
    elements.transmissionList.append(error);
  }

  async function loadTransmissions() {
    if (archiveLoading) {
      return;
    }
    archiveLoading = true;
    try {
      const response = await fetch("/api/transmissions?limit=20&has_audio=true", {
        headers: { Accept: "application/json" },
        cache: "no-store",
      });
      if (!response.ok) {
        throw new Error(`Archive request returned ${response.status}`);
      }
      const payload = await response.json();
      if (!payload || !Array.isArray(payload.items)) {
        throw new Error("Archive returned an invalid response");
      }
      elements.archiveNotice.hidden = true;
      elements.archiveNotice.textContent = "";
      renderTransmissionArchive(payload);
    } catch (error) {
      renderArchiveError(error instanceof Error ? error.message : "Archive request failed");
    } finally {
      archiveLoading = false;
    }
  }

  function createTalkgroupCard(talkgroup) {
    const service = serviceTone(talkgroup.service);
    const article = document.createElement("article");
    article.className = "talkgroup-card";
    article.dataset.talkgroup = String(talkgroup.id);

    const accent = document.createElement("div");
    accent.className = `talkgroup-accent ${service}`;

    const top = document.createElement("div");
    top.className = "talkgroup-top";
    const icon = document.createElement("span");
    icon.className = `tg-icon ${service}`;
    icon.setAttribute("aria-hidden", "true");
    icon.innerHTML = `<svg viewBox="0 0 24 24">${SERVICE_ICONS[service] || SERVICE_ICONS.other}</svg>`;
    const state = document.createElement("span");
    state.className = "tg-state";
    state.innerHTML = "<i></i><span>Monitoring</span>";
    top.append(icon, state);

    const title = document.createElement("h3");
    title.textContent = talkgroup.name || `TGID ${talkgroup.id}`;

    const description = document.createElement("p");
    description.textContent =
      talkgroup.description || `${SERVICE_LABELS[service] || "Other"} · Priority ${talkgroup.priority}`;

    const facts = document.createElement("div");
    facts.className = "talkgroup-id";
    const idFact = document.createElement("div");
    idFact.innerHTML = "<span>TGID</span><strong></strong>";
    idFact.querySelector("strong").textContent = String(talkgroup.id);
    const priorityFact = document.createElement("div");
    priorityFact.innerHTML = "<span>PRIORITY</span><strong></strong>";
    priorityFact.querySelector("strong").textContent = String(talkgroup.priority);
    facts.append(idFact, priorityFact);

    article.append(accent, top, title, description, facts);
    return article;
  }

  function renderConfig(config) {
    receiverConfig = config;
    configuredTalkgroups.clear();
    const talkgroups = Array.isArray(config.talkgroups) ? config.talkgroups : [];
    talkgroups.forEach((talkgroup) => {
      const id = talkgroupId(talkgroup.id);
      if (id !== null) {
        configuredTalkgroups.set(id, talkgroup);
      }
    });

    const site = config.site || {};
    if (site.system) {
      elements.brandSubtitle.textContent = site.system;
    }
    elements.siteEyebrow.textContent = site.system || "P25 Phase 1";
    const siteFacts = [
      site.name,
      Number.isFinite(Number(site.rfss)) && Number.isFinite(Number(site.site))
        ? `RFSS ${site.rfss} · Site ${site.site}`
        : "",
      site.nac_hex ? `NAC ${site.nac_hex}` : "",
      typeof site.modulation === "string" ? site.modulation.toUpperCase() : "",
    ].filter(Boolean);
    elements.siteSummary.textContent = siteFacts.join(" · ") || elements.siteSummary.textContent;
    if (config.version) {
      elements.footerVersion.textContent = `Trunkline v${config.version}`;
    }

    const enabled = talkgroups.filter((talkgroup) => talkgroup.enabled !== false);
    elements.talkgroupCount.textContent = `${enabled.length} configured`;
    elements.sitewideCoverage.hidden = !config.monitor_unlisted;
    elements.scanDescription.textContent = config.monitor_unlisted
      ? "The highest-priority clear call always wins the voice tuner. Any other clear talkgroup on this site is followed at background priority, and every followed call is recorded on this device."
      : "The highest-priority clear call always wins the voice tuner. Only the talkgroups below are followed, and every followed call is recorded on this device.";

    elements.talkgroupGrid.replaceChildren();
    if (enabled.length === 0) {
      const empty = document.createElement("div");
      empty.className = "archive-state";
      const heading = document.createElement("strong");
      heading.textContent = "No talkgroups configured";
      const detail = document.createElement("small");
      detail.textContent = config.monitor_unlisted
        ? "Every clear group call on this site will be followed."
        : "Add [[talkgroups]] entries to the receiver configuration.";
      empty.append(heading, detail);
      elements.talkgroupGrid.append(empty);
    } else {
      const fragment = document.createDocumentFragment();
      enabled.forEach((talkgroup) => fragment.append(createTalkgroupCard(talkgroup)));
      elements.talkgroupGrid.append(fragment);
    }
    elements.talkgroupCards = [...elements.talkgroupGrid.querySelectorAll(".talkgroup-card")];
    if (snapshot) {
      updateCall(snapshot.active_call);
    } else {
      elements.activeTalkgroupMeta.textContent = monitoringSummary();
    }
  }

  async function loadConfig() {
    try {
      const response = await fetch("/api/config", {
        headers: { Accept: "application/json" },
        cache: "no-store",
      });
      if (!response.ok) {
        throw new Error(`Config request returned ${response.status}`);
      }
      renderConfig(await response.json());
    } catch (error) {
      elements.talkgroupCount.textContent = "Unavailable";
      elements.talkgroupGrid.replaceChildren();
      const failed = document.createElement("div");
      failed.className = "archive-state error";
      const heading = document.createElement("strong");
      heading.textContent = "Configuration unavailable";
      const detail = document.createElement("small");
      detail.textContent = error instanceof Error ? error.message : "Config request failed";
      failed.append(heading, detail);
      elements.talkgroupGrid.append(failed);
    }
  }

  function setConnectionStatus(state, label) {
    elements.eventStatus.classList.remove("connected", "disconnected");
    if (state) {
      elements.eventStatus.classList.add(state);
    }
    elements.eventStatus.querySelector("span:last-child").textContent = label;
    elements.eventStatus.title = `Receiver feed: ${label}`;
  }

  function updateMode(mode) {
    const normalized = typeof mode === "string" ? mode : "starting";
    const label = normalized.charAt(0).toUpperCase() + normalized.slice(1);
    elements.receiverMode.textContent = label;
    elements.receiverMode.className = "mode-badge";
    if (normalized === "control" || normalized === "voice") {
      elements.receiverMode.classList.add("healthy");
    } else if (normalized === "degraded" || normalized === "stopped") {
      elements.receiverMode.classList.add(normalized);
    }
  }

  function updateCall(call) {
    const activeId = call ? talkgroupId(call.talkgroup_id) : null;
    const isPinnedCall = activeId !== null && isConfiguredTalkgroup(activeId);
    elements.talkgroupCards.forEach((card) => {
      const isActive = Number(card.dataset.talkgroup) === activeId;
      card.classList.toggle("active", isActive);
      card.querySelector(".tg-state span").textContent = isActive ? "Active call" : "Monitoring";
    });
    elements.sitewideCoverage.classList.toggle("active", Boolean(call) && !isPinnedCall);
    elements.sitewideCoverageLabel.textContent =
      call && !isPinnedCall
        ? "Background talkgroup active"
        : "All other clear talkgroups · Background";

    if (!call) {
      elements.callStatus.classList.add("is-idle");
      elements.callStatus.lastChild.textContent = "Standby";
      elements.signalBars.classList.remove("active");
      elements.signalBars.setAttribute("aria-label", "No active signal");
      elements.activeServiceIcon.classList.remove("fire", "ems", "other");
      elements.activeTalkgroup.textContent = "Waiting for radio traffic";
      elements.activeTalkgroupMeta.textContent = monitoringSummary();
      elements.activeFrequency.textContent = "—";
      elements.activeSource.textContent = "—";
      elements.activeDuration.textContent = "—";
      return;
    }

    const configured = talkgroupMetadata(activeId);
    const trafficId = Number(call.traffic_talkgroup_id);
    const patchLabel =
      Number.isFinite(trafficId) && trafficId > 0 && trafficId !== activeId
        ? ` · Patched via TGID ${trafficId}`
        : "";
    const name = talkgroupName(activeId, call.talkgroup_name);
    const service = serviceTone(call.service || configured?.service);
    elements.callStatus.classList.remove("is-idle");
    elements.callStatus.lastChild.textContent = "Live";
    elements.signalBars.classList.add("active");
    elements.signalBars.setAttribute("aria-label", "Active radio signal");
    elements.activeServiceIcon.classList.toggle("fire", service === "fire");
    elements.activeServiceIcon.classList.toggle("ems", service === "ems");
    elements.activeServiceIcon.classList.toggle("other", service === "other");
    elements.activeTalkgroup.textContent = name;
    elements.activeTalkgroupMeta.textContent = `TGID ${activeId}${patchLabel}${call.encrypted ? " · Encrypted traffic" : " · Clear voice traffic"}`;
    elements.activeFrequency.textContent = formatFrequency(call.frequency_hz);
    elements.activeSource.textContent = call.source_unit ? `Unit ${call.source_unit}` : "Unknown";
    elements.activeDuration.textContent = formatDuration(call.started_at);
  }

  function renderDevices(devices) {
    elements.deviceList.replaceChildren();
    if (!Array.isArray(devices) || devices.length === 0) {
      const row = document.createElement("div");
      row.className = "device-row";
      row.innerHTML =
        '<span class="device-light"></span><div><strong>No receivers detected</strong><small>Check USB device access</small></div>';
      elements.deviceList.append(row);
      return;
    }

    devices.forEach((device) => {
      const row = document.createElement("div");
      row.className = `device-row${device.connected ? " connected" : ""}`;

      const light = document.createElement("span");
      light.className = "device-light";

      const copy = document.createElement("div");
      const title = document.createElement("strong");
      const role = String(device.role || "receiver");
      title.textContent = `${role.charAt(0).toUpperCase() + role.slice(1)} receiver`;
      const detail = document.createElement("small");
      detail.textContent = device.connected
        ? device.serial || `RTL-SDR device ${Number(device.index) + 1}`
        : "Disconnected";
      copy.append(title, detail);

      const metrics = document.createElement("div");
      metrics.className = "device-details";
      const hasPower =
        typeof device.power_dbfs === "number" && Number.isFinite(device.power_dbfs);
      metrics.textContent = hasPower
        ? `${device.power_dbfs.toFixed(1)} dBFS`
        : formatFrequency(device.tuned_hz);

      row.append(light, copy, metrics);
      elements.deviceList.append(row);
    });
  }

  function renderSnapshot(nextSnapshot) {
    if (!nextSnapshot || typeof nextSnapshot !== "object") {
      return;
    }

    snapshot = nextSnapshot;
    updateMode(snapshot.mode);
    elements.systemName.textContent = snapshot.system_name || "—";
    elements.siteName.textContent = snapshot.site_name || "—";
    elements.controlFrequency.textContent = formatFrequency(snapshot.control_frequency_hz);
    elements.controlLock.textContent = snapshot.control_locked ? "Locked" : "Searching";
    elements.controlLock.classList.toggle("locked", Boolean(snapshot.control_locked));
    elements.receiverUptime.textContent = formatDuration(snapshot.started_at);
    elements.framesDecoded.textContent = formatInteger(snapshot.frames_decoded);
    elements.crcErrors.textContent = formatInteger(snapshot.crc_errors);
    elements.lastError.hidden = !snapshot.last_error;
    elements.lastError.textContent = snapshot.last_error || "";
    elements.lastUpdated.textContent = `Updated ${new Date().toLocaleTimeString([], {
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
    })}`;
    renderDevices(snapshot.devices);
    updateCall(snapshot.active_call);
  }

  function addActivity(kind, title, detail) {
    if (activityCount === 0) {
      elements.activityList.replaceChildren();
    }

    const item = document.createElement("li");
    const indicator = document.createElement("span");
    indicator.className = `activity-indicator ${kind}`;
    const copy = document.createElement("div");
    copy.className = "activity-copy";
    const heading = document.createElement("strong");
    heading.textContent = title;
    const secondary = document.createElement("small");
    secondary.textContent = detail;
    copy.append(heading, secondary);
    const time = document.createElement("time");
    time.className = "activity-time";
    time.dateTime = new Date().toISOString();
    time.textContent = new Date().toLocaleTimeString([], {
      hour: "2-digit",
      minute: "2-digit",
    });
    item.append(indicator, copy, time);
    elements.activityList.prepend(item);
    activityCount += 1;

    while (elements.activityList.children.length > 12) {
      elements.activityList.lastElementChild.remove();
    }
  }

  function mergeSnapshot(changes) {
    if (!snapshot) {
      void loadState();
      return;
    }
    renderSnapshot({ ...snapshot, ...changes, updated_at: new Date().toISOString() });
  }

  function processEvent(message) {
    if (!message || typeof message !== "object") {
      return;
    }

    if (message.mode && Array.isArray(message.devices)) {
      renderSnapshot(message);
      return;
    }

    const type = String(message.type || message.event || "").toLowerCase();
    const data = message.data ?? message.payload ?? message;
    switch (type) {
      case "snapshot":
      case "state":
        renderSnapshot(data);
        break;
      case "call_started": {
        const call = data.active_call || data;
        const trafficId = Number(call.traffic_talkgroup_id);
        const patchLabel =
          Number.isFinite(trafficId) &&
          trafficId > 0 &&
          trafficId !== Number(call.talkgroup_id)
            ? ` · Patch TGID ${trafficId}`
            : "";
        mergeSnapshot({ active_call: call, mode: "voice" });
        addActivity(
          "call",
          talkgroupName(call.talkgroup_id, call.talkgroup_name),
          `${formatFrequency(call.frequency_hz)} · TGID ${talkgroupId(call.talkgroup_id) || "unknown"}${patchLabel}`,
        );
        break;
      }
      case "call_ended":
        if (!snapshot?.active_call || Number(data.talkgroup_id) === Number(snapshot.active_call.talkgroup_id)) {
          mergeSnapshot({ active_call: null, mode: snapshot?.control_locked ? "control" : "starting" });
        }
        addActivity("call", "Call ended", `TGID ${data.talkgroup_id || "unknown"}`);
        break;
      case "control_channel_changed":
        mergeSnapshot({ control_frequency_hz: data.frequency_hz });
        addActivity("channel", "Control channel changed", formatFrequency(data.frequency_hz));
        break;
      case "error":
        mergeSnapshot({ last_error: data.message, mode: "degraded" });
        addActivity("error", "Receiver error", data.message || "Unknown receiver error");
        break;
      default:
        if (data.mode && Array.isArray(data.devices)) {
          renderSnapshot(data);
        }
    }
  }

  async function loadState() {
    try {
      const response = await fetch("/api/state", {
        headers: { Accept: "application/json" },
        cache: "no-store",
      });
      if (!response.ok) {
        throw new Error(`State request returned ${response.status}`);
      }
      renderSnapshot(await response.json());
    } catch (error) {
      addActivity("error", "State unavailable", error.message);
    }
  }

  function scheduleEventReconnect() {
    window.clearTimeout(eventRetryTimer);
    const delay = Math.min(15_000, 750 * 2 ** eventRetryAttempt);
    eventRetryAttempt = Math.min(eventRetryAttempt + 1, 5);
    setConnectionStatus("disconnected", "Reconnecting");
    eventRetryTimer = window.setTimeout(connectEvents, delay);
  }

  function connectEvents() {
    window.clearTimeout(eventRetryTimer);
    if (eventSocket) {
      eventSocket.onclose = null;
      eventSocket.close();
    }

    setConnectionStatus("", "Connecting");
    try {
      eventSocket = new WebSocket(websocketUrl("/api/events"));
    } catch (_error) {
      scheduleEventReconnect();
      return;
    }

    eventSocket.addEventListener("open", () => {
      eventRetryAttempt = 0;
      setConnectionStatus("connected", "Live");
      elements.reconnectButton.classList.remove("reconnecting");
    });

    eventSocket.addEventListener("message", (event) => {
      try {
        processEvent(JSON.parse(event.data));
      } catch (error) {
        console.warn("Ignored malformed receiver event", error);
      }
    });

    eventSocket.addEventListener("close", scheduleEventReconnect);
    eventSocket.addEventListener("error", () => eventSocket.close());
  }

  async function prepareAudio() {
    if (audioContext && audioNode && audioPort && gainNode) {
      if (audioContext.state === "suspended") {
        await audioContext.resume();
      }
      return;
    }

    const Context = window.AudioContext || window.webkitAudioContext;
    if (!Context) {
      throw new Error("This browser does not support live audio playback");
    }

    audioContext = new Context({ latencyHint: "interactive" });
    if (audioContext.audioWorklet && window.AudioWorkletNode) {
      await audioContext.audioWorklet.addModule("/audio-worklet.js");
      audioNode = new AudioWorkletNode(audioContext, "pcm-jitter-processor", {
        numberOfInputs: 0,
        numberOfOutputs: 1,
        outputChannelCount: [1],
      });
      audioPort = audioNode.port;
    } else {
      ({ node: audioNode, port: audioPort } = createLegacyAudioNode(audioContext));
    }
    gainNode = audioContext.createGain();
    gainNode.gain.value = muted ? 0 : 1;
    audioNode.connect(gainNode).connect(audioContext.destination);
    audioPort.addEventListener("message", (event) => {
      if (event.data.type === "stats" && listening && audioSocket?.readyState === WebSocket.OPEN) {
        const label = event.data.playing ? "Playing buffered audio" : "Listening for calls";
        elements.audioStatus.textContent = muted ? "Live audio muted" : label;
        elements.player.classList.toggle("streaming", event.data.playing && !muted);
      }
    });
    audioPort.start();
    await audioContext.resume();
  }

  function createLegacyAudioNode(context) {
    const capacity = 32768;
    const ring = new Float32Array(capacity);
    let readIndex = 0;
    let writeIndex = 0;
    let available = 0;
    let inputRate = 8000;
    let sourcePosition = 0;
    let playing = false;
    let expectedSequence = null;
    let underruns = 0;
    let droppedFrames = 0;
    let renderBlocks = 0;
    let messageListener = null;

    function reset() {
      readIndex = 0;
      writeIndex = 0;
      available = 0;
      sourcePosition = 0;
      playing = false;
      expectedSequence = null;
    }

    function pushFrame(arrayBuffer) {
      if (!(arrayBuffer instanceof ArrayBuffer) || arrayBuffer.byteLength < 18) {
        return;
      }
      const view = new DataView(arrayBuffer);
      const sequence = view.getUint32(0, true);
      const incomingRate = view.getUint16(10, true);
      const sampleCount = Math.floor((arrayBuffer.byteLength - 16) / 2);
      if (incomingRate > 0 && incomingRate !== inputRate) {
        inputRate = incomingRate;
        sourcePosition = 0;
      }
      if (expectedSequence !== null && sequence !== expectedSequence) {
        droppedFrames += 1;
      }
      expectedSequence = (sequence + 1) >>> 0;

      for (let index = 0; index < sampleCount; index += 1) {
        if (available === capacity) {
          readIndex = (readIndex + 1) % capacity;
          available -= 1;
        }
        ring[writeIndex] = view.getInt16(16 + index * 2, true) / 32768;
        writeIndex = (writeIndex + 1) % capacity;
        available += 1;
      }
    }

    function peek(offset) {
      return ring[(readIndex + offset) % capacity];
    }

    function discard(count) {
      const discarded = Math.min(count, available);
      readIndex = (readIndex + discarded) % capacity;
      available -= discarded;
    }

    const node = context.createScriptProcessor(1024, 0, 1);
    node.onaudioprocess = (event) => {
      const output = event.outputBuffer.getChannelData(0);
      const jitterTarget = Math.max(160, Math.round(inputRate * 0.2));
      if (!playing && available >= jitterTarget) {
        playing = true;
      }

      if (!playing) {
        output.fill(0);
      } else {
        const step = inputRate / context.sampleRate;
        for (let index = 0; index < output.length; index += 1) {
          if (available < 2) {
            output.fill(0, index);
            playing = false;
            sourcePosition = 0;
            underruns += 1;
            break;
          }
          const first = peek(0);
          output[index] = first + (peek(1) - first) * sourcePosition;
          sourcePosition += step;
          const consumed = Math.floor(sourcePosition);
          if (consumed > 0) {
            discard(consumed);
            sourcePosition -= consumed;
          }
        }
      }

      renderBlocks += 1;
      if (renderBlocks % 25 === 0 && messageListener) {
        messageListener({
          data: {
            type: "stats",
            bufferedSamples: available,
            inputRate,
            underruns,
            droppedFrames,
            playing,
          },
        });
      }
    };

    return {
      node,
      port: {
        addEventListener(type, listener) {
          if (type === "message") {
            messageListener = listener;
          }
        },
        start() {},
        postMessage(message) {
          if (message.type === "reset") {
            reset();
          } else if (message.type === "pcm") {
            pushFrame(message.buffer);
          }
        },
      },
    };
  }

  function scheduleAudioReconnect() {
    window.clearTimeout(audioRetryTimer);
    elements.player.classList.remove("streaming");
    if (!listening) {
      return;
    }

    const delay = Math.min(12_000, 500 * 2 ** audioRetryAttempt);
    audioRetryAttempt = Math.min(audioRetryAttempt + 1, 5);
    elements.audioStatus.textContent = "Reconnecting audio…";
    audioRetryTimer = window.setTimeout(connectAudio, delay);
  }

  function connectAudio() {
    window.clearTimeout(audioRetryTimer);
    if (!listening) {
      return;
    }
    if (audioSocket) {
      audioSocket.onclose = null;
      audioSocket.close();
    }

    elements.audioStatus.textContent = "Connecting audio…";
    audioFramesReceived = 0;
    audioPort?.postMessage({ type: "reset" });
    try {
      audioSocket = new WebSocket(websocketUrl("/api/audio"));
      audioSocket.binaryType = "arraybuffer";
    } catch (_error) {
      scheduleAudioReconnect();
      return;
    }

    audioSocket.addEventListener("open", () => {
      audioRetryAttempt = 0;
      elements.audioStatus.textContent = muted ? "Live audio muted" : "Listening for calls";
    });

    audioSocket.addEventListener("message", (event) => {
      if (!(event.data instanceof ArrayBuffer) || event.data.byteLength < 16) {
        return;
      }

      const header = new DataView(event.data, 0, 16);
      const sampleRate = header.getUint16(10, true);
      if (sampleRate === 0) {
        return;
      }

      audioFramesReceived += 1;
      audioPort.postMessage({ type: "pcm", buffer: event.data }, [event.data]);
      if (audioFramesReceived === 1) {
        elements.audioStatus.textContent = muted ? "Live audio muted" : "Buffering live audio…";
      }
    });

    audioSocket.addEventListener("close", scheduleAudioReconnect);
    audioSocket.addEventListener("error", () => audioSocket.close());
  }

  async function startListening() {
    elements.listenButton.disabled = true;
    elements.audioStatus.textContent = "Starting audio…";
    try {
      await prepareAudio();
      listening = true;
      elements.listenButton.classList.add("listening");
      elements.listenLabel.textContent = "Stop listening";
      elements.muteButton.disabled = false;
      connectAudio();
    } catch (error) {
      listening = false;
      elements.audioStatus.textContent = error.message;
      addActivity("error", "Audio unavailable", error.message);
    } finally {
      elements.listenButton.disabled = false;
    }
  }

  function stopListening() {
    listening = false;
    window.clearTimeout(audioRetryTimer);
    if (audioSocket) {
      audioSocket.onclose = null;
      audioSocket.close();
      audioSocket = null;
    }
    audioPort?.postMessage({ type: "reset" });
    elements.listenButton.classList.remove("listening");
    elements.listenLabel.textContent = "Listen live";
    elements.muteButton.disabled = true;
    elements.player.classList.remove("streaming");
    elements.audioStatus.textContent = "Audio is off";
  }

  function toggleMute() {
    if (!listening || !gainNode || !audioContext) {
      return;
    }
    muted = !muted;
    gainNode.gain.setTargetAtTime(muted ? 0 : 1, audioContext.currentTime, 0.015);
    elements.muteButton.classList.toggle("muted", muted);
    elements.muteButton.setAttribute("aria-pressed", String(muted));
    elements.muteButton.setAttribute("aria-label", muted ? "Unmute live audio" : "Mute live audio");
    elements.audioStatus.textContent = muted ? "Live audio muted" : "Listening for calls";
    elements.player.classList.toggle("streaming", !muted && audioFramesReceived > 0);
  }

  function reconnectAll() {
    elements.reconnectButton.classList.add("reconnecting");
    eventRetryAttempt = 0;
    audioRetryAttempt = 0;
    void loadConfig();
    void loadState();
    void loadTransmissions();
    connectEvents();
    if (listening) {
      connectAudio();
    }
  }

  function updateTimers() {
    const now = new Date();
    elements.localTime.textContent = now.toLocaleTimeString([], {
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
    });
    if (snapshot) {
      elements.receiverUptime.textContent = formatDuration(snapshot.started_at);
      if (snapshot.active_call) {
        elements.activeDuration.textContent = formatDuration(snapshot.active_call.started_at);
      }
    }
  }

  elements.listenButton.addEventListener("click", () => {
    if (listening) {
      stopListening();
    } else {
      void startListening();
    }
  });
  elements.muteButton.addEventListener("click", toggleMute);
  elements.reconnectButton.addEventListener("click", reconnectAll);
  elements.clearActivityButton.addEventListener("click", () => {
    activityCount = 0;
    elements.activityList.innerHTML = '<li class="empty-activity">Waiting for receiver events…</li>';
  });

  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) {
      if (eventSocket?.readyState !== WebSocket.OPEN) {
        connectEvents();
      }
      void loadTransmissions();
    }
  });

  callTimer = window.setInterval(updateTimers, 1000);
  archiveRefreshTimer = window.setInterval(() => {
    if (!document.hidden) {
      void loadTransmissions();
    }
  }, 5000);
  window.addEventListener(
    "beforeunload",
    () => {
      window.clearInterval(callTimer);
      window.clearInterval(archiveRefreshTimer);
    },
    { once: true },
  );
  updateTimers();
  void loadConfig();
  void loadState();
  void loadTransmissions();
  connectEvents();
})();
