import { writeFile } from "node:fs/promises";

const url = process.argv[2] ?? "ws://localhost:8097/api/audio";
const durationMs = Number(process.argv[3] ?? 30000);
const outputPath = process.argv[4] ?? "data/diagnostic-audio.wav";
const wantedTalkgroup = process.argv[5] ? Number(process.argv[5]) : null;
const socket = new WebSocket(url);
socket.binaryType = "arraybuffer";

const segments = [];
let current = [];
let previousSequence = null;
let sampleRate = 8000;

socket.addEventListener("message", (event) => {
  if (!(event.data instanceof ArrayBuffer) || event.data.byteLength < 18) {
    return;
  }
  const view = new DataView(event.data);
  const talkgroup = view.getUint16(8, true);
  if (wantedTalkgroup !== null && talkgroup !== wantedTalkgroup) {
    return;
  }

  const sequence = Number(view.getBigUint64(0, true));
  sampleRate = view.getUint16(10, true) || sampleRate;
  if (previousSequence !== null && sequence !== previousSequence + 1) {
    if (current.length > 0) {
      segments.push(current);
    }
    current = [];
  }
  previousSequence = sequence;

  for (let offset = 16; offset + 1 < event.data.byteLength; offset += 2) {
    current.push(view.getInt16(offset, true));
  }
});

setTimeout(async () => {
  socket.close();
  if (current.length > 0) {
    segments.push(current);
  }
  segments.sort((left, right) => right.length - left.length);
  const samples = segments[0] ?? [];
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
  console.log(
    JSON.stringify({
      output: outputPath,
      sample_rate_hz: sampleRate,
      segments: segments.length,
      longest_segment_frames: samples.length / 160,
      longest_segment_seconds: samples.length / sampleRate,
    }),
  );
}, durationMs);
