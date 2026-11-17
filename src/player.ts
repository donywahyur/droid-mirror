// Decodes the scrcpy H.264 packet stream with WebCodecs and draws it on a canvas.
//
// Each message from Rust is one scrcpy packet, header included:
//   session packet: byte0 MSB set; u32 width at 4, u32 height at 8 (no payload)
//   media packet:   u64 (bit62 = config, bit61 = key frame, low 61 bits = pts),
//                   u32 size at 8, then the Annex-B payload.

const FLAG_SESSION = 0x80;
const FLAG_CONFIG = 0x40;
const FLAG_KEY = 0x20;
const PTS_MASK = (1n << 61n) - 1n;
// Decoding takes ~3 ms/frame, so a queue drains fast after a Wi-Fi burst.
// Only bail out (drop + request a key frame) when the decoder is truly stuck:
// a fresh key frame is large and costs a lot on a slow link.
const MAX_QUEUE = 30;

/** Split an Annex-B buffer (00 00 01 / 00 00 00 01 start codes) into NAL units. */
function splitNals(data: Uint8Array): Uint8Array[] {
  const nals: Uint8Array[] = [];
  let start = -1;
  let i = 0;
  while (i + 2 < data.length) {
    if (data[i] === 0 && data[i + 1] === 0 && data[i + 2] === 1) {
      if (start >= 0) {
        let end = i;
        if (end > start && data[end - 1] === 0) end--; // 4-byte start code
        nals.push(data.subarray(start, end));
      }
      i += 3;
      start = i;
    } else {
      i++;
    }
  }
  if (start >= 0 && start < data.length) nals.push(data.subarray(start));
  return nals;
}

/** Annex-B -> AVCC (4-byte big-endian length prefix per NAL). */
function toAvcc(nals: Uint8Array[]): Uint8Array {
  const size = nals.reduce((n, nal) => n + 4 + nal.length, 0);
  const out = new Uint8Array(size);
  const view = new DataView(out.buffer);
  let off = 0;
  for (const nal of nals) {
    view.setUint32(off, nal.length);
    out.set(nal, off + 4);
    off += 4 + nal.length;
  }
  return out;
}

/** Build an AVCDecoderConfigurationRecord from SPS + PPS. */
function avcC(sps: Uint8Array, pps: Uint8Array): Uint8Array {
  const out = new Uint8Array(11 + sps.length + pps.length);
  out.set([1, sps[1], sps[2], sps[3], 0xff, 0xe1, sps.length >> 8, sps.length & 0xff]);
  out.set(sps, 8);
  let off = 8 + sps.length;
  out.set([1, pps.length >> 8, pps.length & 0xff], off);
  out.set(pps, off + 3);
  return out;
}

const hex = (b: number) => b.toString(16).padStart(2, "0");

export class Player {
  /** Video size announced by the server; touch coordinates must use it. */
  width = 0;
  height = 0;
  onResize?: (w: number, h: number) => void;
  /** Called when the decoder needs a fresh key frame (error or backlog). */
  onNeedKeyFrame?: (reason: string) => void;
  fps = 0;
  /** Smoothed transit delay above the best seen so far, in ms. Rising = backlog. */
  delayMs = 0;

  /** Called with the SPS (hex) whenever the decoder is configured. */
  onConfig?: (spsHex: string, codec: string) => void;

  /** min(arrival time - device pts): the fastest observed transit, used as zero. */
  private baselineSkew = Infinity;
  /** Per-interval timing stats, read and reset by takeStats(). */
  private stats = { arrived: 0, decoded: 0, decodeSum: 0, decodeMax: 0, queueMax: 0, gapMax: 0, transitMax: 0 };
  private lastArrival = 0;
  private decodeStart = new Map<number, number>();
  private decoder: VideoDecoder | null = null;
  private waitingForKey = true;
  private ctx: CanvasRenderingContext2D;
  private frameCount = 0;
  private fpsTimer: number;

  constructor(private canvas: HTMLCanvasElement) {
    const ctx = canvas.getContext("2d", { alpha: false, desynchronized: true });
    if (!ctx) throw new Error("2D canvas not available");
    this.ctx = ctx;
    this.fpsTimer = window.setInterval(() => {
      this.fps = this.frameCount;
      this.frameCount = 0;
    }, 1000);
  }

  static supported(): boolean {
    return typeof VideoDecoder !== "undefined";
  }

  push(buf: ArrayBuffer) {
    if (buf.byteLength < 12) return;
    const view = new DataView(buf);
    const flags = view.getUint8(0);

    if (flags & FLAG_SESSION) {
      this.width = view.getUint32(4);
      this.height = view.getUint32(8);
      this.waitingForKey = true;
      this.baselineSkew = Infinity; // new encoder session restarts pts
      this.onResize?.(this.width, this.height);
      return;
    }

    const data = new Uint8Array(buf, 12);
    if (flags & FLAG_CONFIG) {
      this.configure(data);
      return;
    }

    const pts = Number(view.getBigUint64(0) & PTS_MASK);
    this.trackDelay(pts);

    const decoder = this.decoder;
    if (!decoder || decoder.state !== "configured") return;

    const key = (flags & FLAG_KEY) !== 0;
    if (this.waitingForKey) {
      if (!key) return;
      this.waitingForKey = false;
    }

    if (decoder.decodeQueueSize > MAX_QUEUE) {
      // Falling behind: drop until the next key frame instead of lagging.
      this.waitingForKey = true;
      this.onNeedKeyFrame?.("decoder backlog");
      return;
    }

    // Parameter sets are already in the decoder description.
    const nals = splitNals(data).filter((n) => {
      const type = n[0] & 0x1f;
      return type !== 7 && type !== 8;
    });
    if (nals.length === 0) return;
    try {
      this.decodeStart.set(pts, performance.now());
      decoder.decode(
        new EncodedVideoChunk({ type: key ? "key" : "delta", timestamp: pts, data: toAvcc(nals) }),
      );
      this.stats.queueMax = Math.max(this.stats.queueMax, decoder.decodeQueueSize);
    } catch (e) {
      this.fail(`decode: ${e}`);
    }
  }

  private trackDelay(ptsMicros: number) {
    const now = performance.now();
    const skew = now - ptsMicros / 1000;
    if (skew < this.baselineSkew) this.baselineSkew = skew;
    const delay = skew - this.baselineSkew;
    this.delayMs = this.delayMs * 0.8 + delay * 0.2;
    const s = this.stats;
    s.arrived++;
    s.transitMax = Math.max(s.transitMax, delay);
    if (this.lastArrival) s.gapMax = Math.max(s.gapMax, now - this.lastArrival);
    this.lastArrival = now;
  }

  /** One line of timing stats for the last interval, then reset. */
  takeStats(): string {
    const s = this.stats;
    const avg = s.decoded ? s.decodeSum / s.decoded : 0;
    const line =
      `arrived=${s.arrived} decoded=${s.decoded} transit(now/max)=${this.delayMs.toFixed(0)}/${s.transitMax.toFixed(0)}ms ` +
      `decode(avg/max)=${avg.toFixed(0)}/${s.decodeMax.toFixed(0)}ms queueMax=${s.queueMax} ` +
      `arrivalGapMax=${s.gapMax.toFixed(0)}ms pending=${this.decodeStart.size}`;
    this.stats = { arrived: 0, decoded: 0, decodeSum: 0, decodeMax: 0, queueMax: 0, gapMax: 0, transitMax: 0 };
    return line;
  }

  private configure(configPacket: Uint8Array) {
    const nals = splitNals(configPacket);
    const sps = nals.find((n) => (n[0] & 0x1f) === 7);
    const pps = nals.find((n) => (n[0] & 0x1f) === 8);
    if (!sps || !pps) {
      console.warn("config packet without SPS/PPS");
      return;
    }
    const config: VideoDecoderConfig = {
      codec: `avc1.${hex(sps[1])}${hex(sps[2])}${hex(sps[3])}`,
      description: avcC(sps, pps),
      optimizeForLatency: true,
    };
    if (!this.decoder || this.decoder.state === "closed") {
      this.decoder = new VideoDecoder({
        output: (frame) => this.draw(frame),
        error: (e) => this.fail(`decoder: ${e.message}`),
      });
    }
    try {
      this.decoder.configure(config);
      this.waitingForKey = true;
      this.decodeStart.clear();
      this.onConfig?.(Array.from(sps, hex).join(""), config.codec);
    } catch (e) {
      this.fail(`configure ${config.codec}: ${e}`);
    }
  }

  private draw(frame: VideoFrame) {
    const w = frame.displayWidth;
    const h = frame.displayHeight;
    if (this.canvas.width !== w || this.canvas.height !== h) {
      this.canvas.width = w;
      this.canvas.height = h;
      if (!this.width) this.onResize?.(w, h);
    }
    this.ctx.drawImage(frame, 0, 0);
    const started = this.decodeStart.get(frame.timestamp);
    if (started !== undefined) {
      const ms = performance.now() - started;
      this.decodeStart.delete(frame.timestamp);
      this.stats.decoded++;
      this.stats.decodeSum += ms;
      this.stats.decodeMax = Math.max(this.stats.decodeMax, ms);
    }
    frame.close();
    this.frameCount++;
  }

  private fail(reason: string) {
    console.error(reason);
    this.decoder = null; // a new one is created on the next config packet
    this.waitingForKey = true;
    this.onNeedKeyFrame?.(reason);
  }

  reset() {
    try {
      if (this.decoder && this.decoder.state !== "closed") this.decoder.close();
    } catch {
      /* already closed */
    }
    this.decoder = null;
    this.waitingForKey = true;
    this.width = 0;
    this.height = 0;
    this.baselineSkew = Infinity;
    this.delayMs = 0;
    this.decodeStart.clear();
    this.lastArrival = 0;
    this.ctx.fillStyle = "#000";
    this.ctx.fillRect(0, 0, this.canvas.width, this.canvas.height);
  }

  dispose() {
    this.reset();
    clearInterval(this.fpsTimer);
  }
}
