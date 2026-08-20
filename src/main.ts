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
type FolderDto = {
  id: number;
  name: string;
  filled: number;
  active: boolean;
  /** The home folder — can't be renamed or deleted. */
  permanent: boolean;
};
type StateDto = {
  mode: "master" | "noob";
  slots: SlotDto[];
  folders: FolderDto[];
  activeFolder: number;
  folderName: string;
};

const label = getCurrentWindow().label;
const app = document.getElementById("app")!;

/**
 * macOS gets the system traffic lights and ⌘; Windows keeps its own title bar
 * buttons and Ctrl. Detected here rather than passed down from Rust so the
 * shipping Windows build's backend is untouched — on Windows every string below
 * renders exactly as it did before.
 */
const IS_MAC = navigator.userAgent.includes("Mac");
/** The trigger key, as the user should see it written. */
const MOD = IS_MAC ? "⌘" : "Ctrl";
// Already set by the inline script in index.html, which has to run before the
// first paint. Repeated here only so the attribute still lands if that script
// is ever removed; it is idempotent.
document.documentElement.dataset.platform = IS_MAC ? "macos" : "other";

// Undo-after-clear: how long the Undo button stays offered, and its state.
const UNDO_MS = 10000;

/** Full slot text, fetched on hover and cached until the state changes. */
const fullText = new Map<number, string>();
const svg = (body: string, size = 13) =>
  `<svg viewBox="0 0 24 24" width="${size}" height="${size}" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${body}</svg>`;

const UNDO_ICON = svg(`<polyline points="1 4 1 10 7 10"/><path d="M3.51 15a9 9 0 1 0 2.13-9.36L1 10"/>`);
const FOLDER_ICON = svg(
  `<path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/>`,
);
const CHECK_ICON = svg(`<polyline points="20 6 9 17 4 12"/>`, 12);
const PENCIL_ICON = svg(
  `<path d="M12 20h9"/><path d="M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4z"/>`,
  12,
);
const X_ICON = svg(`<line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/>`, 12);
const PLUS_ICON = svg(`<line x1="12" y1="5" x2="12" y2="19"/><line x1="5" y1="12" x2="19" y2="12"/>`, 12);

let undoUntil = 0;
let undoTimer: number | undefined;

// ---- Folder dropdown state (main window only) ----
type Editing = { kind: "create" } | { kind: "rename"; id: number };
let menuOpen = false;
let editing: Editing | null = null;
let confirmDelete: number | null = null;
/** Set when a state update arrives mid-edit; applied once the edit finishes. */
let deferred: StateDto | null = null;

const MAX_NAME = 24;

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
        <span class="popup-folder" title="Active folder">${FOLDER_ICON}${escapeHtml(
          state.folderName,
        )}</span>
      </div>
      <ul class="popup-list">${rows}</ul>
      <div class="popup-foot">${MOD}+N+C copy · ${MOD}+N+V paste · +Shift = plain text</div>
    </div>`;
}

// ---------------------------------------------------------------------------
// Folder picker: a pill showing the active folder, opening a dropdown of all
// folders plus a permanent "create" row.
// ---------------------------------------------------------------------------
function folderControl(state: StateDto): string {
  const rows = state.folders
    .map((f) => {
      if (editing?.kind === "rename" && editing.id === f.id) {
        return `<li class="folder-row editing">
          <input class="fr-input" id="fr-input" type="text" maxlength="${MAX_NAME}"
                 value="${escapeHtml(f.name)}" aria-label="Rename folder" />
        </li>`;
      }
      if (confirmDelete === f.id) {
        return `<li class="folder-row confirming">
          <span class="fr-confirm">Delete “${escapeHtml(f.name)}”?</span>
          <button class="fr-yes" data-yes="${f.id}">Delete</button>
          <button class="fr-no" data-no="${f.id}">Cancel</button>
        </li>`;
      }
      // The home folder is fixed: no rename, no delete. Its row keeps showing
      // the fill count on hover, since there are no actions to swap in.
      const cls = [f.active ? "active" : "", f.permanent ? "no-actions" : ""]
        .filter(Boolean)
        .join(" ");
      const actions = f.permanent
        ? ""
        : `<span class="fr-actions">
             <button class="fr-btn" data-rename="${f.id}" title="Rename">${PENCIL_ICON}</button>
             <button class="fr-btn fr-del" data-del="${f.id}" title="Delete folder">${X_ICON}</button>
           </span>`;
      return `<li class="folder-row ${cls}" data-select="${f.id}"
                  title="${
                    f.permanent
                      ? "Home folder — always here"
                      : `Switch to ${escapeHtml(f.name)}`
                  }">
        <span class="fr-check">${f.active ? CHECK_ICON : ""}</span>
        <span class="fr-name">${escapeHtml(f.name)}</span>
        <span class="fr-tail">
          <span class="fr-count">${f.filled}/9</span>
          ${actions}
        </span>
      </li>`;
    })
    .join("");

  const newRow =
    editing?.kind === "create"
      ? `<div class="folder-new editing">
           <input class="fr-input" id="fr-input" type="text" maxlength="${MAX_NAME}"
                  placeholder="Folder name" aria-label="New folder name" />
         </div>`
      : `<button class="folder-new" id="folder-new">${PLUS_ICON} Create new folder</button>`;

  return `
    <div class="folder-wrap">
      <button class="folder-pill" id="folder-btn" aria-haspopup="true" aria-expanded="${menuOpen}"
              title="Folder — each has its own 9 slots">
        <span class="fp-name">${escapeHtml(state.folderName)}</span>
        ${FOLDER_ICON}
      </button>
      <div class="folder-menu"${menuOpen ? "" : " hidden"}>
        <ul class="folder-list">${rows}</ul>
        ${newRow}
      </div>
    </div>`;
}

/** Close the dropdown and drop any in-progress edit/confirm. */
function closeMenu() {
  menuOpen = false;
  editing = null;
  confirmDelete = null;
  redraw();
}

/** Finish an edit, applying any state update that arrived while it was open. */
function endEdit() {
  editing = null;
  if (deferred) {
    latest = deferred;
    deferred = null;
  }
  redraw();
}

function commitEdit(value: string) {
  const name = value.trim().slice(0, MAX_NAME);
  const was = editing;
  editing = null;
  if (name && was) {
    if (was.kind === "create") {
      menuOpen = false;
      invoke("create_folder", { name });
    } else {
      invoke("rename_folder", { id: was.id, name });
    }
  }
  endEdit();
}

function wireFolderControl(state: StateDto) {
  const q = <T extends HTMLElement>(sel: string) => app.querySelector<T>(sel);

  q<HTMLButtonElement>("#folder-btn")?.addEventListener("click", () => {
    menuOpen = !menuOpen;
    editing = null;
    confirmDelete = null;
    redraw();
  });

  app.querySelectorAll<HTMLElement>("[data-select]").forEach((row) => {
    row.addEventListener("click", (e) => {
      if ((e.target as HTMLElement).closest("button")) return; // rename / delete
      const id = Number(row.dataset.select);
      menuOpen = false;
      confirmDelete = null;
      if (id !== state.activeFolder) invoke("select_folder", { id });
      redraw();
    });
  });

  app.querySelectorAll<HTMLButtonElement>("[data-rename]").forEach((btn) => {
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      confirmDelete = null;
      editing = { kind: "rename", id: Number(btn.dataset.rename) };
      redraw();
    });
  });

  app.querySelectorAll<HTMLButtonElement>("[data-del]").forEach((btn) => {
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      editing = null;
      confirmDelete = Number(btn.dataset.del);
      redraw();
    });
  });

  app.querySelectorAll<HTMLButtonElement>("[data-yes]").forEach((btn) => {
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      confirmDelete = null;
      invoke("delete_folder", { id: Number(btn.dataset.yes) });
      redraw();
    });
  });

  app.querySelectorAll<HTMLButtonElement>("[data-no]").forEach((btn) => {
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      confirmDelete = null;
      redraw();
    });
  });

  q<HTMLButtonElement>("#folder-new")?.addEventListener("click", () => {
    confirmDelete = null;
    editing = { kind: "create" };
    redraw();
  });

  const input = q<HTMLInputElement>("#fr-input");
  if (input) {
    input.focus();
    input.select();
    input.addEventListener("keydown", (e) => {
      if (e.key === "Enter") {
        e.preventDefault();
        commitEdit(input.value);
      } else if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation(); // don't also close the menu
        endEdit();
      }
    });
    // Clicking away commits a non-empty name rather than silently discarding it.
    input.addEventListener("blur", () => {
      if (editing) commitEdit(input.value);
    });
  }
}

/**
 * Hover a filled slot to read the whole item.
 *
 * The stored preview is capped at 240 characters, because it is persisted into
 * `folders.bin` for every slot of every folder. So the full text is fetched on
 * demand the first time a row is hovered and cached until the state changes —
 * nothing extra is written to disk, and nothing crosses the IPC boundary until
 * someone actually looks.
 */
function wireSlotScrolling(root: HTMLElement) {
  root.querySelectorAll<HTMLElement>("[data-desc]").forEach((desc) => {
    const index = Number(desc.dataset.desc);

    desc.addEventListener("mouseenter", async () => {
      if (fullText.has(index)) {
        applyFullText(desc, fullText.get(index)!);
        return;
      }
      const text = await invoke<string | null>("slot_text", { index });
      if (text == null) return; // an image or file list: nothing to expand
      fullText.set(index, text);
      // The pointer may have moved on while that was in flight.
      if (desc.matches(":hover")) applyFullText(desc, text);
    });

    // A mouse wheel only gives vertical deltas, so translate them — but only
    // while there is somewhere left to scroll. At either end the event is left
    // alone so it bubbles and scrolls the slot list, which is the behaviour you
    // want when a wheel passes over a long row on the way down the panel.
    desc.addEventListener(
      "wheel",
      (e: WheelEvent) => {
        if (e.deltaX !== 0) return; // trackpad already scrolling sideways
        const max = desc.scrollWidth - desc.clientWidth;
        if (max <= 0) return;
        const next = desc.scrollLeft + e.deltaY;
        if (next < 0 || next > max) return; // at an end — let the list have it
        desc.scrollLeft = next;
        e.preventDefault();
      },
      { passive: false },
    );
  });
}

/** Swap in the untruncated text and let the row scroll. */
function applyFullText(desc: HTMLElement, text: string) {
  if (desc.dataset.expanded === "1") return;
  desc.dataset.expanded = "1";
  desc.textContent = text;
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
        ? `<div class="s-desc" data-desc="${s.index}">${escapeHtml(describe(s.preview))}</div>
           <div class="s-kind">${s.preview!.kind} · ${fmtBytes(s.preview!.bytes)}</div>`
        : `<div class="s-desc empty">empty... <b>${MOD}+${s.index}+C</b> to fill</div>`;
      const clearBtn = filled
        ? `<button class="s-clear" data-clear="${s.index}" title="Clear slot ${s.index}">✕</button>`
        : "";
      const copiedOverlay = filled
        ? `<div class="s-copied">✓ Copied — press ${MOD}+V to paste</div>`
        : "";
      return `
        <div class="slot ${filled ? "filled" : ""}" data-index="${s.index}"${
          filled ? ` title="Click to copy slot ${s.index} to the clipboard"` : ""
        }>
          <span class="s-dot" aria-hidden="true"></span>
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
    <div class="panel" data-mode="${state.mode}">
      <header class="panel-head">
        ${folderControl(state)}
        <div class="head-right">
          <div class="mode-switch" role="group" aria-label="Mode">
            <button class="mode ${!isNoob ? "active" : ""}" data-mode="master">Master &gt;:)</button>
            <button class="mode ${isNoob ? "active" : ""}" data-mode="noob">Noob :)</button>
          </div>
          <div class="info" tabindex="0" role="button" aria-label="Shortcuts and help">
            <span class="info-q" aria-hidden="true">?</span>
            <div class="info-pop" role="tooltip">
              <b>${MOD}+&lt;N&gt;+C</b> copies into slot N<br />
              <b>${MOD}+&lt;N&gt;+V</b> pastes it (add <b>Shift</b> to paste as plain text)<br />
              Plain ${MOD}+C / ${MOD}+V still work normally.<br />
              <b>Click any slot</b> to load it onto the clipboard, then paste with ${MOD}+V.
              <br /><br />
              <b>Folders</b> each hold their own 9 slots — hotkeys, Clear all and Undo
              apply only to the folder you're in.
              <br /><br />
              <b>Noob</b> shows a popup by your cursor; <b>Master</b> is fully invisible.
            </div>
          </div>
        </div>
      </header>

      <div class="slots">${slotRows}</div>

      <footer class="panel-foot">
        <button class="ghost" id="clear-all"${hasFilled ? "" : " disabled"}
          title="Clear the 9 slots in “${escapeHtml(state.folderName)}” — other folders are untouched">Clear all</button>
        ${
          Date.now() < undoUntil
            ? `<button class="ghost undo" id="undo-clear" title="Restore the cleared slots">${UNDO_ICON} Undo</button>`
            : ""
        }
        <span class="spacer"></span>
        <span class="tip">You can close this window,<br />CQ Paster runs in the background</span>
      </footer>
    </div>`;

  wireSlotScrolling(app);

  const win = getCurrentWindow();
  app.querySelector<HTMLButtonElement>("#tb-min")?.addEventListener("click", () => {
    win.minimize();
  });
  app.querySelector<HTMLButtonElement>("#tb-close")?.addEventListener("click", () => {
    win.hide(); // keep running in the tray
  });
  app.querySelectorAll<HTMLButtonElement>(".mode").forEach((btn) => {
    btn.addEventListener("click", () => {
      const mode = btn.dataset.mode as "master" | "noob";
      invoke("set_mode", { mode });
      // Toggle active in place so the colors cross-fade — a full re-render would
      // replace the buttons and skip the CSS transition.
      app.querySelectorAll<HTMLElement>(".mode").forEach((b) => {
        b.classList.toggle("active", b === btn);
      });
      // Drives the slot dots, which take their hue from the mode.
      app.querySelector<HTMLElement>(".panel")?.setAttribute("data-mode", mode);
      // `set_mode` deliberately emits no event, so keep the cached state in step
      // — otherwise the next redraw (e.g. opening the folder menu) would snap
      // the toggle and the dots back to the old mode.
      if (latest) latest.mode = mode;
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

  wireFolderControl(state);
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

/** Re-render the control panel from the last known state. */
function redraw() {
  if (latest && label !== "popup") {
    renderMain(latest);
    fitMainWindow();
  }
}

function render(state: StateDto) {
  if (label === "popup") {
    latest = state;
    renderPopup(state);
    return;
  }
  // A re-render replaces the DOM, which would destroy a folder name the user is
  // halfway through typing. Hold the update until the edit finishes.
  if (editing) {
    deferred = state;
    return;
  }
  latest = state;
  renderMain(state);
  fitMainWindow();
}

async function boot() {
  document.body.dataset.window = label;
  // Re-fit the control panel whenever it's opened/focused, so it can't flash at
  // the initial config size before the content measurement settles.
  if (label === "main") {
    getCurrentWindow().onFocusChanged(({ payload: focused }) => {
      if (focused) fitMainWindow();
    });
    // Dismiss the folder dropdown on an outside click or Escape. Registered
    // once, on the document, so re-renders don't stack duplicate listeners.
    document.addEventListener("mousedown", (e) => {
      if (!menuOpen) return;
      if ((e.target as HTMLElement).closest(".folder-wrap")) return;
      closeMenu();
    });
    document.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && menuOpen) closeMenu();
    });
  }
  try {
    const state = await invoke<StateDto>("get_state");
    render(state);
    dropInitialFocus();
  } catch (e) {
    console.error("get_state failed", e);
  }
  await listen<StateDto>("state-updated", (ev) => {
    // A slot may have been refilled, so any expanded text is stale.
    fullText.clear();
    render(ev.payload);
  });
}

/**
 * Leave nothing focused when a window opens.
 *
 * The webview hands focus to the first control it finds, so a freshly opened
 * control panel drew a focus ring around the folder pill — a selection the user
 * never made. `:focus-visible` does not suppress it: that focus arrives with no
 * pointer event before it, so the browser's heuristic reasonably calls it
 * keyboard-driven and draws the ring.
 *
 * Blurring once, after the first paint, is the honest fix — the window opens
 * with nothing focused, exactly as if the webview had not intervened. Tab still
 * works normally from there, and this never runs again, so it cannot steal
 * focus from someone typing a folder name.
 */
function dropInitialFocus() {
  requestAnimationFrame(() => {
    const el = document.activeElement as HTMLElement | null;
    if (el && el !== document.body) el.blur();
  });
}

boot();

// Keep a reference so bundlers don't tree-shake `latest`.
export { latest };
