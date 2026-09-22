/**
 * CQ Jotter (macOS only): one notepad per folder, in the control panel beside
 * Paster.
 *
 * This module owns the Jotter's data — its folders, their notes, and the two
 * things the control panel remembers between launches — and the editor itself.
 * `main.ts` draws the chrome around it (title bar, folder menu, footer) and
 * calls in here.
 *
 * The editor is one contenteditable element with a div per line. Plain typing
 * inside a line is left to WebKit, so accents, dictation and spelling
 * corrections all behave natively. Anything that changes the line structure —
 * Return, Tab, pasting, deleting across lines — is intercepted, applied to the
 * model in `outline.ts`, and redrawn. The dots are not part of the editable
 * content at all: they sit in a layer on top, so neither the caret nor a
 * selection can ever land on one.
 */
import { invoke } from "@tauri-apps/api/core";
import {
  backspaceAtStart,
  blankLine,
  comparePos,
  crossings,
  deleteAtEnd,
  deleteRange,
  hasCrossed,
  insertText,
  isPristine,
  normalize,
  openCount,
  ordered,
  removeCrossed,
  repairLines,
  restoreCrossed,
  serialize,
  shiftDepth,
  splitLine,
  toggleDone,
  type Line,
  type Pos,
} from "./outline";

/** Which tool the control panel shows. Kept here because it is saved with the notes. */
export type View = "paster" | "jotter" | "shotter";
type Folder = {
  id: number;
  name: string;
  lines: Line[];
  reminder: Reminder;
  /**
   * Only ever "plain", only ever on a dictation pad from an older version, and
   * only long enough for `retireDictationPads` to let go of it. Nothing sets
   * it any more.
   */
  kind?: "plain";
};

/**
 * One folder's reminder settings. The schedule itself runs in the backend
 * (`reminders.rs`), which reads these from every save; `customEvery` and
 * `hours` only matter to the settings menu.
 */
export type Reminder = {
  on: boolean;
  /** Minutes between reminders, lined up with the clock. */
  every: number;
  /** The menu shows a number field instead of the presets. */
  customEvery: boolean;
  hours: "working" | "custom";
  /** Minutes after midnight. */
  start: number;
  end: number;
  /** 0 = Sunday. */
  days: number[];
  /** A macOS system sound, or "" for none. */
  sound: string;
};

export const WORKING_HOURS = { start: 9 * 60, end: 17 * 60, days: [1, 2, 3, 4, 5] };

export const defaultReminder = (): Reminder => ({
  on: false,
  every: 60,
  customEvery: false,
  hours: "working",
  ...WORKING_HOURS,
  days: [...WORKING_HOURS.days],
  sound: "Glass",
});
type Doc = {
  version: 1;
  /** Which tool the control panel was last left on. */
  view: View;
  /** Window height for a launch that opens straight into Jotter or Shotter. */
  height: number | null;
  active: number;
  nextId: number;
  folders: Folder[];
};
type Sel = { anchor: Pos; focus: Pos };

const MAX_NAME = 24;
/** The home folder can't be renamed, so its name lives here rather than on disk. */
const HOME_NAME = "Main Jots";
/**
 * Dictations used to be kept in a jotpad of this name. They are not any more —
 * a list of raw transcripts is not something anyone wants among their notes,
 * and the backend keeps them now. The name survives only so an existing one
 * can be recognised and let go of; see `repairDoc`.
 */
const DICTATIONS_NAME = "Dictations";
const SAVE_MS = 300;
/** Same window Paster gives its Undo. */
const UNDO_MS = 10000;
const UNDO_LIMIT = 200;
/** Keystrokes on one line with no pause this long undo as one step. */
const TYPING_MS = 1500;

// Geometry the dot layer shares with `.j-line` in styles.css. Change together.
const LINE_PAD_TOP = 3;
const LINE_HEIGHT = 22;
const DOT_BOX = 22;
const DOT_LEFT = 6;
const INDENT = 24;

const defaultDoc = (): Doc => ({
  version: 1,
  view: "paster",
  height: null,
  active: 1,
  nextId: 2,
  folders: [{ id: 1, name: HOME_NAME, lines: [blankLine()], reminder: defaultReminder() }],
});

let doc = defaultDoc();
/** Set when a saved file exists but couldn't be read. Nothing is saved over it. */
let saveBlocked = false;
let onChangeFn: () => void = () => {};

// ---------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------

export async function load(): Promise<void> {
  try {
    const text = await invoke<string | null>("jotter_load");
    doc = text == null ? defaultDoc() : repairDoc(JSON.parse(text));
  } catch (e) {
    saveBlocked = true;
    console.error("[jotter] could not load notes; saving is off so the file is left alone", e);
  }
}

/**
 * Retire dictation pads from older versions.
 *
 * There may be several. `kind` was never read back from disk, so every load
 * found no dictation pad and made another one — a bug that left one pad per
 * launch, each holding whatever was dictated that session. They are merged
 * into one, in the order they were created, and it becomes an ordinary
 * jotpad: no longer written to, no longer undeletable, renameable and
 * deletable like any other.
 *
 * The text stays. It is the user's, and discarding it on their behalf is not
 * this function's business — but a pad that only ever held an empty line is
 * dropped, since an empty pad nobody asked for is just clutter.
 *
 * Exported so it can be tested against the shape a real file had.
 */
export function retireDictationPads(folders: Folder[]): Folder[] {
  const pads = folders.filter((f) => f.name === DICTATIONS_NAME || f.kind === "plain");
  if (pads.length === 0) return folders;
  const kept = pads[0];
  delete kept.kind;
  for (const other of pads.slice(1)) {
    if (!isPristine(other.lines)) {
      kept.lines = isPristine(kept.lines)
        ? other.lines
        : [...kept.lines, blankLine(), ...other.lines];
    }
    folders.splice(folders.indexOf(other), 1);
  }
  if (isPristine(kept.lines) && folders.length > 1) {
    folders.splice(folders.indexOf(kept), 1);
  }
  return folders;
}

/** Trim, collapse whitespace, cap the length — the same rules as Paster's folders. */
function cleanName(raw: string): string {
  const collapsed = raw.split(/\s+/).filter(Boolean).join(" ");
  return Array.from(collapsed).slice(0, MAX_NAME).join("") || "Folder";
}

/** Rebuild the document from whatever was on disk, keeping every folder's note. */
function repairDoc(raw: unknown): Doc {
  const fresh = defaultDoc();
  if (!raw || typeof raw !== "object") return fresh;
  const r = raw as Record<string, unknown>;
  const folders: Folder[] = [];
  const needsId: Folder[] = [];
  const seen = new Set<number>();
  for (const item of Array.isArray(r.folders) ? r.folders : []) {
    if (!item || typeof item !== "object") continue;
    const o = item as Record<string, unknown>;
    const folder: Folder = {
      id: 0,
      name: cleanName(typeof o.name === "string" ? o.name : ""),
      lines: repairLines(o.lines),
      reminder: repairReminder(o.reminder),
    };
    const id = o.id;
    if (typeof id === "number" && Number.isInteger(id) && id > 0 && !seen.has(id)) {
      folder.id = id;
      seen.add(id);
    } else {
      // A duplicate or missing id: give the folder a new one rather than drop it.
      needsId.push(folder);
    }
    folders.push(folder);
  }
  if (folders.length === 0) folders.push(...fresh.folders);
  // Also renames a home folder saved under an earlier name.
  folders[0].name = HOME_NAME;
  retireDictationPads(folders);

  let nextId = Math.max(
    typeof r.nextId === "number" && Number.isInteger(r.nextId) ? r.nextId : 1,
    ...folders.map((f) => f.id + 1),
  );
  for (const f of needsId) f.id = nextId++;
  const height = r.height;
  return {
    version: 1,
    view: r.view === "jotter" || r.view === "shotter" ? r.view : "paster",
    height: typeof height === "number" && Number.isFinite(height) && height >= 200 ? Math.round(height) : null,
    active: folders.some((f) => f.id === r.active) ? (r.active as number) : folders[0].id,
    nextId,
    folders,
  };
}

function repairReminder(raw: unknown): Reminder {
  const fallback = defaultReminder();
  if (!raw || typeof raw !== "object") return fallback;
  const r = raw as Record<string, unknown>;
  const int = (v: unknown, min: number, max: number, otherwise: number) =>
    typeof v === "number" && Number.isInteger(v) && v >= min && v <= max ? v : otherwise;
  const days = Array.isArray(r.days)
    ? r.days.filter((d): d is number => typeof d === "number" && Number.isInteger(d) && d >= 0 && d <= 6)
    : fallback.days;
  return {
    on: r.on === true,
    every: int(r.every, 5, 720, fallback.every),
    customEvery: r.customEvery === true,
    hours: r.hours === "custom" ? "custom" : "working",
    start: int(r.start, 0, 1439, fallback.start),
    end: int(r.end, 0, 1439, fallback.end),
    days: [...new Set(days)].sort((a, b) => a - b),
    sound: typeof r.sound === "string" ? r.sound : fallback.sound,
  };
}

const folder = (): Folder => doc.folders.find((f) => f.id === doc.active) ?? doc.folders[0];
/** The home folder is always first: created folders append, and it can't be deleted. */
const homeId = () => doc.folders[0].id;

export const view = () => doc.view;
export const savedHeight = () => doc.height;
export const activeId = () => folder().id;
export const activeName = () => folder().name;
/** Whether Clear Jots has anything to remove in the open note. */
export const canClear = () => hasCrossed(folder().lines);
export const reminder = () => folder().reminder;

/** Change the open folder's reminder settings. */
export function setReminder(patch: Partial<Reminder>) {
  const f = folder();
  f.reminder = { ...f.reminder, ...patch };
  scheduleSave();
  onChangeFn();
}

/** Called after anything the chrome around the editor might need to reflect. */
export function onChange(fn: () => void) {
  onChangeFn = fn;
}

export function setView(v: View) {
  if (doc.view === v) return;
  doc.view = v;
  scheduleSave();
}

export function rememberHeight(height: number) {
  const h = Math.round(height);
  if (h < 200 || doc.height === h) return;
  doc.height = h;
  scheduleSave();
}

export function folderList() {
  return doc.folders.map((f) => ({
    id: f.id,
    name: f.name,
    active: f.id === doc.active,
    permanent: f.id === homeId(),
    count: `${openCount(f.lines)} open`,
  }));
}

/** Create a folder and switch to it — you make one in order to write in it. */
export function createFolder(name: string) {
  const id = doc.nextId++;
  doc.folders.push({ id, name: cleanName(name), lines: [blankLine()], reminder: defaultReminder() });
  selectFolder(id);
}

export function renameFolder(id: number, name: string) {
  const f = doc.folders.find((x) => x.id === id);
  // The dictation pad keeps its name for the same reason the home folder
  // does: dictations are addressed to it, and a renamed one could not be
  // found again.
  if (!f || id === homeId() || f.kind === "plain") return;
  f.name = cleanName(name);
  scheduleSave();
  onChangeFn();
}

/** Delete a folder and its note. If it was open, the neighbour takes over. */
export function deleteFolder(id: number) {
  const pos = doc.folders.findIndex((f) => f.id === id);
  if (pos < 0 || id === homeId() || doc.folders.length <= 1) return;
  // Deleting it would only mean `repairDoc` making an empty one on the next
  // load, minus everything that had been dictated into it.
  if (doc.folders[pos].kind === "plain") return;
  doc.folders.splice(pos, 1);
  history.delete(id);
  savedSel.delete(id);
  if (cleared?.folderId === id) cleared = null;
  if (doc.active === id) {
    doc.active = doc.folders[Math.min(pos, doc.folders.length - 1)].id;
    showActive();
  }
  scheduleSave();
  onChangeFn();
}

export function selectFolder(id: number) {
  if (!doc.folders.some((f) => f.id === id)) return;
  doc.active = id;
  typing = null;
  showActive();
  scheduleSave();
  onChangeFn();
}

// ---- Saving ----

let saveTimer: number | undefined;
let dirty = false;
/** Saves run one at a time, in order, so an older document never lands last. */
let saving: Promise<unknown> = Promise.resolve();

function scheduleSave() {
  dirty = true;
  window.clearTimeout(saveTimer);
  saveTimer = window.setTimeout(flush, SAVE_MS);
}

/** Write any pending change now. Resolves once every save so far has landed. */
export function flush(): Promise<unknown> {
  window.clearTimeout(saveTimer);
  if (!dirty || saveBlocked) return saving;
  dirty = false;
  const text = JSON.stringify(doc);
  saving = saving
    .then(() => invoke("jotter_save", { doc: text }))
    .catch((e) => {
      dirty = true; // retried with the next change
      console.error("[jotter] save failed", e);
    });
  return saving;
}

// ---- Clear Jots ----

let cleared: { folderId: number; before: Line[] } | null = null;
let clearedUntil = 0;
let clearTimer: number | undefined;

export const undoOffered = () => cleared !== null && Date.now() < clearedUntil;

/** Clear Jots: remove the crossed-out lines from the open note. */
export function clearCrossed() {
  const f = folder();
  if (!hasCrossed(f.lines)) return;
  checkpoint(readSel());
  typing = null;
  cleared = { folderId: f.id, before: f.lines };
  clearedUntil = Date.now() + UNDO_MS;
  window.clearTimeout(clearTimer);
  clearTimer = window.setTimeout(() => {
    cleared = null;
    onChangeFn();
  }, UNDO_MS);
  f.lines = removeCrossed(f.lines);
  showActive();
  changed();
}

/**
 * Put the cleared lines back where they were, in the folder they came from.
 * Anything written there since the clear is kept, so Undo never costs text.
 */
export function undoClear() {
  const c = cleared;
  if (!c) return;
  cleared = null;
  window.clearTimeout(clearTimer);
  const f = doc.folders.find((x) => x.id === c.folderId);
  if (f) {
    if (f.id === doc.active) {
      checkpoint(readSel());
      typing = null;
    }
    f.lines = restoreCrossed(c.before, f.lines);
    if (f.id === doc.active) showActive();
  }
  changed();
}

// ---------------------------------------------------------------------------
// Editor
// ---------------------------------------------------------------------------

// Created once and kept: switching views re-attaches this same element, so the
// note is never rebuilt out from under the caret by a redraw of the chrome.
const scroller = document.createElement("div");
scroller.className = "j-scroll";
const editor = document.createElement("div");
editor.className = "j-editor";
editor.contentEditable = "true";
editor.spellcheck = true;
editor.setAttribute("role", "textbox");
editor.setAttribute("aria-multiline", "true");
editor.setAttribute("aria-label", "Note");
const dots = document.createElement("div");
dots.className = "j-dots";
const placeholder = document.createElement("div");
placeholder.className = "j-placeholder";
placeholder.textContent = "Jot something down…";
scroller.append(editor, dots, placeholder);

type Snapshot = { lines: Line[]; sel: Sel | null };
/** Undo and redo, per folder. */
const history = new Map<number, { undo: Snapshot[]; redo: Snapshot[] }>();
/** The line being typed on, so a run of keystrokes undoes as one step. */
let typing: { folder: number; line: number; at: number } | null = null;
/** Where the caret was in each folder, to put it back on return. */
const savedSel = new Map<number, Sel>();
/** The folder whose note is currently in the editor's DOM. */
let rendered = -1;
let wired = false;

export function mount(host: HTMLElement) {
  if (!wired) {
    wire();
    wired = true;
  }
  host.replaceChildren(scroller);
  renderLines();
}

/** Put the caret in the note: where it last was in this folder, or at the end. */
export function focus() {
  if (!editor.isConnected) return;
  editor.focus({ preventScroll: true });
  const saved = savedSel.get(doc.active);
  select(saved && fits(saved) ? saved : caretAt(endPos()));
}

function wire() {
  editor.addEventListener("beforeinput", onBeforeInput);
  editor.addEventListener("input", onInput);
  editor.addEventListener("keydown", onKeyDown);
  editor.addEventListener("compositionstart", onCompositionStart);
  editor.addEventListener("paste", onPaste);
  editor.addEventListener("copy", onCopy);
  editor.addEventListener("cut", onCut);
  // Dragging text within the note would move it as an unstructured blob.
  editor.addEventListener("dragstart", (e) => e.preventDefault());
  dots.addEventListener("mousedown", onDotDown);
  document.addEventListener("selectionchange", () => {
    if (rendered !== doc.active) return;
    const sel = readSel();
    if (sel) savedSel.set(doc.active, sel);
  });
  // Text re-wraps when the window is resized, which moves every line below.
  new ResizeObserver(() => layoutDots()).observe(editor);
}

function showActive() {
  if (editor.isConnected) renderLines();
}

/** Anything changed: reposition dots, save, and let the chrome catch up. */
function changed() {
  layoutDots();
  scheduleSave();
  onChangeFn();
}

// ---- Drawing ----

function renderLines() {
  const lines = folder().lines;
  const { crossed } = crossings(lines);
  const frag = document.createDocumentFragment();
  lines.forEach((l, i) => {
    const row = document.createElement("div");
    row.className = crossed[i] ? "j-line done" : "j-line";
    row.dataset.depth = String(l.depth);
    if (l.done) row.dataset.done = "1";
    // An empty block needs a <br> to have height and to hold the caret.
    if (l.text) row.textContent = l.text;
    else row.append(document.createElement("br"));
    frag.append(row);
  });
  editor.replaceChildren(frag);
  rendered = doc.active;
  layoutDots();
}

function layoutDots() {
  const lines = folder().lines;
  placeholder.hidden = !isPristine(lines);
  const rows = editor.children;
  // Mid-edit, before the input handler has reconciled the page with the model.
  if (rendered !== doc.active || rows.length !== lines.length) return;
  const { crossed, inherited } = crossings(lines);
  while (dots.children.length < rows.length) {
    const dot = document.createElement("button");
    dot.type = "button";
    dot.className = "j-dot";
    dot.tabIndex = -1;
    dots.append(dot);
  }
  while (dots.children.length > rows.length) dots.lastElementChild!.remove();
  for (let i = 0; i < rows.length; i++) {
    const row = rows[i] as HTMLElement;
    const dot = dots.children[i] as HTMLButtonElement;
    const x = DOT_LEFT + lines[i].depth * INDENT;
    const y = row.offsetTop + LINE_PAD_TOP + LINE_HEIGHT / 2 - DOT_BOX / 2;
    dot.style.transform = `translate(${x}px, ${y}px)`;
    dot.dataset.i = String(i);
    dot.classList.toggle("done", crossed[i]);
    dot.classList.toggle("inert", inherited[i]);
    const label = inherited[i] ? "Crossed out with the line above" : crossed[i] ? "Bring back" : "Cross out";
    if (dot.title !== label) {
      dot.title = label;
      dot.setAttribute("aria-label", label);
    }
  }
}

// ---- Positions: page <-> model ----

const rowText = (n: Node) => (n.textContent ?? "").replace(/[\r\n]/g, "").replace(/\u00a0/g, " ");
const caretAt = (p: Pos): Sel => ({ anchor: p, focus: p });

function endPos(): Pos {
  const lines = folder().lines;
  return { line: lines.length - 1, offset: lines[lines.length - 1].text.length };
}

function fits(sel: Sel): boolean {
  const lines = folder().lines;
  return [sel.anchor, sel.focus].every((p) => p.line < lines.length && p.offset <= lines[p.line].text.length);
}

function rowIndex(node: Node): number {
  let n: Node | null = node;
  while (n && n.parentNode !== editor) n = n.parentNode;
  return n ? Array.prototype.indexOf.call(editor.childNodes, n) : -1;
}

function toPos(node: Node, offset: number): Pos | null {
  const nodes = editor.childNodes;
  if (node === editor) {
    if (offset < nodes.length) return { line: offset, offset: 0 };
    return nodes.length ? { line: nodes.length - 1, offset: rowText(nodes[nodes.length - 1]).length } : null;
  }
  const i = rowIndex(node);
  if (i < 0) return null;
  const range = document.createRange();
  range.setStart(nodes[i], 0);
  range.setEnd(node, offset);
  return { line: i, offset: range.toString().replace(/[\r\n]/g, "").length };
}

function toDom(p: Pos): [Node, number] {
  const nodes = editor.childNodes;
  const row = nodes[Math.min(p.line, nodes.length - 1)];
  const walker = document.createTreeWalker(row, NodeFilter.SHOW_TEXT);
  let left = p.offset;
  let last: Text | null = null;
  for (let t = walker.nextNode() as Text | null; t; t = walker.nextNode() as Text | null) {
    if (left <= t.data.length) return [t, left];
    left -= t.data.length;
    last = t;
  }
  return last ? [last, last.data.length] : [row, 0];
}

function readSel(): Sel | null {
  const s = document.getSelection();
  if (!s || s.rangeCount === 0 || !s.anchorNode || !s.focusNode) return null;
  if (!editor.contains(s.anchorNode) || !editor.contains(s.focusNode)) return null;
  const anchor = toPos(s.anchorNode, s.anchorOffset);
  const focus = toPos(s.focusNode, s.focusOffset);
  return anchor && focus ? { anchor, focus } : null;
}

function select(sel: Sel, reveal = true) {
  const s = document.getSelection();
  if (!s || !editor.childNodes.length) return;
  const [an, ao] = toDom(sel.anchor);
  const [fn, fo] = toDom(sel.focus);
  s.setBaseAndExtent(an, ao, fn, fo);
  if (reveal) (editor.children[sel.focus.line] as HTMLElement | undefined)?.scrollIntoView({ block: "nearest" });
}

// ---- Editing ----

function checkpoint(sel: Sel | null) {
  let s = history.get(doc.active);
  if (!s) history.set(doc.active, (s = { undo: [], redo: [] }));
  s.undo.push({ lines: folder().lines, sel });
  if (s.undo.length > UNDO_LIMIT) s.undo.shift();
  s.redo.length = 0;
}

/** A structural edit: one undo step, redraw, place the caret, save. */
function commit(lines: Line[], before: Sel | null, after: Sel) {
  checkpoint(before);
  typing = null;
  folder().lines = lines;
  renderLines();
  select(after);
  changed();
}

function step(direction: "undo" | "redo") {
  const s = history.get(doc.active);
  const from = direction === "undo" ? s?.undo : s?.redo;
  const snap = from?.pop();
  if (!s || !snap) return;
  (direction === "undo" ? s.redo : s.undo).push({ lines: folder().lines, sel: readSel() });
  typing = null;
  folder().lines = snap.lines;
  renderLines();
  select(snap.sel && fits(snap.sel) ? snap.sel : caretAt(endPos()));
  changed();
}

/** Native typing is about to happen: start a new undo step if it's a new run. */
function noteTyping(sel: Sel) {
  const now = Date.now();
  const line = sel.focus.line;
  if (!typing || typing.folder !== doc.active || typing.line !== line || now - typing.at > TYPING_MS) {
    checkpoint(sel);
  }
  typing = { folder: doc.active, line, at: now };
}

function onBeforeInput(e: InputEvent) {
  const type = e.inputType;
  if (type === "historyUndo" || type === "historyRedo") {
    e.preventDefault();
    step(type === "historyUndo" ? "undo" : "redo");
    return;
  }
  // Plain text only: ⌘B and friends do nothing.
  if (type.startsWith("format")) {
    e.preventDefault();
    return;
  }
  const sel = readSel();
  if (!sel) return; // the input handler reconciles whatever happens
  const lines = folder().lines;
  const [start, end] = ordered(sel.anchor, sel.focus);
  const spansLines = start.line !== end.line;
  const collapsed = comparePos(start, end) === 0;

  if (type === "insertParagraph" || type === "insertLineBreak") {
    e.preventDefault();
    const ed = splitLine(lines, start, end);
    commit(ed.lines, sel, caretAt(ed.caret));
    return;
  }

  if (type === "insertFromPaste" || type === "insertFromDrop" || type === "insertFromYank" || (type.startsWith("insert") && spansLines)) {
    e.preventDefault();
    const text = e.dataTransfer?.getData("text/plain") ?? e.data ?? "";
    const ed = insertText(lines, start, end, text);
    commit(ed.lines, sel, caretAt(ed.caret));
    return;
  }

  if (type.startsWith("delete")) {
    if (spansLines) {
      e.preventDefault();
      const ed = deleteRange(lines, start, end);
      commit(ed.lines, sel, caretAt(ed.caret));
      return;
    }
    if (collapsed) {
      const backward = type.includes("Backward");
      const atEdge = backward ? start.offset === 0 : start.offset === lines[start.line].text.length;
      if (atEdge) {
        e.preventDefault();
        const ed = backward ? backspaceAtStart(lines, start.line) : deleteAtEnd(lines, start.line);
        if (ed) commit(ed.lines, sel, caretAt(ed.caret));
        return;
      }
      // A word or line deletion that WebKit means to carry across a line break.
      const target = e.getTargetRanges()[0];
      const a = target && toPos(target.startContainer, target.startOffset);
      const b = target && toPos(target.endContainer, target.endOffset);
      if (a && b && a.line !== b.line) {
        e.preventDefault();
        const ed = deleteRange(lines, a, b);
        commit(ed.lines, sel, caretAt(ed.caret));
        return;
      }
    }
  }

  noteTyping(sel); // ordinary typing within one line: WebKit does it
}

function onInput() {
  if (rendered !== doc.active) return;
  const nodes = editor.childNodes;
  let wellFormed = nodes.length === folder().lines.length;
  for (let i = 0; wellFormed && i < nodes.length; i++) wellFormed = nodes[i].nodeName === "DIV";
  if (wellFormed) syncText();
  else rebuildFromDom();
  changed();
}

/** Copy what WebKit typed into the model. */
function syncText() {
  const lines = folder().lines;
  let next: Line[] | null = null;
  for (let i = 0; i < lines.length; i++) {
    const text = rowText(editor.childNodes[i]);
    if (text !== lines[i].text) {
      next ??= lines.slice();
      next[i] = { ...lines[i], text };
    }
  }
  if (next) folder().lines = next;
}

/**
 * Safety net for an edit that changed the line structure without passing
 * through a handler above. Rather than let the page and the model disagree,
 * read the page back in — keeping each surviving line's indent and crossed-out
 * state — and redraw.
 */
function rebuildFromDom() {
  const s = document.getSelection();
  const focusNode = s && s.rangeCount ? s.focusNode : null;
  const focusOffset = s ? s.focusOffset : 0;
  const out: Line[] = [];
  let caret: Pos | null = null;
  let loose = "";
  const nodes = Array.from(editor.childNodes);
  for (const n of nodes) {
    if (n.nodeName === "DIV") {
      if (loose) out.push({ text: loose, depth: 0, done: false }), (loose = "");
      const el = n as HTMLElement;
      if (focusNode && el.contains(focusNode)) {
        const r = document.createRange();
        r.setStart(el, 0);
        r.setEnd(focusNode, focusOffset);
        caret = { line: out.length, offset: r.toString().length };
      }
      out.push({ text: rowText(el), depth: Number(el.dataset.depth) || 0, done: el.dataset.done === "1" });
    } else if (n.nodeName === "BR") {
      out.push({ text: loose, depth: 0, done: false });
      loose = "";
    } else {
      if (focusNode && (n === focusNode || n.contains(focusNode))) {
        caret = { line: out.length, offset: loose.length + (n === focusNode ? focusOffset : 0) };
      }
      loose += rowText(n);
    }
  }
  if (loose) out.push({ text: loose, depth: 0, done: false });
  folder().lines = normalize(out);
  renderLines();
  select(caretAt(caret && fits(caretAt(caret)) ? caret : endPos()));
}

function onKeyDown(e: KeyboardEvent) {
  if (e.isComposing || e.keyCode === 229) return;
  if (e.key === "Tab" && !e.metaKey && !e.ctrlKey && !e.altKey) {
    e.preventDefault(); // Tab never leaves the note
    const sel = readSel();
    if (!sel) return;
    const [start, end] = ordered(sel.anchor, sel.focus);
    // A selection ending at the very start of a line doesn't include that line.
    const last = end.line > start.line && end.offset === 0 ? end.line - 1 : end.line;
    const next = shiftDepth(folder().lines, start.line, last, e.shiftKey ? -1 : 1);
    if (next) commit(next, sel, sel);
    return;
  }
  // Handled here as well as via `historyUndo`: WebKit only offers its own Undo
  // when its own undo stack has something on it, and most edits here bypass it.
  if (e.metaKey && !e.ctrlKey && !e.altKey && e.key.toLowerCase() === "z") {
    e.preventDefault();
    step(e.shiftKey ? "redo" : "undo");
  }
}

/** An input method starting over a multi-line selection: clear it first. */
function onCompositionStart() {
  const sel = readSel();
  if (!sel || sel.anchor.line === sel.focus.line) return;
  const [start, end] = ordered(sel.anchor, sel.focus);
  const ed = deleteRange(folder().lines, start, end);
  commit(ed.lines, sel, caretAt(ed.caret));
}

function onPaste(e: ClipboardEvent) {
  e.preventDefault(); // never let rich content in
  const sel = readSel();
  if (!sel) return;
  const text = e.clipboardData?.getData("text/plain") ?? "";
  const [start, end] = ordered(sel.anchor, sel.focus);
  if (!text && comparePos(start, end) === 0) return;
  const ed = insertText(folder().lines, start, end, text);
  commit(ed.lines, sel, caretAt(ed.caret));
}

function onCopy(e: ClipboardEvent) {
  const sel = readSel();
  if (!sel || comparePos(sel.anchor, sel.focus) === 0 || !e.clipboardData) return;
  e.preventDefault();
  e.clipboardData.setData("text/plain", serialize(folder().lines, sel.anchor, sel.focus));
}

function onCut(e: ClipboardEvent) {
  onCopy(e);
  const sel = readSel();
  if (!e.defaultPrevented || !sel) return;
  const [start, end] = ordered(sel.anchor, sel.focus);
  const ed = deleteRange(folder().lines, start, end);
  commit(ed.lines, sel, caretAt(ed.caret));
}

function onDotDown(e: MouseEvent) {
  const dot = (e.target as HTMLElement).closest<HTMLElement>(".j-dot");
  if (!dot) return;
  e.preventDefault(); // leave the caret and focus where they are
  const next = toggleDone(folder().lines, Number(dot.dataset.i));
  if (!next) return;
  const sel = readSel();
  checkpoint(sel);
  typing = null;
  folder().lines = next;
  renderLines();
  // Don't scroll back to the caret: the dot clicked may be far from it.
  if (sel) select(sel, false);
  changed();
}
