/**
 * The dictation setup window (macOS only).
 *
 * Dictation needs a speech model far too large to ship inside the app, so CQ
 * fetches it the first time. This window is the whole of that: what will be
 * downloaded, how big it is, how far it has got, and — since a 547 MB download
 * is not always convenient — a way to stop and pick it up later.
 *
 * All of the work is in Rust; this listens to `dictate-progress` and draws it.
 */
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

type ModelStatus = {
  id: string;
  label: string;
  total: number;
  have: boolean;
  downloaded: number;
};

type VocabView = { terms: string[]; max: number };

type MicInfo = { id: string; name: string; maker: string | null; is_default: boolean };
type Mics = { devices: MicInfo[]; chosen: string | null; locked: boolean };

type Progress = {
  id: string;
  label: string;
  downloaded: number;
  total: number;
  state: "downloading" | "verifying" | "done" | "failed" | "cancelled";
  message: string | null;
};

let root: HTMLElement;
let models: ModelStatus[] = [];
/** The last progress seen per model, so a redraw doesn't lose the bar. */
const live = new Map<string, Progress>();
let busy = false;
/** The end-to-end check: what it is doing, and what came back. */
let trying: "idle" | "listening" | "thinking" = "idle";
let heard: string | null = null;
let heardError: string | null = null;
let mics: Mics = { devices: [], chosen: null, locked: false };
/** Set when a locked microphone was missing and CQ recorded off another. */
let substituted: string | null = null;
let vocab: VocabView = { terms: [], max: 48 };

/** Sizes here are hundreds of megabytes, so one decimal place is plenty. */
export function size(bytes: number): string {
  const mb = bytes / 1_000_000;
  if (mb >= 1000) return `${(mb / 1000).toFixed(1)} GB`;
  return `${Math.round(mb)} MB`;
}

/** Whole percent, clamped — a bar that reads 101% looks broken. */
export function percent(done: number, total: number): number {
  if (total <= 0) return 0;
  return Math.max(0, Math.min(100, Math.round((done / total) * 100)));
}

/** One term per line is what the box shows; the file keeps an array. */
export function vocabText(terms: string[]): string {
  return terms.join("\n");
}

/** Split what was typed back into terms, without deciding what is valid — the
 *  backend does the trimming and de-duplicating, so both agree. */
export function vocabTerms(text: string): string[] {
  return text
    .split("\n")
    .map((t) => t.trim())
    .filter((t) => t !== "");
}

/**
 * What to say under the box. The cap is real: past it whisper keeps only the
 * last part of the prompt and says so in a log nobody reads, so the count has
 * to be visible here.
 */
export function vocabNote(count: number, max: number): string {
  if (count === 0) return `Names CQ should expect to hear — one per line, up to ${max}.`;
  if (count > max) {
    const dropped = count - max;
    return `${count} terms — only the first ${max} are used, so ${dropped} ${dropped === 1 ? "is" : "are"} ignored.`;
  }
  if (count === max) return `${count} terms — that is the limit.`;
  return `${count} of ${max} terms.`;
}

/**
 * What to call a device in the list. Two microphones can share a name — a pair
 * of identical USB interfaces, say — so the maker is added when it would tell
 * them apart, and left off when it would just be noise.
 */
export function micLabel(m: MicInfo, all: MicInfo[]): string {
  const sameName = all.filter((o) => o.name === m.name).length > 1;
  const base = sameName && m.maker ? `${m.name} (${m.maker})` : m.name;
  return m.is_default ? `${base} — system default` : base;
}

/** The line under each model's name. Pure, so the wording can be tested. */
export function detail(m: ModelStatus, p: Progress | undefined): string {
  if (p?.state === "failed") return p.message ?? "Download failed.";
  if (p?.state === "verifying") return "Checking the download…";
  if (m.have || p?.state === "done") return `Ready · ${size(m.total)}`;
  if (p?.state === "downloading") {
    return `${size(p.downloaded)} of ${size(p.total)} · ${percent(p.downloaded, p.total)}%`;
  }
  if (m.downloaded > 0) {
    return `Paused at ${percent(m.downloaded, m.total)}% · ${size(m.total)} in total`;
  }
  return size(m.total);
}

function render() {
  // Speech is what dictation cannot do without; the rewrite model is optional
  // and its absence only means the transcript is pasted as heard.
  const speech = models.find((m) => m.id === "whisper");
  const allDone = !!speech?.have;
  const everything = models.length > 0 && models.every((m) => m.have);
  const failed = [...live.values()].some((p) => p.state === "failed");

  root.innerHTML = `
    <div class="titlebar" data-tauri-drag-region>
      <div class="titlebar-brand">
        <img class="titlebar-logo" src="/logo-white.png" alt="" />
        <span>Dictation</span>
      </div>
    </div>
    <div class="dc-body">
      <p class="dc-lede">
        Hold the right <b>⌥</b> key, speak, let go. Dictation runs on this Mac:
        what you say is transcribed here and never sent anywhere.${
          models.find((m) => m.id === "rewrite")?.have
            ? " Hold <b>Shift</b> too to paste it exactly as heard, without the clean-up."
            : ""
        }
      </p>
      <div class="dc-models">
        ${models
          .map((m) => {
            const p = live.get(m.id);
            const done = m.have || p?.state === "done";
            const pct = p
              ? percent(p.downloaded, p.total)
              : percent(m.downloaded, m.total);
            const active = p?.state === "downloading" || p?.state === "verifying";
            return `
              <div class="dc-model${done ? " done" : ""}${p?.state === "failed" ? " failed" : ""}">
                <div class="dc-model-top">
                  <span class="dc-name">${m.label}</span>
                  <span class="dc-state">${done ? "✓" : ""}</span>
                </div>
                <div class="dc-bar" role="progressbar"
                     aria-valuenow="${done ? 100 : pct}" aria-valuemin="0" aria-valuemax="100">
                  <div class="dc-fill${active ? " active" : ""}" style="width:${done ? 100 : pct}%"></div>
                </div>
                <div class="dc-detail">${detail(m, p)}</div>
              </div>`;
          })
          .join("")}
      </div>
      <div class="dc-actions">
        ${
          // A way out at every moment. Mid-download the window used to offer
          // only "Stop", which left no way to dismiss it without abandoning
          // the download it was reporting on.
          everything
            ? `<button class="dc-btn primary" id="dc-close">Done</button>`
            : busy
              ? `<button class="dc-btn" id="dc-cancel">Stop</button>
                 <button class="dc-btn" id="dc-close">Close</button>`
              : `<button class="dc-btn" id="dc-close">Close</button>
                 <button class="dc-btn primary" id="dc-go">${
                   failed || models.some((m) => m.downloaded > 0) ? "Try again" : "Download"
                 }</button>`
        }
      </div>
      ${
        allDone
          ? `<div class="dc-vocab">
               <label class="dc-mic-label" for="dc-vocab">Words to expect</label>
               <textarea id="dc-vocab" class="dc-vocab-box" spellcheck="false"
                 placeholder="Snowflake&#10;Pendo&#10;customer_id">${vocabText(vocab.terms)}</textarea>
               <p class="dc-vocab-note${vocab.terms.length > vocab.max ? " over" : ""}">${vocabNote(vocab.terms.length, vocab.max)}</p>
             </div>
             <div class="dc-mic">
               <label class="dc-mic-row">
                 <span class="dc-mic-label">Microphone</span>
                 <select id="dc-mic" class="dc-select">
                   <option value="">Follow the system default</option>
                   ${mics.devices
                     .map(
                       (m) =>
                         `<option value="${m.id}"${m.id === mics.chosen ? " selected" : ""}>${micLabel(m, mics.devices)}</option>`,
                     )
                     .join("")}
                 </select>
               </label>
               <label class="dc-mic-lock${mics.chosen ? "" : " off"}">
                 <input type="checkbox" id="dc-lock" ${mics.locked ? "checked" : ""} ${mics.chosen ? "" : "disabled"} />
                 <span>Always use this microphone</span>
               </label>
               ${
                 substituted
                   ? `<p class="dc-sub">Recorded with a different microphone — ${substituted} was not connected.</p>`
                   : ""
               }
             </div>
             <div class="dc-try">
               <button class="dc-btn" id="dc-try" ${trying === "idle" ? "" : "disabled"}>
                 ${trying === "listening" ? "Listening…" : trying === "thinking" ? "Transcribing…" : "Test the microphone"}
               </button>
               <span class="dc-try-hint">${
                 trying === "listening"
                   ? "Say something — five seconds."
                   : trying === "thinking"
                     ? "Running it through the engine."
                     : "Records five seconds and transcribes it here."
               }</span>
             </div>
             ${
               heardError
                 ? `<p class="dc-heard error">${heardError}</p>`
                 : heard !== null
                   ? `<p class="dc-heard">${heard || "(nothing was heard)"}</p>`
                   : ""
             }`
          : ""
      }
      <p class="dc-foot">
        ${
          everything
            ? "Dictation is ready, and what you say is cleaned up before it is pasted."
            : allDone
              ? "Dictation works now. The second model cleans up what you say — backtracking and filler — and is optional."
              : "You can close this window — the download carries on, and stopping keeps what has arrived."
        }
      </p>
    </div>`;

  root.querySelector<HTMLButtonElement>("#dc-go")?.addEventListener("click", () => {
    busy = true;
    live.clear();
    render();
    void invoke("dictate_download");
  });
  root.querySelector<HTMLButtonElement>("#dc-cancel")?.addEventListener("click", () => {
    void invoke("dictate_cancel");
  });
  const box = root.querySelector<HTMLTextAreaElement>("#dc-vocab");
  // Saved when the box loses focus, not on every keystroke: the list is a
  // whole thought, and saving mid-word would write half a term to disk.
  box?.addEventListener("blur", async () => {
    const wanted = vocabTerms(box.value);
    if (vocabText(wanted) === vocabText(vocab.terms)) return;
    vocab = await invoke<VocabView>("dictate_set_vocab", { terms: wanted });
    render();
  });
  root.querySelector<HTMLSelectElement>("#dc-mic")?.addEventListener("change", async (e) => {
    const id = (e.target as HTMLSelectElement).value || null;
    // Following the system default and locking are contradictory, so choosing
    // "follow the default" releases the lock rather than leaving it set on
    // nothing.
    const locked = id ? mics.locked : false;
    await invoke("dictate_set_mic", { device: id, locked });
    mics = await invoke<Mics>("dictate_mics");
    render();
  });
  root.querySelector<HTMLInputElement>("#dc-lock")?.addEventListener("change", async (e) => {
    await invoke("dictate_set_mic", {
      device: mics.chosen,
      locked: (e.target as HTMLInputElement).checked,
    });
    mics = await invoke<Mics>("dictate_mics");
    render();
  });
  root.querySelector<HTMLButtonElement>("#dc-try")?.addEventListener("click", () => {
    trying = "listening";
    heard = null;
    heardError = null;
    render();
    // Returns at once; the result arrives as `dictate-heard`. It cannot be
    // awaited, because the command must not hold the main thread for the
    // length of a recording.
    void invoke("dictate_try", { seconds: 5 });
  });
  root.querySelector<HTMLButtonElement>("#dc-close")?.addEventListener("click", () => {
    // Through Rust: the web view is not granted `allow-close`, so closing its
    // own window from here is refused by the ACL and does nothing at all.
    void invoke("dictate_close");
  });
}

async function refresh() {
  models = await invoke<ModelStatus[]>("dictate_models");
  // Re-read every time: microphones come and go while the window is open.
  try {
    mics = await invoke<Mics>("dictate_mics");
    vocab = await invoke<VocabView>("dictate_vocab");
  } catch {
    mics = { devices: [], chosen: null, locked: false };
  }
  render();
}

export async function start(app: HTMLElement) {
  root = app;
  await refresh();
  await listen<{ device: string; instead_of: string | null }>("dictate-using", ({ payload }) => {
    substituted = payload.instead_of;
  });
  await listen<{ text: string | null; error: string | null }>("dictate-heard", ({ payload }) => {
    heard = payload.text;
    heardError = payload.error;
    trying = "idle";
    render();
  });
  await listen<boolean>("dictate-listening", ({ payload }) => {
    trying = payload ? "listening" : "thinking";
    render();
  });
  await listen<Progress>("dictate-progress", async ({ payload }) => {
    live.set(payload.id, payload);
    if (payload.state === "done" || payload.state === "failed" || payload.state === "cancelled") {
      // Ask Rust rather than assume: it is the one that knows whether the file
      // passed its checksum and was put into place.
      busy = payload.state === "done" && models.some((m) => !m.have && m.id !== payload.id);
      await refresh();
      return;
    }
    busy = true;
    render();
  });
}
