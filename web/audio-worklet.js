class PcmJitterProcessor extends AudioWorkletProcessor {
  constructor() {
    super();

    this.capacity = 32768;
    this.buffer = new Float32Array(this.capacity);
    this.readIndex = 0;
    this.writeIndex = 0;
    this.available = 0;
    this.inputRate = 8000;
    this.sourcePosition = 0;
    this.playing = false;
    this.expectedSequence = null;
    this.underruns = 0;
    this.droppedFrames = 0;
    this.renderBlocks = 0;

    this.port.onmessage = (event) => {
      const message = event.data;
      if (message.type === "reset") {
        this.reset();
      } else if (message.type === "pcm" && message.buffer instanceof ArrayBuffer) {
        this.pushFrame(message.buffer);
      }
    };
  }

  reset() {
    this.readIndex = 0;
    this.writeIndex = 0;
    this.available = 0;
    this.sourcePosition = 0;
    this.playing = false;
    this.expectedSequence = null;
  }

  pushFrame(arrayBuffer) {
    if (arrayBuffer.byteLength < 18) {
      return;
    }

    const view = new DataView(arrayBuffer);
    const sequence = view.getBigUint64(0, true);
    const incomingRate = view.getUint16(10, true);
    const sampleCount = Math.floor((arrayBuffer.byteLength - 16) / 2);

    if (incomingRate > 0 && incomingRate !== this.inputRate) {
      this.inputRate = incomingRate;
      this.sourcePosition = 0;
    }

    if (this.expectedSequence !== null && sequence !== this.expectedSequence) {
      this.droppedFrames += 1;
    }
    this.expectedSequence = sequence + 1n;

    for (let i = 0; i < sampleCount; i += 1) {
      if (this.available === this.capacity) {
        this.readIndex = (this.readIndex + 1) % this.capacity;
        this.available -= 1;
      }

      this.buffer[this.writeIndex] = view.getInt16(16 + i * 2, true) / 32768;
      this.writeIndex = (this.writeIndex + 1) % this.capacity;
      this.available += 1;
    }
  }

  peek(offset) {
    return this.buffer[(this.readIndex + offset) % this.capacity];
  }

  discard(count) {
    const discarded = Math.min(count, this.available);
    this.readIndex = (this.readIndex + discarded) % this.capacity;
    this.available -= discarded;
  }

  process(_inputs, outputs) {
    const output = outputs[0][0];
    if (!output) {
      return true;
    }

    const jitterTarget = Math.max(160, Math.round(this.inputRate * 0.2));
    if (!this.playing && this.available >= jitterTarget) {
      this.playing = true;
    }

    if (!this.playing) {
      output.fill(0);
      this.reportStats();
      return true;
    }

    const step = this.inputRate / sampleRate;
    for (let i = 0; i < output.length; i += 1) {
      if (this.available < 2) {
        output.fill(0, i);
        this.playing = false;
        this.sourcePosition = 0;
        this.underruns += 1;
        break;
      }

      const first = this.peek(0);
      const second = this.peek(1);
      output[i] = first + (second - first) * this.sourcePosition;
      this.sourcePosition += step;

      const consumed = Math.floor(this.sourcePosition);
      if (consumed > 0) {
        this.discard(consumed);
        this.sourcePosition -= consumed;
      }
    }

    this.reportStats();
    return true;
  }

  reportStats() {
    this.renderBlocks += 1;
    if (this.renderBlocks % 100 === 0) {
      this.port.postMessage({
        type: "stats",
        bufferedSamples: this.available,
        inputRate: this.inputRate,
        underruns: this.underruns,
        droppedFrames: this.droppedFrames,
        playing: this.playing,
      });
    }
  }
}

registerProcessor("pcm-jitter-processor", PcmJitterProcessor);
