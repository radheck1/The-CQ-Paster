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
  const allDone = models.length > 0 && models.every((m) => m.have);
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
        Dictation runs on this Mac. What you say is transcribed here and never
        sent anywhere.
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
          allDone
            ? `<button class="dc-btn primary" id="dc-close">Done</button>`
            : busy
              ? `<button class="dc-btn" id="dc-cancel">Stop</button>`
              : `<button class="dc-btn primary" id="dc-go">${
                  failed || models.some((m) => m.downloaded > 0) ? "Try again" : "Download"
                }</button>`
        }
      </div>
      <p class="dc-foot">
        ${
          allDone
            ? "Dictation is ready to use."
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
  root.querySelector<HTMLButtonElement>("#dc-close")?.addEventListener("click", () => {
    void import("@tauri-apps/api/window").then((w) => w.getCurrentWindow().close());
  });
}

async function refresh() {
  models = await invoke<ModelStatus[]>("dictate_models");
  render();
}

export async function start(app: HTMLElement) {
  root = app;
  await refresh();
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
