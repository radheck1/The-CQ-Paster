/**
 * CQ Shotter (macOS only): the screenshots taken while CQ is running, newest
 * first, in the control panel beside Paster and Jotter.
 *
 * The list lives in the backend (`shotter.rs`), which watches the folder macOS
 * saves screenshots to and keeps the list across launches. This module draws it
 * and passes on what the user does with a screenshot: click to copy it, click
 * its name to rename the file, the pencil to mark it up, the trash can to move it
 * to the Trash. Clear Shots moves them all, and either comes with a short Undo.
 */
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export type Shot = {
  id: number;
  /** The file's name, shown on hover. */
  name: string;
  /** When it was taken, in ms since the epoch. */
  taken: number;
  /** Changes whenever the file does, so a marked-up screenshot gets a fresh thumbnail. */
  version: number;
  /** Whether the markup window can open it: PNG only. */
  editable: boolean;
};
type ShotList = { folder: string; shots: Shot[] };

/** Same window Paster and Jotter give their Undo. */
const UNDO_MS = 10000;
/** How long the "Copied" flash stays up. */
const COPIED_MS = 1300;

const icon = (body: string, size: number) =>
  `<svg viewBox="0 0 24 24" width="${size}" height="${size}" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${body}</svg>`;
const PENCIL_ICON = icon(`<path d="M12 20h9"/><path d="M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4z"/>`, 14);
const TRASH_ICON = icon(
  `<polyline points="3 6 5 6 21 6"/><path d="M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6"/><path d="M10 11v6M14 11v6"/><path d="M9 6V4a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2"/>`,
  14,
);
const CAMERA_ICON = icon(
  `<path d="M23 19a2 2 0 0 1-2 2H3a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h4l2-3h6l2 3h4a2 2 0 0 1 2 2z"/><circle cx="12" cy="13" r="4"/>`,
  28,
);

let folder = "Desktop";
let shots: Shot[] = [];
let onChangeFn: () => void = () => {};
/** Thumbnails as data URLs, by `id:version`. */
const thumbs = new Map<string, string>();
/** Until when Undo is offered for the last trash, of one screenshot or all of them. */
let undoUntil = 0;
let undoTimer: number | undefined;
/** The screenshot whose name is being typed; the list holds still while it is. */
let editing: number | null = null;

// Created once and kept, like Jotter's editor: switching tools re-attaches it,
// so its scroll position survives.
const list = document.createElement("div");
list.className = "shot-list";

const esc = (s: string) =>
  s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
const thumbKey = (s: Shot) => `${s.id}:${s.version}`;

export const count = () => shots.length;
export const undoOffered = () => Date.now() < undoUntil;

/** Called whenever the list or the Undo offer changes. */
export function onChange(fn: () => void) {
  onChangeFn = fn;
}

/** Load the list and follow the backend's changes to it. */
export async function start() {
  try {
    apply(await invoke<ShotList>("shots_list"));
  } catch (e) {
    console.error("[shotter] could not load the list", e);
  }
  await listen<ShotList>("shots-changed", ({ payload }) => apply(payload));
}

function apply(next: ShotList) {
  folder = next.folder;
  shots = next.shots;
  // Drop thumbnails for screenshots that are gone or have changed.
  const live = new Set(shots.map(thumbKey));
  for (const key of thumbs.keys()) if (!live.has(key)) thumbs.delete(key);
  // Redrawing now would take the name field away mid-word, and a screenshot can
  // arrive at any moment. Closing the field draws, by which time `shots` is
  // whatever the latest update left here.
  if (editing === null) draw();
  onChangeFn();
}

export function mount(host: HTMLElement) {
  host.replaceChildren(list);
  draw();
  matchGutter();
}

/** Pad the left by the scrollbar's width, so the cards sit centred. See `.shot-list`. */
function matchGutter() {
  if (!list.isConnected) return;
  const gutter = list.offsetWidth - list.clientWidth;
  list.style.setProperty("--gutter", `${Math.max(0, gutter)}px`);
}

window.addEventListener("resize", matchGutter);

/**
 * "1:54 PM" today, "Yesterday 10:43 AM", and a date before that. Exported for
 * the unit tests.
 */
export function timeLabel(taken: number, now: number): string {
  const t = new Date(taken);
  const time = t.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  const day = (d: Date) => new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
  const days = Math.round((day(new Date(now)) - day(t)) / 86_400_000);
  if (days === 0) return time;
  if (days === 1) return `Yesterday ${time}`;
  const date = t.toLocaleDateString([], { month: "short", day: "numeric" });
  return `${date}, ${time}`;
}

function draw() {
  if (shots.length === 0) {
    list.innerHTML = `
      <div class="shot-empty">
        <span class="shot-empty-icon">${CAMERA_ICON}</span>
        <b>Take a screenshot and it shows up here.</b>
        <span>Screenshots saved to ${esc(folder)} while cQ is running appear here, newest first.</span>
      </div>`;
    return;
  }
  const now = Date.now();
  list.innerHTML = shots
    .map((s) => {
      const src = thumbs.get(thumbKey(s));
      return `
        <figure class="shot" data-id="${s.id}">
          <button class="shot-img" data-copy="${s.id}" title="Click to copy · ${esc(s.name)}">
            ${src ? `<img src="${src}" alt="" />` : `<span class="shot-loading"></span>`}
            <span class="shot-copied">✓ Copied</span>
          </button>
          <figcaption class="shot-meta">
            <button class="shot-name" data-rename="${s.id}" title="Click to rename · ${esc(s.name)}">${esc(baseName(s.name))}</button>
            <span class="shot-when">${esc(timeLabel(s.taken, now))}</span>
          </figcaption>
          <div class="shot-actions">
            ${
              s.editable
                ? `<button class="shot-act" data-markup="${s.id}" title="Mark up" aria-label="Mark up">${PENCIL_ICON}</button>`
                : ""
            }
            <button class="shot-act danger" data-trash="${s.id}" title="Move to Trash" aria-label="Move to Trash">${TRASH_ICON}</button>
          </div>
        </figure>`;
    })
    .join("");
  list.querySelectorAll<HTMLButtonElement>("[data-copy]").forEach((b) =>
    b.addEventListener("click", () => copy(Number(b.dataset.copy))),
  );
  list.querySelectorAll<HTMLButtonElement>("[data-markup]").forEach((b) =>
    b.addEventListener("click", () => invoke("shot_markup", { id: Number(b.dataset.markup) })),
  );
  list.querySelectorAll<HTMLButtonElement>("[data-trash]").forEach((b) =>
    b.addEventListener("click", () => trash(Number(b.dataset.trash))),
  );
  list.querySelectorAll<HTMLButtonElement>("[data-rename]").forEach((b) =>
    b.addEventListener("click", () => startRename(Number(b.dataset.rename))),
  );
  loadThumbnails();
}

/** The name without its extension: the part worth typing over. */
const baseName = (name: string) => name.replace(/\.[^.]+$/, "");

/**
 * Click a name to type a new one, the same as renaming a folder or a jotpad:
 * Enter saves, Escape cancels, clicking away saves. A name the folder already
 * has keeps the field open with the reason, since the alternative is replacing
 * a file that may not even be a screenshot.
 */
function startRename(id: number) {
  const shot = shots.find((s) => s.id === id);
  const meta = list.querySelector<HTMLElement>(`.shot[data-id="${id}"] .shot-meta`);
  if (!shot || !meta) return;
  editing = id;
  meta.innerHTML = `<input class="shot-rename" type="text" aria-label="Screenshot name" /><span class="shot-error" role="status"></span>`;
  const input = meta.querySelector("input")!;
  const error = meta.querySelector(".shot-error")!;
  input.value = baseName(shot.name);
  input.focus();
  input.select();

  let closed = false;
  let saving = false;
  const close = () => {
    closed = true;
    editing = null;
    draw();
  };
  const save = async () => {
    if (closed || saving) return;
    const typed = input.value.trim();
    if (!typed || typed === baseName(shot.name)) return close();
    saving = true;
    input.disabled = true;
    try {
      await invoke<string>("shot_rename", { id, name: typed });
      close(); // the backend's update redraws too; this is just immediate
    } catch (e) {
      const reason = String(e);
      if (reason === "empty") return close();
      error.textContent = reason === "taken" ? "That name is taken" : "Couldn't rename that file";
      input.disabled = false;
      input.focus();
      input.select();
    } finally {
      saving = false;
    }
  };
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      void save();
    } else if (e.key === "Escape") {
      e.preventDefault();
      close();
    }
  });
  input.addEventListener("blur", () => void save());
}

let loading = false;

/** Fetch missing thumbnails one at a time, so a burst of screenshots doesn't pile up decodes. */
async function loadThumbnails() {
  if (loading) return;
  loading = true;
  try {
    for (;;) {
      const next = shots.find((s) => !thumbs.has(thumbKey(s)));
      if (!next) break;
      const key = thumbKey(next);
      const url = await invoke<string | null>("shot_thumbnail", { id: next.id }).catch(() => null);
      // An empty string marks a file that can't be drawn, so it isn't asked for again.
      thumbs.set(key, url ?? "");
      if (!url) continue;
      const card = list.querySelector<HTMLElement>(`.shot[data-id="${next.id}"]`);
      const holder = card?.querySelector(".shot-loading");
      if (holder) {
        const img = document.createElement("img");
        img.src = url;
        img.alt = "";
        holder.replaceWith(img);
      }
    }
  } finally {
    loading = false;
  }
}

async function copy(id: number) {
  const ok = await invoke<boolean>("shot_copy", { id }).catch(() => false);
  if (!ok) return;
  const card = list.querySelector<HTMLElement>(`.shot[data-id="${id}"]`);
  if (!card) return;
  card.classList.add("copied");
  window.setTimeout(() => card.classList.remove("copied"), COPIED_MS);
}

async function trash(id: number) {
  if (await invoke<boolean>("shot_trash", { id }).catch(() => false)) offerUndo();
}

/** Clear Shots: move every screenshot in the list to the Trash. */
export async function trashAll() {
  if (await invoke<boolean>("shots_trash_all").catch(() => false)) offerUndo();
}

function offerUndo() {
  undoUntil = Date.now() + UNDO_MS;
  window.clearTimeout(undoTimer);
  undoTimer = window.setTimeout(() => {
    undoUntil = 0;
    onChangeFn();
  }, UNDO_MS);
  onChangeFn();
}

/** Put back whatever the last trash moved. */
export async function undoTrash() {
  if (!undoOffered()) return;
  undoUntil = 0;
  window.clearTimeout(undoTimer);
  onChangeFn();
  await invoke<boolean>("shot_untrash").catch(() => false);
}
