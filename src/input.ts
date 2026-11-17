// Mouse / keyboard -> scrcpy control messages (sent through Rust commands).

import { invoke } from "@tauri-apps/api/core";
import type { Player } from "./player";

const ACTION_DOWN = 0;
const ACTION_UP = 1;
const ACTION_MOVE = 2;

// Android KeyEvent keycodes
export const KEYCODE = {
  HOME: 3,
  BACK: 4,
  VOLUME_UP: 24,
  VOLUME_DOWN: 25,
  POWER: 26,
  APP_SWITCH: 187,
} as const;

const SPECIAL_KEYS: Record<string, number> = {
  Enter: 66,
  NumpadEnter: 66,
  Backspace: 67,
  Delete: 112,
  Tab: 61,
  Escape: 111,
  ArrowUp: 19,
  ArrowDown: 20,
  ArrowLeft: 21,
  ArrowRight: 22,
  Home: 122,
  End: 123,
  PageUp: 92,
  PageDown: 93,
};

// Android meta state flags
const META_SHIFT = 0x1 | 0x40;
const META_ALT = 0x2 | 0x10;
const META_CTRL = 0x1000 | 0x2000;

/** KeyA..KeyZ, Digit0..9, Space -> Android keycode (used with Ctrl). */
function physicalKeycode(code: string): number | undefined {
  if (/^Key[A-Z]$/.test(code)) return 29 + code.charCodeAt(3) - 65;
  if (/^Digit[0-9]$/.test(code)) return 7 + Number(code[5]);
  if (code === "Space") return 62;
  return undefined;
}

function metaState(e: KeyboardEvent): number {
  return (e.shiftKey ? META_SHIFT : 0) | (e.altKey ? META_ALT : 0) | (e.ctrlKey ? META_CTRL : 0);
}

function report(promise: Promise<unknown>) {
  promise.catch((e) => console.warn(e));
}

export class InputHandler {
  enabled = false;
  private pointerDown = false;
  /** Keycodes sent as "down" that still need an "up". */
  private heldKeys = new Map<string, number>();

  constructor(
    private canvas: HTMLCanvasElement,
    private player: Player,
  ) {
    canvas.addEventListener("pointerdown", (e) => this.onPointerDown(e));
    canvas.addEventListener("pointermove", (e) => this.onPointerMove(e));
    canvas.addEventListener("pointerup", (e) => this.onPointerUp(e));
    canvas.addEventListener("pointercancel", (e) => this.onPointerUp(e));
    canvas.addEventListener("wheel", (e) => this.onWheel(e), { passive: false });
    canvas.addEventListener("contextmenu", (e) => e.preventDefault());
    window.addEventListener("keydown", (e) => this.onKeyDown(e));
    window.addEventListener("keyup", (e) => this.onKeyUp(e));
    window.addEventListener("blur", () => this.releaseAll());
  }

  /** Map client coordinates to device video coordinates. */
  private position(e: MouseEvent) {
    const rect = this.canvas.getBoundingClientRect();
    const w = this.player.width;
    const h = this.player.height;
    const x = Math.round(((e.clientX - rect.left) / rect.width) * w);
    const y = Math.round(((e.clientY - rect.top) / rect.height) * h);
    return {
      x: Math.min(Math.max(x, 0), w - 1),
      y: Math.min(Math.max(y, 0), h - 1),
      w,
      h,
    };
  }

  private ready() {
    return this.enabled && this.player.width > 0 && this.player.height > 0;
  }

  private onPointerDown(e: PointerEvent) {
    if (!this.ready()) return;
    e.preventDefault();
    this.canvas.focus();
    if (e.button === 2) {
      report(invoke("press_key", { keycode: KEYCODE.BACK })); // right click = Back
      return;
    }
    if (e.button === 1) {
      report(invoke("press_key", { keycode: KEYCODE.HOME })); // middle click = Home
      return;
    }
    if (e.button !== 0) return;
    this.pointerDown = true;
    this.canvas.setPointerCapture(e.pointerId);
    report(invoke("touch", { action: ACTION_DOWN, ...this.position(e) }));
  }

  private onPointerMove(e: PointerEvent) {
    if (!this.pointerDown || !this.ready()) return;
    // Coalesced events give smoother swipes on fast trackpads.
    const events = e.getCoalescedEvents?.() ?? [e];
    for (const ev of events.length ? events : [e]) {
      report(invoke("touch", { action: ACTION_MOVE, ...this.position(ev) }));
    }
  }

  private onPointerUp(e: PointerEvent) {
    if (!this.pointerDown) return;
    this.pointerDown = false;
    if (this.canvas.hasPointerCapture(e.pointerId)) this.canvas.releasePointerCapture(e.pointerId);
    if (!this.ready()) return;
    report(invoke("touch", { action: ACTION_UP, ...this.position(e) }));
  }

  private onWheel(e: WheelEvent) {
    if (!this.ready()) return;
    e.preventDefault();
    // Pixels -> "notches". One mouse-wheel notch is ~100px in WebKit.
    const unit = e.deltaMode === WheelEvent.DOM_DELTA_LINE ? 3 : e.deltaMode === WheelEvent.DOM_DELTA_PAGE ? 0.1 : 100;
    const clamp = (v: number) => Math.max(-16, Math.min(16, v));
    report(
      invoke("scroll", {
        ...this.position(e),
        hscroll: clamp(-e.deltaX / unit),
        vscroll: clamp(-e.deltaY / unit),
      }),
    );
  }

  private isUiTarget(e: KeyboardEvent) {
    const t = e.target as HTMLElement | null;
    return !!t && (t.tagName === "INPUT" || t.tagName === "SELECT" || t.tagName === "TEXTAREA");
  }

  private onKeyDown(e: KeyboardEvent) {
    if (!this.enabled || this.isUiTarget(e) || e.isComposing) return;

    if (e.metaKey) {
      // Cmd shortcuts: clipboard bridging; leave the rest to macOS (Cmd+Q, Cmd+W...).
      if (e.code === "KeyV") {
        e.preventDefault();
        report(invoke("paste_from_mac"));
      } else if (e.code === "KeyC") {
        e.preventDefault();
        report(invoke("copy_to_mac"));
      }
      return;
    }

    const special = SPECIAL_KEYS[e.code];
    const keycode = special ?? (e.ctrlKey ? physicalKeycode(e.code) : undefined);
    if (keycode !== undefined) {
      e.preventDefault();
      this.heldKeys.set(e.code, keycode);
      report(invoke("key", { action: ACTION_DOWN, keycode, repeat: e.repeat ? 1 : 0, meta: metaState(e) }));
      return;
    }

    // Printable characters (including Option+key characters on macOS) go as text.
    if ([...e.key].length === 1 && !e.ctrlKey) {
      e.preventDefault();
      report(invoke("inject_text", { text: e.key }));
    }
  }

  private onKeyUp(e: KeyboardEvent) {
    const keycode = this.heldKeys.get(e.code);
    if (keycode === undefined) return;
    this.heldKeys.delete(e.code);
    if (!this.enabled) return;
    e.preventDefault();
    report(invoke("key", { action: ACTION_UP, keycode, repeat: 0, meta: metaState(e) }));
  }

  private releaseAll() {
    if (this.enabled) {
      for (const keycode of this.heldKeys.values()) {
        report(invoke("key", { action: ACTION_UP, keycode, repeat: 0, meta: 0 }));
      }
    }
    this.heldKeys.clear();
    this.pointerDown = false;
  }
}
