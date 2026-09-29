import { writeFile } from "node:fs/promises";

const url = process.argv[2] ?? "ws://localhost:8097/api/audio";
const outputPath = process.argv[3] ?? "data/target-confirmation.wav";
const minimumSeconds = Number(process.argv[4] ?? 2.5);
const maximumWaitMs = Number(process.argv[5] ?? 10800000);
const targetTalkgroups = new Set(
  (process.argv[6] ?? "1,2,3")
    .split(",")
    .map((value) => Number(value.trim()))
    .filter((value) => Number.isInteger(value) && value > 0),
);

let socket;
let reconnectTimer;
let inactivityTimer;
let heartbeatTimer;
let current = [];
let currentTalkgroup = null;
let previousSequence = null;
let sampleRate = 8000;
let finished = false;
const startedAt = Date.now();

async function writeWav(samples, talkgroup) {
  const pcmBytes = samples.length * 2;
  const wav = Buffer.alloc(44 + pcmBytes);
  wav.write("RIFF", 0);
  wav.writeUInt32LE(36 + pcmBytes, 4);
  wav.write("WAVEfmt ", 8);
  wav.writeUInt32LE(16, 16);
  wav.writeUInt16LE(1, 20);
  wav.writeUInt16LE(1, 22);
  wav.writeUInt32LE(sampleRate, 24);
  wav.writeUInt32LE(sampleRate * 2, 28);
  wav.writeUInt16LE(2, 32);
  wav.writeUInt16LE(16, 34);
  wav.write("data", 36);
  wav.writeUInt32LE(pcmBytes, 40);
  samples.forEach((sample, index) => wav.writeInt16LE(sample, 44 + index * 2));
  await writeFile(outputPath, wav);
  await writeFile(
    `${outputPath}.json`,
    JSON.stringify(
      {
        output: outputPath,
        talkgroup,
        sample_rate_hz: sampleRate,
        frames: samples.length / 160,
        seconds: samples.length / sampleRate,
        captured_at: new Date().toISOString(),
      },
      null,
      2,
    ),
  );
}

async function finishCapture() {
  if (finished || current.length < sampleRate * minimumSeconds) {
    return;
  }
  finished = true;
  clearTimeout(reconnectTimer);
  clearTimeout(inactivityTimer);
  clearInterval(heartbeatTimer);
  socket?.close();
  await writeWav(current, currentTalkgroup);
  console.log(
    JSON.stringify({
      status: "captured",
      talkgroup: currentTalkgroup,
      seconds: current.length / sampleRate,
      output: outputPath,
    }),
  );
  process.exit(0);
}

function resetSegment(talkgroup, sequence) {
  current = [];
  currentTalkgroup = talkgroup;
  previousSequence = sequence - 1;
}

function connect() {
  if (finished) {
    return;
  }
  socket = new WebSocket(url);
  socket.binaryType = "arraybuffer";

  socket.addEventListener("open", () => {
    console.log(
      JSON.stringify({
        status: "monitoring",
        url,
        targets: [...targetTalkgroups],
        minimum_seconds: minimumSeconds,
      }),
    );
  });

  socket.addEventListener("message", (event) => {
    if (!(event.data instanceof ArrayBuffer) || event.data.byteLength < 18) {
      return;
    }
    const view = new DataView(event.data);
    const talkgroup = view.getUint16(8, true);
    if (!targetTalkgroups.has(talkgroup)) {
      return;
    }

    const sequence = Number(view.getBigUint64(0, true));
    sampleRate = view.getUint16(10, true) || sampleRate;
    if (
      currentTalkgroup !== talkgroup ||
      previousSequence === null ||
      sequence !== previousSequence + 1
    ) {
      resetSegment(talkgroup, sequence);
    }
    previousSequence = sequence;

    for (let offset = 16; offset + 1 < event.data.byteLength; offset += 2) {
      current.push(view.getInt16(offset, true));
    }

    clearTimeout(inactivityTimer);
    inactivityTimer = setTimeout(() => void finishCapture(), 700);
    if (current.length >= sampleRate * 12) {
      void finishCapture();
    }
  });

  socket.addEventListener("close", () => {
    if (!finished) {
      reconnectTimer = setTimeout(connect, 1500);
    }
  });

  socket.addEventListener("error", () => socket.close());
}

heartbeatTimer = setInterval(() => {
  console.log(
    JSON.stringify({
      status: "waiting",
      elapsed_seconds: Math.floor((Date.now() - startedAt) / 1000),
      partial_talkgroup: currentTalkgroup,
      partial_seconds: current.length / sampleRate,
    }),
  );
}, 30000);

setTimeout(() => {
  if (!finished) {
    console.error(
      JSON.stringify({
        status: "timeout",
        elapsed_seconds: Math.floor((Date.now() - startedAt) / 1000),
      }),
    );
    process.exit(2);
  }
}, maximumWaitMs);

connect();
