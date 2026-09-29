const url = process.argv[2] ?? "ws://localhost:8097/api/audio";
const durationMs = Number(process.argv[3] ?? 30000);
const socket = new WebSocket(url);
socket.binaryType = "arraybuffer";

let frames = 0;
let samples = 0;
let nonzeroSamples = 0;
let sumSquares = 0;
let peak = 0;
const talkgroups = new Map();

socket.addEventListener("message", (event) => {
  if (!(event.data instanceof ArrayBuffer) || event.data.byteLength < 18) {
    return;
  }
  const view = new DataView(event.data);
  const talkgroup = view.getUint16(8, true);
  talkgroups.set(talkgroup, (talkgroups.get(talkgroup) ?? 0) + 1);
  frames += 1;

  for (let offset = 16; offset + 1 < event.data.byteLength; offset += 2) {
    const sample = view.getInt16(offset, true);
    samples += 1;
    nonzeroSamples += Number(sample !== 0);
    sumSquares += sample * sample;
    peak = Math.max(peak, Math.abs(sample));
  }
});

socket.addEventListener("error", () => {
  console.error("audio WebSocket failed");
  process.exitCode = 1;
});

setTimeout(() => {
  socket.close();
  const rms = samples === 0 ? 0 : Math.sqrt(sumSquares / samples);
  console.log(
    JSON.stringify(
      {
        url,
        duration_ms: durationMs,
        frames,
        samples,
        nonzero_samples: nonzeroSamples,
        rms_pcm16: rms,
        peak_pcm16: peak,
        talkgroups: Object.fromEntries(talkgroups),
      },
      null,
      2,
    ),
  );
}, durationMs);
