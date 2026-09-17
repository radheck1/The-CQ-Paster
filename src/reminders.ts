/**
 * Jotter reminders (macOS only): the clock button beside the jotpad pill, its
 * settings menu, and the reminder card, which runs in a small window of its own.
 *
 * Settings belong to each folder and are saved with the Jotter document. The
 * schedule runs in the backend (`reminders.rs`), which reads them from every
 * save, so nothing here depends on the control panel being open.
 */
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import * as jotter from "./jotter";

/** The alert sounds macOS ships in /System/Library/Sounds. */
const SOUNDS = ["Basso", "Blow", "Bottle", "Frog", "Funk", "Glass", "Hero", "Morse", "Ping", "Pop", "Purr", "Sosumi", "Submarine", "Tink"];
const PRESETS = [15, 30, 60, 120];
const DAY_LETTERS = ["S", "M", "T", "W", "T", "F", "S"];

const icon = (body: string, size: number) =>
  `<svg viewBox="0 0 24 24" width="${size}" height="${size}" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${body}</svg>`;
const CLOCK_ICON = icon(`<circle cx="12" cy="12" r="9"/><polyline points="12 7 12 12 15.5 14"/>`, 15);
const JOTPAD_ICON = icon(`<rect x="5" y="4" width="14" height="18" rx="2"/><path d="M9 2v4M15 2v4M9 11h6M9 15h4"/>`, 11);

const esc = (s: string) =>
  s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
const everyLabel = (m: number) => (m % 60 === 0 ? `${m / 60} hour${m === 60 ? "" : "s"}` : `${m} min`);
const toTime = (m: number) => `${String(Math.floor(m / 60)).padStart(2, "0")}:${String(m % 60).padStart(2, "0")}`;
function fromTime(value: string): number | null {
  const m = /^(\d{1,2}):(\d{2})/.exec(value);
  return m ? Math.min(23 * 60 + 59, Number(m[1]) * 60 + Number(m[2])) : null;
}

// ---------------------------------------------------------------------------
// The clock button and its menu
// ---------------------------------------------------------------------------

let open = false;
let wired = false;
/** The folder the control was last drawn for. */
let drawnFor = -1;

export function control(): string {
  const r = jotter.reminder();
  const name = esc(jotter.activeName());
  drawnFor = jotter.activeId();
  return `<div class="rem-wrap">
      <button class="rem-btn${r.on ? " on" : ""}" id="rem-btn" aria-haspopup="true" aria-expanded="${open}"
              title="Reminders ${r.on ? "on" : "off"} for “${name}”">${CLOCK_ICON}</button>
      ${open ? `<div class="rem-menu">${menu(r, name)}</div>` : ""}
    </div>`;
}

function menu(r: jotter.Reminder, name: string): string {
  const option = (value: string, label: string, current: string) =>
    `<option value="${value}"${value === current ? " selected" : ""}>${label}</option>`;
  const every = r.customEvery ? "custom" : String(r.every);
  const hours =
    r.hours === "working"
      ? `<div class="rem-hint">Monday to Friday, 9 AM – 5 PM</div>`
      : `<div class="rem-field">
           <span class="rem-label">From</span>
           <input type="time" id="rem-start" value="${toTime(r.start)}" />
           <span class="rem-to">to</span>
           <input type="time" id="rem-end" value="${toTime(r.end)}" />
         </div>
         <div class="rem-days" role="group" aria-label="Days">
           ${DAY_LETTERS.map(
             (d, i) =>
               `<button type="button" class="rem-day${r.days.includes(i) ? " on" : ""}" data-day="${i}" aria-pressed="${r.days.includes(i)}">${d}</button>`,
           ).join("")}
         </div>`;
  return `
      <div class="rem-head">
        <div class="rem-heading">
          <div class="rem-title">Reminders</div>
          <div class="rem-sub">for “${name}”</div>
        </div>
        <label class="rem-switch" title="Turn reminders ${r.on ? "off" : "on"}">
          <input type="checkbox" id="rem-on"${r.on ? " checked" : ""} />
          <span class="rem-track"></span>
        </label>
      </div>
      <fieldset class="rem-body"${r.on ? "" : " disabled"}>
        <div class="rem-field">
          <span class="rem-label">Every</span>
          <select id="rem-every">
            ${PRESETS.map((m) => option(String(m), everyLabel(m), every)).join("")}
            ${option("custom", "Custom…", every)}
          </select>
          ${
            r.customEvery
              ? `<input type="number" id="rem-every-custom" min="5" max="720" step="5" value="${r.every}" /><span class="rem-unit">min</span>`
              : ""
          }
        </div>
        <div class="rem-field">
          <span class="rem-label">When</span>
          <select id="rem-hours">
            ${option("working", "Working hours", r.hours)}
            ${option("custom", "Custom hours", r.hours)}
          </select>
        </div>
        ${hours}
        <div class="rem-field">
          <span class="rem-label">Sound</span>
          <select id="rem-sound">
            ${option("", "None", r.sound)}
            ${SOUNDS.map((s) => option(s, s, r.sound)).join("")}
          </select>
        </div>
      </fieldset>
      <div class="rem-foot">
        <span class="rem-next" id="rem-next"></span>
        <button type="button" class="rem-test" id="rem-test">Show a test reminder</button>
      </div>`;
}

/** Wire the control after it has been drawn. */
export function wire() {
  if (!wired) {
    wired = true;
    document.addEventListener("mousedown", (e) => {
      if (open && !(e.target as HTMLElement).closest(".rem-wrap")) close();
    });
    document.addEventListener("keydown", (e) => {
      if (open && e.key === "Escape") close();
    });
  }
  const wrap = document.querySelector<HTMLElement>(".rem-wrap");
  if (!wrap) return;
  const q = <T extends HTMLElement>(sel: string) => wrap.querySelector<T>(sel);
  q("#rem-btn")?.addEventListener("click", () => {
    open = !open;
    redraw();
  });
  if (!open) return;

  const set = (patch: Partial<jotter.Reminder>) => {
    jotter.setReminder(patch);
    redraw();
  };
  const value = (e: Event) => (e.target as HTMLInputElement | HTMLSelectElement).value;

  q("#rem-on")?.addEventListener("change", (e) => set({ on: (e.target as HTMLInputElement).checked }));
  q("#rem-every")?.addEventListener("change", (e) => {
    const v = value(e);
    set(v === "custom" ? { customEvery: true } : { customEvery: false, every: Number(v) });
  });
  q("#rem-every-custom")?.addEventListener("change", (e) => {
    const n = Math.round(Number(value(e)));
    set({ every: Number.isFinite(n) ? Math.min(720, Math.max(5, n)) : 60 });
  });
  q("#rem-hours")?.addEventListener("change", (e) => {
    set(
      value(e) === "working"
        ? { hours: "working", ...jotter.WORKING_HOURS, days: [...jotter.WORKING_HOURS.days] }
        : { hours: "custom" },
    );
  });
  q("#rem-start")?.addEventListener("change", (e) => {
    const m = fromTime(value(e));
    if (m !== null) set({ start: m });
  });
  q("#rem-end")?.addEventListener("change", (e) => {
    const m = fromTime(value(e));
    if (m !== null) set({ end: m });
  });
  wrap.querySelectorAll<HTMLButtonElement>("[data-day]").forEach((b) =>
    b.addEventListener("click", () => {
      const day = Number(b.dataset.day);
      const days = jotter.reminder().days;
      set({ days: days.includes(day) ? days.filter((d) => d !== day) : [...days, day].sort((a, b) => a - b) });
    }),
  );
  q("#rem-sound")?.addEventListener("change", (e) => {
    const sound = value(e);
    set({ sound });
    if (sound) invoke("reminder_preview_sound", { name: sound });
  });
  q("#rem-test")?.addEventListener("click", async () => {
    await jotter.flush(); // the backend reads the note from the latest save
    invoke("reminder_test", { folder: jotter.activeId() });
  });
  showNext();
}

function redraw() {
  const wrap = document.querySelector(".rem-wrap");
  if (!wrap) return;
  wrap.outerHTML = control();
  wire();
}

export function close() {
  if (!open) return;
  open = false;
  redraw();
}

/** Leaving Jotter: the menu starts closed next time. */
export function reset() {
  open = false;
}

/** The open folder may have changed: redraw for it, closed. */
export function refresh() {
  if (jotter.activeId() === drawnFor) return;
  open = false;
  redraw();
}

/** "Next: 2:00 PM", from the backend, which owns the schedule. */
async function showNext() {
  const r = jotter.reminder();
  const el = () => document.querySelector("#rem-next");
  if (!r.on) {
    el()!.textContent = "Reminders are off";
    return;
  }
  await jotter.flush();
  const at = await invoke<number | null>("reminder_next", { folder: jotter.activeId() });
  const target = el(); // the menu may have been redrawn meanwhile
  if (!target) return;
  target.textContent = at == null ? "No reminder times in these hours" : `Next: ${when(new Date(at))}`;
}

function when(d: Date): string {
  const time = d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  const today = new Date();
  const tomorrow = new Date();
  tomorrow.setDate(today.getDate() + 1);
  if (d.toDateString() === today.toDateString()) return time;
  if (d.toDateString() === tomorrow.toDateString()) return `tomorrow, ${time}`;
  return `${d.toLocaleDateString([], { weekday: "long" })}, ${time}`;
}

// ---------------------------------------------------------------------------
// The reminder card (its own window)
// ---------------------------------------------------------------------------

type CardItem = { text: string; depth: number };
type CardFolder = { id: number; name: string; items: CardItem[]; more: number };
type Card = { folders: CardFolder[] };

/** Wait for the backend to hand over a card, draw it, and ask to be shown. */
export async function startCard(app: HTMLElement) {
  await listen<Card>("reminder-card", ({ payload }) => {
    drawCard(app, payload);
    // Measured directly: a hidden window may never get an animation frame.
    const height = Math.ceil(app.querySelector(".rcard")?.getBoundingClientRect().height ?? 0);
    invoke("reminder_present", { height });
  });
}

function drawCard(app: HTMLElement, card: Card) {
  const time = new Date().toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  const folders = card.folders
    .map((f) => {
      const base = Math.min(...f.items.map((i) => i.depth));
      const items = f.items.length
        ? `<ul class="rcard-items">${f.items
            .map(
              (i) =>
                `<li data-depth="${Math.min(i.depth - base, 3)}"><span class="rcard-dot"></span><span class="rcard-text">${esc(i.text)}</span></li>`,
            )
            .join("")}</ul>`
        : `<div class="rcard-empty">Nothing open right now.</div>`;
      return `
        <section class="rcard-folder">
          <div class="rcard-name">${JOTPAD_ICON}<span>${esc(f.name)}</span></div>
          ${items}
          ${f.more ? `<div class="rcard-more">and ${f.more} more</div>` : ""}
        </section>`;
    })
    .join("");
  app.innerHTML = `
    <div class="rcard" role="alert">
      <div class="rcard-head">
        <img class="rcard-logo theme-logo for-dark" src="/logo-white.png" alt="" />
        <img class="rcard-logo theme-logo for-light" src="/logo-black.png" alt="" />
        <span>Jotter reminder</span>
        <span class="rcard-time">${time}</span>
      </div>
      ${folders}
      <div class="rcard-actions">
        <button class="rcard-btn primary" data-act="open">Open Jotter</button>
        <button class="rcard-btn" data-act="snooze">Snooze 10 min</button>
        <button class="rcard-btn quiet" data-act="dismiss">Dismiss</button>
      </div>
    </div>`;
  const first = card.folders[0]?.id;
  app.querySelectorAll<HTMLButtonElement>("[data-act]").forEach((b) =>
    b.addEventListener("click", () => {
      if (b.dataset.act === "open") invoke("reminder_open", { folder: first });
      else if (b.dataset.act === "snooze") invoke("reminder_snooze");
      else invoke("reminder_dismiss");
    }),
  );
}
