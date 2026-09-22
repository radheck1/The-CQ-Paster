/**
 * The "CQ is listening" mark that sits beside the pointer while dictation is
 * recording (macOS only).
 *
 * Seven bars that rise and fall with the microphone's actual level, so it
 * shows not just that dictation is on but that it can hear you. A bar that
 * animated on its own would look the same whether the microphone was working
 * or muted, which is the one thing this is here to tell you.
 *
 * The window is click-through and never becomes key — both enforced in Rust,
 * since dictation pastes into whatever was focused when you started talking.
 */
import { listen } from "@tauri-apps/api/event";

/** Five rather than seven: at this size more bars read as a smudge. */
const BARS = 5;
/** Fractions of full height; the middle bars lead so it reads as a voice. */
const SHAPE = [0.55, 0.85, 1.0, 0.85, 0.55];
/** Always visible, so the mark reads as "listening" even in silence. With no
 *  pill behind them the bars are the whole mark, so the floor is higher than
 *  it was — at 0.14 of 12px there would be almost nothing on screen. */
const FLOOR = 0.22;
/** How fast a bar falls back. Rising is immediate so speech looks responsive. */
const DECAY = 0.82;

let bars: HTMLElement[] = [];
const heights = new Array(BARS).fill(FLOOR);
let level = 0;

function frame() {
  for (let i = 0; i < BARS; i++) {
    // Each bar gets its own slice of the level, with a little variation so
    // they do not move as one block.
    const wobble = 0.82 + 0.36 * Math.abs(Math.sin(Date.now() / 150 + i * 1.7));
    const target = Math.max(FLOOR, level * SHAPE[i] * wobble);
    // Snap up, ease down: a meter that eased both ways would lag behind the
    // start of every word.
    heights[i] = target > heights[i] ? target : heights[i] * DECAY + target * (1 - DECAY);
    bars[i].style.transform = `scaleY(${Math.max(FLOOR, Math.min(1, heights[i])).toFixed(3)})`;
  }
  requestAnimationFrame(frame);
}

export async function start(app: HTMLElement) {
  app.innerHTML = `
    <div class="dl-wrap" aria-label="CQ is listening">
      ${Array.from({ length: BARS }, () => `<span class="dl-bar"></span>`).join("")}
    </div>`;
  bars = Array.from(app.querySelectorAll<HTMLElement>(".dl-bar"));
  await listen<number>("dictate-level", ({ payload }) => {
    level = typeof payload === "number" && payload >= 0 ? Math.min(1, payload) : 0;
  });
  requestAnimationFrame(frame);
}
