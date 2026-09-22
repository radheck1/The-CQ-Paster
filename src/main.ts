import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow, LogicalSize } from "@tauri-apps/api/window";
import * as jotter from "./jotter";
import * as reminders from "./reminders";
import * as shotter from "./shotter";
import * as markup from "./markup";
import * as dictate from "./dictate";

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
/** Slot thumbnails as data URLs, fetched on render and cached the same way. */
const thumbs = new Map<number, string>();
const svg = (body: string, size = 13) =>
  `<svg viewBox="0 0 24 24" width="${size}" height="${size}" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${body}</svg>`;

const UNDO_ICON = svg(`<polyline points="1 4 1 10 7 10"/><path d="M3.51 15a9 9 0 1 0 2.13-9.36L1 10"/>`);
const FOLDER_ICON = svg(
  `<path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/>`,
);
/** Jotter's jotpads: a notepad with two rings and two lines of writing. */
const JOTPAD_ICON = svg(`<rect x="5" y="4" width="14" height="18" rx="2"/><path d="M9 2v4M15 2v4M9 11h6M9 15h4"/>`);
/** The footer's reminder that closing the control panel doesn't quit. */
const BACKGROUND_TIP = IS_MAC
  ? "You can close this window.<br />cQ runs in the background."
  : "You can close this window,<br />CQ Paster runs in the background";
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

/**
 * Which tool the control panel is showing: Paster, Jotter or Shotter. macOS
 * only: on Windows it stays "paster", and every code path below behaves exactly
 * as it did before.
 */
let view: jotter.View = "paster";

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

/** Last path segment, handling both separators. */
function basename(path: string): string {
  return path.split(/[\\/]/).pop() || path;
}

/**
 * The colour a slot holds, if the copied text is exactly one hex colour.
 *
 * Whole-string only, and hex only, both deliberate. Matching inside longer text
 * would put a swatch beside any prose that happens to mention `#fff`, and bare
 * `255, 0, 0` is indistinguishable from an ordinary list of numbers.
 *
 * The return value is used in a `style` attribute, so the pattern is anchored
 * and allows only hex digits — never interpolate the raw copied text there.
 */
function hexColor(p?: Preview): string | null {
  if (!p || p.kind !== "text") return null;
  const t = (p.text ?? "").trim();
  return /^#(?:[0-9a-fA-F]{3,4}|[0-9a-fA-F]{6}|[0-9a-fA-F]{8})$/.test(t) ? t : null;
}

function describe(p?: Preview): string {
  if (!p) return "";
  if (p.kind === "text") return (p.text ?? "").trim() || "(empty text)";
  if (p.kind === "files") {
    // Name only, not the path. Reading left to right, a path front-loads
    // directories the user does not need and pushes the actual name out of
    // view. The multi-file branch below already did this; the single-file
    // branch did not, which is the inconsistency that showed up in use.
    if (p.files.length === 1) return basename(p.files[0]);
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

  // macOS: with more than one folder, ← and → switch between them while the
  // popup is up. Windows renders exactly as before.
  const cycle = IS_MAC && state.folders.length > 1;
  const folder = cycle
    ? `<span class="popup-chev" aria-hidden="true">‹</span>${FOLDER_ICON}<span class="popup-folder-name">${escapeHtml(
        state.folderName,
      )}</span><span class="popup-chev" aria-hidden="true">›</span>`
    : `${FOLDER_ICON}${escapeHtml(state.folderName)}`;
  const foot = IS_MAC
    ? `${MOD}+N+C copy · ${MOD}+N+V paste<br />+Shift = plain · ← → folder`
    : `${MOD}+N+C copy · ${MOD}+N+V paste · +Shift = plain text`;

  app.innerHTML = `
    <div class="popup">
      <div class="popup-head">
        <img class="popup-logo theme-logo for-dark" src="/logo-white.png" alt="" />
        <img class="popup-logo theme-logo for-light" src="/logo-black.png" alt="" />
        <span>Paster</span>
        <span class="popup-folder" title="Active folder">${folder}</span>
      </div>
      <ul class="popup-list">${rows}</ul>
      <div class="popup-foot">${foot}</div>
    </div>`;
}

// ---------------------------------------------------------------------------
// Folder picker: a pill showing the active folder, opening a dropdown of all
// folders plus a permanent "create" row.
// ---------------------------------------------------------------------------
/**
 * What the folder dropdown lists and does. Paster's folders live in the backend,
 * Jotter's (macOS) in `jotter.ts`; the dropdown itself is the same for both.
 */
type FolderSource = {
  folders: { id: number; name: string; active: boolean; permanent: boolean; count: string }[];
  activeId: number;
  activeName: string;
  pillTitle: string;
  /** What one is called in the menu: "folder", or "jotpad" in Jotter. */
  noun: string;
  /** The pill's icon. */
  icon: string;
  create(name: string): void;
  rename(id: number, name: string): void;
  select(id: number): void;
  remove(id: number): void;
  /** Runs when a name edit ends, just before the redraw. */
  afterEdit?(): void;
};

function pasterFolders(state: StateDto): FolderSource {
  return {
    folders: state.folders.map((f) => ({ ...f, count: `${f.filled}/9` })),
    activeId: state.activeFolder,
    activeName: state.folderName,
    pillTitle: "Folder — each has its own 9 slots",
    noun: "folder",
    icon: FOLDER_ICON,
    create: (name) => invoke("create_folder", { name }),
    rename: (id, name) => invoke("rename_folder", { id, name }),
    select: (id) => invoke("select_folder", { id }),
    remove: (id) => invoke("delete_folder", { id }),
    // A state update that arrived mid-edit was held back; apply it now.
    afterEdit: () => {
      if (deferred) {
        latest = deferred;
        deferred = null;
      }
    },
  };
}

const capital = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);

function folderControl(source: FolderSource): string {
  const rows = source.folders
    .map((f) => {
      if (editing?.kind === "rename" && editing.id === f.id) {
        return `<li class="folder-row editing">
          <input class="fr-input" id="fr-input" type="text" maxlength="${MAX_NAME}"
                 value="${escapeHtml(f.name)}" aria-label="Rename ${source.noun}" />
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
             <button class="fr-btn fr-del" data-del="${f.id}" title="Delete ${source.noun}">${X_ICON}</button>
           </span>`;
      return `<li class="folder-row ${cls}" data-select="${f.id}"
                  title="${
                    f.permanent
                      ? `Home ${source.noun} — always here`
                      : `Switch to ${escapeHtml(f.name)}`
                  }">
        <span class="fr-check">${f.active ? CHECK_ICON : ""}</span>
        <span class="fr-name">${escapeHtml(f.name)}</span>
        <span class="fr-tail">
          <span class="fr-count">${f.count}</span>
          ${actions}
        </span>
      </li>`;
    })
    .join("");

  const newRow =
    editing?.kind === "create"
      ? `<div class="folder-new editing">
           <input class="fr-input" id="fr-input" type="text" maxlength="${MAX_NAME}"
                  placeholder="${capital(source.noun)} name" aria-label="New ${source.noun} name" />
         </div>`
      : `<button class="folder-new" id="folder-new">${PLUS_ICON} Create new ${source.noun}</button>`;

  return `
    <div class="folder-wrap">
      <button class="folder-pill" id="folder-btn" aria-haspopup="true" aria-expanded="${menuOpen}"
              title="${source.pillTitle}">
        <span class="fp-name">${escapeHtml(source.activeName)}</span>
        ${source.icon}
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

/** Finish an edit, applying anything the source held back while it was open. */
function endEdit(source: FolderSource) {
  editing = null;
  source.afterEdit?.();
  redraw();
}

function commitEdit(value: string, source: FolderSource) {
  const name = value.trim().slice(0, MAX_NAME);
  const was = editing;
  editing = null;
  if (name && was) {
    if (was.kind === "create") {
      menuOpen = false;
      source.create(name);
    } else {
      source.rename(was.id, name);
    }
  }
  endEdit(source);
}

function wireFolderControl(source: FolderSource) {
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
      if (id !== source.activeId) source.select(id);
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
      source.remove(Number(btn.dataset.yes));
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
        commitEdit(input.value, source);
      } else if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation(); // don't also close the menu
        endEdit(source);
      }
    });
    // Clicking away commits a non-empty name rather than silently discarding it.
    input.addEventListener("blur", () => {
      if (editing) commitEdit(input.value, source);
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
      // Only text expands. A file row shows a filename by design, and replacing
      // it with the clipboard's own text would undo that; a row that already
      // fits has nothing to reveal, and rewriting it would drop the swatch.
      if (desc.dataset.kind !== "text") return;
      if (desc.scrollWidth <= desc.clientWidth) return;
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

/**
 * Fill in each image slot's thumbnail.
 *
 * Fetched per render rather than sent with the state: a stored image is the
 * full-size original, and `SlotPreview` is persisted for every slot of every
 * folder, so neither the preview nor the state payload is the place for it. The
 * backend downscales to a few KB before it crosses the IPC boundary.
 *
 * A slot whose image cannot be decoded simply loses the element and keeps its
 * text description, so an unsupported format degrades rather than showing a
 * broken image.
 */
function wireThumbnails(root: HTMLElement) {
  root.querySelectorAll<HTMLImageElement>("[data-thumb]").forEach(async (img) => {
    const index = Number(img.dataset.thumb);
    const cached = thumbs.get(index);
    if (cached) {
      img.src = cached;
      return;
    }
    const url = await invoke<string | null>("slot_thumbnail", { index });
    if (!url) {
      img.remove();
      return;
    }
    thumbs.set(index, url);
    img.src = url;
    // The row just got taller, so the window needs to grow with it.
    fitMainWindow();
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
function titlebar(name: string, active: jotter.View): string {
  return `
    <div class="titlebar" data-tauri-drag-region>
      <div class="titlebar-brand">
        <img class="titlebar-logo" src="/logo-white.png" alt="" />
        ${IS_MAC ? "" : `<span>${name}</span>`}
      </div>${IS_MAC ? viewSwitch(active) : ""}
      <div class="titlebar-controls">
        <button class="tb-btn" id="tb-min" title="Minimize" aria-label="Minimize">
          <svg viewBox="0 0 12 12" aria-hidden="true"><rect x="1.5" y="6" width="9" height="1.1" fill="currentColor"/></svg>
        </button>
        <button class="tb-btn tb-close" id="tb-close" title="Close" aria-label="Close">
          <svg viewBox="0 0 12 12" aria-hidden="true"><path d="M2 2 L10 10 M10 2 L2 10" stroke="currentColor" stroke-width="1.2" stroke-linecap="round"/></svg>
        </button>
      </div>
    </div>`;
}

function helpButton(body: string): string {
  return `<div class="info" tabindex="0" role="button" aria-label="Shortcuts and help">
            <span class="info-q" aria-hidden="true">?</span>
            <div class="info-pop" role="tooltip">${body}</div>
          </div>`;
}

/** macOS: the switch between the three tools, in the middle of the title bar. */
function viewSwitch(active: jotter.View): string {
  const tab = (v: jotter.View, name: string) =>
    `<button class="mode${active === v ? " active" : ""}" data-view="${v}">${name}</button>`;
  return `<div class="mode-switch" role="group" aria-label="Tool">
            ${tab("paster", "Paster")}${tab("jotter", "Jotter")}${tab("shotter", "Shotter")}
          </div>`;
}

function renderMain(state: StateDto) {
  const isNoob = state.mode === "noob";
  const hasFilled = state.slots.some((s) => s.filled);
  const slotRows = state.slots
    .map((s) => {
      const filled = s.filled;
      const swatch = hexColor(s.preview)
        ? `<span class="s-swatch" style="background-color:${hexColor(s.preview)}" aria-hidden="true"></span>`
        : "";
      // Images get a thumbnail above the description. The element is rendered
      // empty and filled in once the backend has produced a small PNG, so a
      // render is never blocked on decoding an image.
      const thumb =
        s.preview?.kind === "image"
          ? `<img class="s-thumb" data-thumb="${s.index}" alt="" />`
          : "";
      const meta = filled
        ? `<div class="s-desc" data-desc="${s.index}" data-kind="${s.preview!.kind}">${thumb}${escapeHtml(
            describe(s.preview),
          )}${swatch}</div>
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
    ${titlebar("Paster", "paster")}
    <div class="panel" data-mode="${IS_MAC ? "noob" : state.mode}">
      <header class="panel-head">
        ${folderControl(pasterFolders(state))}
        <div class="head-right">
          ${
            IS_MAC
              ? ""
              : `<div class="mode-switch" role="group" aria-label="Mode">
            <button class="mode ${!isNoob ? "active" : ""}" data-mode="master">Master &gt;:)</button>
            <button class="mode ${isNoob ? "active" : ""}" data-mode="noob">Noob :)</button>
          </div>`
          }
          ${helpButton(`
              <b>${MOD}+&lt;N&gt;+C</b> copies into slot N<br />
              <b>${MOD}+&lt;N&gt;+V</b> pastes it (add <b>Shift</b> to paste as plain text)<br />
              Plain ${MOD}+C / ${MOD}+V still work normally.<br />
              <b>Click any slot</b> to load it onto the clipboard, then paste with ${MOD}+V.
              <br /><br />
              <b>Folders</b> each hold their own 9 slots — hotkeys, Clear all and Undo
              apply only to the folder you're in.${
                IS_MAC ? ` Hold <b>${MOD}+&lt;N&gt;</b> and press <b>←</b> / <b>→</b> to switch folders.` : ""
              }${
                IS_MAC
                  ? ""
                  : `
              <br /><br />
              <b>Noob</b> shows a popup by your cursor; <b>Master</b> is fully invisible.`
              }
            `)}
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
        <span class="tip">${BACKGROUND_TIP}</span>
      </footer>
    </div>`;

  wireSlotScrolling(app);
  wireThumbnails(app);

  const win = getCurrentWindow();
  app.querySelector<HTMLButtonElement>("#tb-min")?.addEventListener("click", () => {
    win.minimize();
  });
  app.querySelector<HTMLButtonElement>("#tb-close")?.addEventListener("click", () => {
    win.hide(); // keep running in the tray
  });
  // macOS has no modes; its switch, in the title bar, picks the tool.
  if (IS_MAC) {
    wireViewSwitch();
  } else {
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
  }
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

  wireFolderControl(pasterFolders(state));
}

// ---------------------------------------------------------------------------
// Jotter view (macOS): the same chrome as Paster, around a notepad.
// ---------------------------------------------------------------------------
const JOTTER_HELP = `
              <b>Return</b> starts a new line with its own dot.<br />
              <b>Tab</b> tucks a line under the one above; <b>Shift+Tab</b> brings it back out.<br />
              <b>Click a dot</b> to cross out that line and everything tucked under it. Click
              it again to bring them back.
              <br /><br />
              <b>Jotpads</b> each hold their own note. <b>Clear Jots</b> removes the lines
              you've crossed out, and Undo puts them back; both apply only to the jotpad
              you're in.
              <br /><br />
              <b>The clock</b> beside the jotpad sets reminders for that jotpad: how often,
              when, and which sound.
              <br /><br />
              Notes save as you type. Slot hotkeys still work here:
              <b>${MOD}+&lt;N&gt;+V</b> pastes a slot into the note.`;

function jotterFolders(): FolderSource {
  return {
    folders: jotter.folderList(),
    activeId: jotter.activeId(),
    activeName: jotter.activeName(),
    pillTitle: "Jotpad — each has its own note",
    noun: "jotpad",
    icon: JOTPAD_ICON,
    create: (name) => {
      jotter.createFolder(name);
      jotter.focus();
    },
    rename: (id, name) => jotter.renameFolder(id, name),
    select: (id) => {
      jotter.selectFolder(id);
      jotter.focus();
    },
    remove: (id) => jotter.deleteFolder(id),
  };
}

function renderJotter() {
  document.body.dataset.view = "jotter";
  app.innerHTML = `
    ${titlebar("Jotter", "jotter")}
    <div class="panel jotter" data-mode="noob">
      <header class="panel-head">
        <div class="head-left">
          ${folderControl(jotterFolders())}
          ${reminders.control()}
        </div>
        <div class="head-right">
          ${helpButton(JOTTER_HELP)}
        </div>
      </header>
      <div class="j-host"></div>
      <footer class="panel-foot"></footer>
    </div>`;
  jotter.mount(app.querySelector<HTMLElement>(".j-host")!);
  wireViewSwitch();
  wireFolderControl(jotterFolders());
  reminders.wire();
  refreshJotterFoot(true);
}

/**
 * Jotter's stand-in for a full redraw: rebuild the folder menu and the footer,
 * never the note. Replacing the note would take it out from under the caret.
 */
function redrawJotterChrome() {
  const wrap = app.querySelector(".folder-wrap");
  if (wrap) {
    const source = jotterFolders();
    wrap.outerHTML = folderControl(source);
    wireFolderControl(source);
  }
  reminders.refresh();
  refreshJotterFoot(true);
}

let jotterFootKey = "";

/** Jotter's footer. Called on every edit, so it only rebuilds when it would change. */
function refreshJotterFoot(force = false) {
  const foot = app.querySelector(".panel.jotter .panel-foot");
  if (!foot) return;
  const key = `${jotter.activeId()}|${jotter.canClear()}|${jotter.undoOffered()}`;
  if (!force && key === jotterFootKey) return;
  jotterFootKey = key;
  foot.innerHTML = `
    <button class="ghost" id="clear-all"${jotter.canClear() ? "" : " disabled"}
      title="Remove the crossed-out lines in “${escapeHtml(jotter.activeName())}” — other jotpads are untouched">Clear Jots</button>
    ${
      jotter.undoOffered()
        ? `<button class="ghost undo" id="undo-clear" title="Put the cleared jots back">${UNDO_ICON} Undo</button>`
        : ""
    }
    <span class="spacer"></span>
    <span class="tip">${BACKGROUND_TIP}</span>`;
  foot.querySelector("#clear-all")?.addEventListener("click", () => {
    jotter.clearCrossed();
    jotter.focus();
  });
  foot.querySelector("#undo-clear")?.addEventListener("click", () => {
    jotter.undoClear();
    jotter.focus();
  });
}

// ---------------------------------------------------------------------------
// Shotter view (macOS): screenshots taken while CQ is running.
// ---------------------------------------------------------------------------
const SHOTTER_HELP = `
              <b>Screenshots</b> you take while cQ is running show up here, newest first,
              a few seconds after you take them.<br />
              <b>Click one</b> to copy it, then paste anywhere with ${MOD}+V.
              <br /><br />
              <b>The pencil</b> opens it for markup. <b>Done</b> saves over the original
              and copies it.<br />
              <b>The trash can</b> moves the file to the Trash, and <b>Clear Shots</b> moves them
              all; Undo brings them back.`;

function renderShotter() {
  document.body.dataset.view = "shotter";
  app.innerHTML = `
    ${titlebar("Shotter", "shotter")}
    <div class="panel shotter" data-mode="noob">
      <header class="panel-head">
        <div class="head-left"><span class="shot-count"></span></div>
        <div class="head-right">
          ${helpButton(SHOTTER_HELP)}
        </div>
      </header>
      <div class="shot-host"></div>
      <footer class="panel-foot"></footer>
    </div>`;
  shotter.mount(app.querySelector<HTMLElement>(".shot-host")!);
  wireViewSwitch();
  refreshShotterChrome(true);
}

let shotterChromeKey = "";

/** Shotter's count and footer. Rebuilt only when they would change. */
function refreshShotterChrome(force = false) {
  const panel = app.querySelector(".panel.shotter");
  if (!panel) return;
  const key = `${shotter.count()}|${shotter.undoOffered()}`;
  if (!force && key === shotterChromeKey) return;
  shotterChromeKey = key;
  const n = shotter.count();
  panel.querySelector(".shot-count")!.textContent =
    n === 0 ? "No screenshots yet" : `${n} screenshot${n === 1 ? "" : "s"}`;
  const foot = panel.querySelector(".panel-foot")!;
  foot.innerHTML = `
    <button class="ghost" id="clear-shots"${n ? "" : " disabled"}
      title="Move every screenshot here to the Trash">Clear Shots</button>
    ${
      shotter.undoOffered()
        ? `<button class="ghost undo" id="undo-trash" title="Put back what was just moved to the Trash">${UNDO_ICON} Undo</button>`
        : ""
    }
    <span class="spacer"></span>
    <span class="tip">${BACKGROUND_TIP}</span>`;
  foot.querySelector("#clear-shots")?.addEventListener("click", () => shotter.trashAll());
  foot.querySelector("#undo-trash")?.addEventListener("click", () => shotter.undoTrash());
}

function wireViewSwitch() {
  app.querySelectorAll<HTMLButtonElement>(".mode[data-view]").forEach((btn) => {
    btn.addEventListener("click", () => switchView(btn.dataset.view as jotter.View));
  });
}

function switchView(next: jotter.View) {
  if (next === view) return;
  const from = view;
  // Each tool has its own folders; a half-finished folder edit doesn't carry over.
  menuOpen = false;
  editing = null;
  confirmDelete = null;
  reminders.reset();
  if (deferred) {
    latest = deferred;
    deferred = null;
  }
  if (from === "paster") {
    // Jotter and Shotter keep the height Paster had sized the window to.
    jotter.rememberHeight(window.innerHeight);
  } else if (from === "jotter") {
    jotter.flush();
  }
  view = next;
  jotter.setView(next);
  if (next === "paster") {
    delete document.body.dataset.view;
    if (latest) renderMain(latest);
    fitMainWindow();
  } else if (next === "jotter") {
    renderJotter();
  } else {
    renderShotter();
  }
  crossFade(from, next);
  if (next === "jotter") jotter.focus();
}

/**
 * The switch is rebuilt along with the rest of the page, which would skip its
 * colour transition. Draw it in the old position first, then flip it.
 */
function crossFade(from: jotter.View, to: jotter.View) {
  const buttons = app.querySelectorAll<HTMLElement>(".mode[data-view]");
  buttons.forEach((b) => b.classList.toggle("active", b.dataset.view === from));
  void app.offsetWidth; // commit the old state so the change below animates
  buttons.forEach((b) => b.classList.toggle("active", b.dataset.view === to));
}

/**
 * macOS: load the Jotter and, if the control panel was last left on it, open on
 * that side at the height it had.
 */
async function startJotter() {
  await jotter.load();
  jotter.onChange(() => refreshJotterFoot());
  // "Open Jotter" on a reminder card: land in that folder's note.
  await listen<number>("jotter-open", ({ payload: id }) => {
    if (view !== "jotter") switchView("jotter");
    if (id !== jotter.activeId()) {
      jotter.selectFolder(id);
      redraw();
    }
    jotter.focus();
  });
  let resizeTimer: number | undefined;
  window.addEventListener("resize", () => {
    if (view === "paster") return;
    window.clearTimeout(resizeTimer);
    resizeTimer = window.setTimeout(() => jotter.rememberHeight(window.innerHeight), 250);
  });
}

/** macOS: load Shotter's list and keep the view in step with it. */
async function startShotter() {
  shotter.onChange(() => refreshShotterChrome());
  await shotter.start();
}

/**
 * macOS: if the control panel was last left on Jotter or Shotter, open there, at
 * the height it had.
 */
function openSavedView() {
  const saved = jotter.view();
  if (saved === "paster") return;
  view = saved;
  if (saved === "jotter") renderJotter();
  else renderShotter();
  const h = jotter.savedHeight();
  if (h) getCurrentWindow().setSize(new LogicalSize(PANEL_WIDTH, h)).catch(() => {});
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
  // Jotter and Shotter keep whatever height the window had when they were switched to.
  if (view !== "paster") return;
  requestAnimationFrame(() => {
    if (view !== "paster") return; // switched while this frame was pending
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
  if (view === "jotter") {
    redrawJotterChrome();
    return;
  }
  if (view === "shotter") {
    refreshShotterChrome(true);
    return;
  }
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
  // Jotter or Shotter is showing. Keep the state for when Paster comes back, but
  // leave the page alone: a redraw would take the note out from under the caret.
  if (view !== "paster") {
    latest = state;
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
  // macOS: the Jotter reminder card, in a small window of its own.
  if (label === "reminder") {
    await reminders.startCard(app);
    return;
  }
  // macOS: Shotter's markup window.
  if (label === "markup") {
    await markup.start(app);
    return;
  }
  // macOS: the dictation setup window, which fetches the speech model.
  if (label === "dictate") {
    await dictate.start(app);
    return;
  }
  // Re-fit the control panel whenever it's opened/focused, so it can't flash at
  // the initial config size before the content measurement settles.
  if (label === "main") {
    getCurrentWindow().onFocusChanged(({ payload: focused }) => {
      if (focused) fitMainWindow();
      if (view === "jotter") {
        // Reopening the window lands back in the note, ready to type.
        if (focused && document.activeElement === document.body) jotter.focus();
        if (!focused) jotter.flush();
      }
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
  if (IS_MAC && label === "main") {
    await startJotter();
    await startShotter();
    openSavedView();
  }
  try {
    const state = await invoke<StateDto>("get_state");
    render(state);
    // Opening on Jotter puts focus in the note, so leave it there.
    if (view !== "jotter") dropInitialFocus();
  } catch (e) {
    console.error("get_state failed", e);
  }
  await listen<StateDto>("state-updated", (ev) => {
    // A slot may have been refilled, so any cached content is stale.
    fullText.clear();
    thumbs.clear();
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
