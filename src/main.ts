import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow, LogicalSize } from "@tauri-apps/api/window";

type Preview = {
  kind: "text" | "image" | "files" | "other";
  text?: string;
  files: string[];
  bytes: number;
  width?: number;
  height?: number;
};

type SlotDto = { index: number; filled: boolean; preview?: Preview };
type StateDto = { mode: "master" | "noob"; slots: SlotDto[] };

const label = getCurrentWindow().label;
const app = document.getElementById("app")!;

// Undo-after-clear: how long the Undo button stays offered, and its state.
const UNDO_MS = 10000;
const UNDO_ICON = `<svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><polyline points="1 4 1 10 7 10"/><path d="M3.51 15a9 9 0 1 0 2.13-9.36L1 10"/></svg>`;
let undoUntil = 0;
let undoTimer: number | undefined;

function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

function iconFor(kind: string): string {
  switch (kind) {
    case "text":
      return "¶";
    case "image":
      return "▦";
    case "files":
      return "🗎";
    default:
      return "◆";
  }
}

function describe(p?: Preview): string {
  if (!p) return "";
  if (p.kind === "text") return (p.text ?? "").trim() || "(empty text)";
  if (p.kind === "files") {
    if (p.files.length === 1) return p.files[0];
    return `${p.files.length} files — ${p.files
      .map((f) => f.split(/[\\/]/).pop())
      .join(", ")}`;
  }
  if (p.kind === "image") {
    const dim = p.width && p.height ? `${p.width}×${p.height}` : "";
    return `Image ${dim} (${fmtBytes(p.bytes)})`.trim();
  }
  return `Data (${fmtBytes(p.bytes)})`;
}

// ---------------------------------------------------------------------------
// Popup view: a compact reference list shown next to the cursor (Noob mode).
// ---------------------------------------------------------------------------
function renderPopup(state: StateDto) {
  const rows = state.slots
    .map((s) => {
      const filled = s.filled;
      const body = filled
        ? `<span class="p-icon">${iconFor(s.preview!.kind)}</span><span class="p-text">${escapeHtml(
            describe(s.preview),
          )}</span>`
        : `<span class="p-icon empty">-</span><span class="p-text empty">empty</span>`;
      return `<li class="${filled ? "" : "is-empty"}"><span class="p-num">${s.index}</span>${body}</li>`;
    })
    .join("");

  app.innerHTML = `
    <div class="popup">
      <div class="popup-head">
        <img class="popup-logo theme-logo for-dark" src="/logo-white.png" alt="" />
        <img class="popup-logo theme-logo for-light" src="/logo-black.png" alt="" />
        <span>Paster</span>
      </div>
      <ul class="popup-list">${rows}</ul>
      <div class="popup-foot">Ctrl+N+C copy · Ctrl+N+V paste · +Shift = plain text</div>
    </div>`;
}

// ---------------------------------------------------------------------------
// Main view: the control panel.
// ---------------------------------------------------------------------------
function renderMain(state: StateDto) {
  const isNoob = state.mode === "noob";
  const hasFilled = state.slots.some((s) => s.filled);
  const slotRows = state.slots
    .map((s) => {
      const filled = s.filled;
      const meta = filled
        ? `<div class="s-desc">${escapeHtml(describe(s.preview))}</div>
           <div class="s-kind">${s.preview!.kind} · ${fmtBytes(s.preview!.bytes)}</div>`
        : `<div class="s-desc empty">empty... <b>Ctrl+${s.index}+C</b> to fill</div>`;
      const clearBtn = filled
        ? `<button class="s-clear" data-clear="${s.index}" title="Clear slot ${s.index}">✕</button>`
        : "";
      const copiedOverlay = filled
        ? `<div class="s-copied">✓ Copied — press Ctrl+V to paste</div>`
        : "";
      return `
        <div class="slot ${filled ? "filled" : ""}" data-index="${s.index}"${
          filled ? ` title="Click to copy slot ${s.index} to the clipboard"` : ""
        }>
          <div class="s-num">${s.index}</div>
          <div class="s-body">${meta}</div>
          ${clearBtn}
          ${copiedOverlay}
        </div>`;
    })
    .join("");

  app.innerHTML = `
    <div class="titlebar" data-tauri-drag-region>
      <div class="titlebar-brand">
        <img class="titlebar-logo" src="/logo-white.png" alt="" />
        <span>Paster</span>
      </div>
      <div class="titlebar-controls">
        <button class="tb-btn" id="tb-min" title="Minimize" aria-label="Minimize">
          <svg viewBox="0 0 12 12" aria-hidden="true"><rect x="1.5" y="6" width="9" height="1.1" fill="currentColor"/></svg>
        </button>
        <button class="tb-btn tb-close" id="tb-close" title="Close" aria-label="Close">
          <svg viewBox="0 0 12 12" aria-hidden="true"><path d="M2 2 L10 10 M10 2 L2 10" stroke="currentColor" stroke-width="1.2" stroke-linecap="round"/></svg>
        </button>
      </div>
    </div>
    <div class="panel">
      <header class="panel-head">
        <div class="mode-switch" role="group" aria-label="Mode">
          <button class="mode ${!isNoob ? "active" : ""}" data-mode="master">Master &gt;:)</button>
          <button class="mode ${isNoob ? "active" : ""}" data-mode="noob">Noob :)</button>
        </div>
        <div class="info" tabindex="0" role="button" aria-label="Shortcuts and help">
          <span class="info-q" aria-hidden="true">?</span>
          <div class="info-pop" role="tooltip">
            <b>Ctrl+&lt;N&gt;+C</b> copies into slot N<br />
            <b>Ctrl+&lt;N&gt;+V</b> pastes it (add <b>Shift</b> to paste as plain text)<br />
            Plain Ctrl+C / Ctrl+V still work normally.<br />
            <b>Click any slot</b> to load it onto the clipboard, then paste with Ctrl+V.
            <br /><br />
            <b>Noob</b> shows a popup by your cursor; <b>Master</b> is fully invisible.
          </div>
        </div>
      </header>

      <div class="slots">${slotRows}</div>

      <footer class="panel-foot">
        <button class="ghost" id="clear-all"${hasFilled ? "" : " disabled"}>Clear all</button>
        ${
          Date.now() < undoUntil
            ? `<button class="ghost undo" id="undo-clear" title="Restore the cleared slots">${UNDO_ICON} Undo</button>`
            : ""
        }
        <span class="spacer"></span>
        <span class="tip">You can close this window,<br />CQ Paster runs in the background</span>
      </footer>
    </div>`;

  const win = getCurrentWindow();
  app.querySelector<HTMLButtonElement>("#tb-min")?.addEventListener("click", () => {
    win.minimize();
  });
  app.querySelector<HTMLButtonElement>("#tb-close")?.addEventListener("click", () => {
    win.hide(); // keep running in the tray
  });
  app.querySelectorAll<HTMLButtonElement>(".mode").forEach((btn) => {
    btn.addEventListener("click", () => {
      invoke("set_mode", { mode: btn.dataset.mode });
      // Toggle active in place so the colors cross-fade — a full re-render would
      // replace the buttons and skip the CSS transition.
      app.querySelectorAll<HTMLElement>(".mode").forEach((b) => {
        b.classList.toggle("active", b === btn);
      });
    });
  });
  app.querySelectorAll<HTMLButtonElement>(".s-clear").forEach((btn) => {
    btn.addEventListener("click", (e) => {
      e.stopPropagation(); // don't also trigger the slot's copy handler
      invoke("clear_slot", { index: Number(btn.dataset.clear) });
    });
  });
  app.querySelectorAll<HTMLElement>(".slot.filled").forEach((el) => {
    el.addEventListener("click", (e) => {
      if ((e.target as HTMLElement).closest(".s-clear")) return;
      const index = Number(el.dataset.index);
      invoke<boolean>("copy_slot", { index }).then((ok) => {
        if (!ok) return;
        el.classList.add("copied");
        window.setTimeout(() => el.classList.remove("copied"), 1300);
      });
    });
  });
  app.querySelector<HTMLButtonElement>("#clear-all")?.addEventListener("click", () => {
    invoke("clear_all");
    // Offer a 10-second window to undo the clear.
    undoUntil = Date.now() + UNDO_MS;
    if (undoTimer) clearTimeout(undoTimer);
    undoTimer = window.setTimeout(() => {
      undoUntil = 0;
      if (latest) render(latest); // re-render to drop the Undo button
    }, UNDO_MS);
  });
  app.querySelector<HTMLButtonElement>("#undo-clear")?.addEventListener("click", () => {
    undoUntil = 0;
    if (undoTimer) clearTimeout(undoTimer);
    invoke("undo_clear"); // restores slots and emits state-updated → re-render
  });
}

function escapeHtml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

let latest: StateDto | null = null;
const PANEL_WIDTH = 442;

/** Size the control-panel window to exactly fit its content — no bottom gap,
 *  no scrollbar — and re-fit as slots fill (filled slots are a touch taller). */
function fitMainWindow() {
  requestAnimationFrame(() => {
    const h = Math.ceil(document.body.getBoundingClientRect().height);
    if (h > 0) {
      getCurrentWindow()
        .setSize(new LogicalSize(PANEL_WIDTH, h))
        .catch(() => {});
    }
  });
}

function render(state: StateDto) {
  latest = state;
  if (label === "popup") {
    renderPopup(state);
  } else {
    renderMain(state);
    fitMainWindow();
  }
}

async function boot() {
  document.body.dataset.window = label;
  // Re-fit the control panel whenever it's opened/focused, so it can't flash at
  // the initial config size before the content measurement settles.
  if (label === "main") {
    getCurrentWindow().onFocusChanged(({ payload: focused }) => {
      if (focused) fitMainWindow();
    });
  }
  try {
    const state = await invoke<StateDto>("get_state");
    render(state);
  } catch (e) {
    console.error("get_state failed", e);
  }
  await listen<StateDto>("state-updated", (ev) => render(ev.payload));
}

boot();

// Keep a reference so bundlers don't tree-shake `latest`.
export { latest };
