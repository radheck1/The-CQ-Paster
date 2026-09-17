/**
 * Shotter's markup window (macOS only): one screenshot, a pen and an arrow, six
 * colours and three stroke sizes. Done saves the result over the original and
 * copies it; Cancel or Escape closes without saving.
 *
 * Marks are kept in the image's own pixels, so Done draws them at full
 * resolution whatever size the window is. A stroke's width is chosen in screen
 * points and converted when the stroke starts, so it looks the size it was
 * picked at.
 */
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { arrowHead, fit, toImage, type Fit, type Point } from "./markup-geom";

type Tool = "pen" | "arrow";
type Mark = { tool: Tool; color: string; width: number; points: Point[] };

const COLORS = [
  { name: "Red", value: "#e5484d" },
  { name: "Yellow", value: "#f5b82e" },
  { name: "Green", value: "#30a46c" },
  { name: "Blue", value: "#2b74c9" },
  { name: "Black", value: "#1a1d24" },
  { name: "White", value: "#ffffff" },
];
const SIZES = [
  { name: "Thin", points: 2, dot: 4 },
  { name: "Medium", points: 4, dot: 7 },
  { name: "Thick", points: 8, dot: 11 },
];
/** Space around the image inside the window, in CSS pixels. Matches `.mk-stage`. */
const STAGE_PAD = 12;

const icon = (body: string) =>
  `<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${body}</svg>`;
const PEN_ICON = icon(`<path d="M12 20h9"/><path d="M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4z"/>`);
const ARROW_ICON = icon(`<line x1="5" y1="19" x2="19" y2="5"/><polyline points="9 5 19 5 19 15"/>`);
const UNDO_ICON = icon(`<polyline points="1 4 1 10 7 10"/><path d="M3.51 15a9 9 0 1 0 2.13-9.36L1 10"/>`);

let shotId: number | null = null;
let image: ImageBitmap | null = null;
let marks: Mark[] = [];
let drawing: Mark | null = null;
let tool: Tool = "pen";
let color = COLORS[0].value;
let size = 1;
let busy = false;
let geometry: Fit = { scale: 1, x: 0, y: 0, w: 0, h: 0 };

let root: HTMLElement;
let stage: HTMLElement;
let canvas: HTMLCanvasElement;
/** The image drawn once at screen size, so a stroke doesn't rescale it every frame. */
const backdrop = document.createElement("canvas");
let frame = 0;

export async function start(app: HTMLElement) {
  root = app;
  app.innerHTML = `
    <div class="titlebar" data-tauri-drag-region>
      <div class="titlebar-brand">
        <img class="titlebar-logo" src="/logo-white.png" alt="" />
        <span>Markup</span>
      </div>
    </div>
    <div class="mk-toolbar">
      <div class="mk-group" role="group" aria-label="Tool">
        <button class="mk-tool active" data-tool="pen" title="Pen" aria-label="Pen">${PEN_ICON}</button>
        <button class="mk-tool" data-tool="arrow" title="Arrow" aria-label="Arrow">${ARROW_ICON}</button>
      </div>
      <span class="mk-sep"></span>
      <div class="mk-group" role="group" aria-label="Colour">
        ${COLORS.map(
          (c, i) =>
            `<button class="mk-swatch${i === 0 ? " active" : ""}" data-color="${c.value}" style="--c:${c.value}" title="${c.name}" aria-label="${c.name}"></button>`,
        ).join("")}
      </div>
      <span class="mk-sep"></span>
      <div class="mk-group" role="group" aria-label="Stroke size">
        ${SIZES.map(
          (s, i) =>
            `<button class="mk-tool mk-size${i === size ? " active" : ""}" data-size="${i}" title="${s.name}" aria-label="${s.name}"><span style="--d:${s.dot}px"></span></button>`,
        ).join("")}
      </div>
      <span class="mk-sep"></span>
      <button class="mk-tool" id="mk-undo" title="Undo (⌘Z)" aria-label="Undo" disabled>${UNDO_ICON}</button>
      <span class="spacer"></span>
      <span class="mk-status" id="mk-status" role="status"></span>
      <button class="ghost" id="mk-cancel">Cancel</button>
      <button class="mk-done" id="mk-done">Done</button>
    </div>
    <div class="mk-stage"><canvas class="mk-canvas"></canvas></div>`;
  stage = app.querySelector(".mk-stage")!;
  canvas = app.querySelector(".mk-canvas")!;
  wire();
  new ResizeObserver(() => layout()).observe(stage);
  await listen<number>("markup-open", ({ payload }) => open(payload));
  const current = await invoke<number | null>("markup_current");
  if (current != null) await open(current);
}

function wire() {
  root.querySelectorAll<HTMLButtonElement>("[data-tool]").forEach((b) =>
    b.addEventListener("click", () => {
      tool = b.dataset.tool as Tool;
      select("[data-tool]", b);
    }),
  );
  root.querySelectorAll<HTMLButtonElement>("[data-color]").forEach((b) =>
    b.addEventListener("click", () => {
      color = b.dataset.color!;
      select("[data-color]", b);
    }),
  );
  root.querySelectorAll<HTMLButtonElement>("[data-size]").forEach((b) =>
    b.addEventListener("click", () => {
      size = Number(b.dataset.size);
      select("[data-size]", b);
    }),
  );
  root.querySelector("#mk-undo")!.addEventListener("click", undo);
  root.querySelector("#mk-cancel")!.addEventListener("click", cancel);
  root.querySelector("#mk-done")!.addEventListener("click", done);

  canvas.addEventListener("pointerdown", (e) => {
    if (!image || busy || e.button !== 0) return;
    canvas.setPointerCapture(e.pointerId);
    const p = toImage(local(e), geometry);
    drawing = {
      tool,
      color,
      width: SIZES[size].points / geometry.scale,
      points: tool === "pen" ? [p] : [p, p],
    };
    schedule();
  });
  canvas.addEventListener("pointermove", (e) => {
    if (!drawing) return;
    const p = toImage(local(e), geometry);
    if (drawing.tool === "arrow") {
      drawing.points[1] = p;
    } else {
      const last = drawing.points[drawing.points.length - 1];
      // Skip moves under a screen pixel: they add points, not shape.
      if (Math.hypot(p.x - last.x, p.y - last.y) * geometry.scale >= 1) drawing.points.push(p);
    }
    schedule();
  });
  const finish = (e: PointerEvent) => {
    if (!drawing) return;
    if (e.type === "pointerup") {
      const p = toImage(local(e), geometry);
      if (drawing.tool === "arrow") drawing.points[1] = p;
      else drawing.points.push(p);
    }
    const [a, b] = [drawing.points[0], drawing.points[drawing.points.length - 1]];
    // An arrow needs some length; a pen tap still leaves a dot.
    const long = Math.hypot(b.x - a.x, b.y - a.y) * geometry.scale >= 4;
    if (drawing.tool === "pen" || long) marks.push(drawing);
    drawing = null;
    updateUndo();
    schedule();
  };
  canvas.addEventListener("pointerup", finish);
  canvas.addEventListener("pointercancel", finish);

  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      e.preventDefault();
      cancel();
    } else if (e.metaKey && !e.shiftKey && e.key.toLowerCase() === "z") {
      e.preventDefault();
      undo();
    } else if (e.metaKey && e.key === "Enter") {
      e.preventDefault();
      done();
    }
  });
}

function select(group: string, active: HTMLElement) {
  root.querySelectorAll(group).forEach((b) => b.classList.toggle("active", b === active));
}

function local(e: PointerEvent): Point {
  const r = canvas.getBoundingClientRect();
  return { x: e.clientX - r.left, y: e.clientY - r.top };
}

async function open(id: number) {
  shotId = id;
  marks = [];
  drawing = null;
  status("");
  updateUndo();
  try {
    const bytes = await invoke<ArrayBuffer>("shot_image", { id });
    const next = await createImageBitmap(new Blob([bytes], { type: "image/png" }));
    if (shotId !== id) return; // another screenshot was opened meanwhile
    image?.close();
    image = next;
    layout();
  } catch (e) {
    image = null;
    status("Couldn't open this screenshot.");
    console.error("[markup] open failed", e);
  }
}

/** Size the canvas to the stage and redraw the image at that size. */
function layout() {
  const w = stage.clientWidth;
  const h = stage.clientHeight;
  const dpr = window.devicePixelRatio || 1;
  canvas.style.width = `${w}px`;
  canvas.style.height = `${h}px`;
  canvas.width = Math.round(w * dpr);
  canvas.height = Math.round(h * dpr);
  if (!image) return draw();
  const inner = fit(image.width, image.height, w - STAGE_PAD * 2, h - STAGE_PAD * 2);
  geometry = { ...inner, x: inner.x + STAGE_PAD, y: inner.y + STAGE_PAD };
  backdrop.width = Math.max(1, Math.round(geometry.w * dpr));
  backdrop.height = Math.max(1, Math.round(geometry.h * dpr));
  const g = backdrop.getContext("2d")!;
  g.imageSmoothingQuality = "high";
  g.drawImage(image, 0, 0, backdrop.width, backdrop.height);
  draw();
}

function schedule() {
  if (frame) return;
  frame = requestAnimationFrame(() => {
    frame = 0;
    draw();
  });
}

function draw() {
  const g = canvas.getContext("2d")!;
  const dpr = window.devicePixelRatio || 1;
  g.setTransform(1, 0, 0, 1, 0, 0);
  g.clearRect(0, 0, canvas.width, canvas.height);
  if (!image) return;
  g.drawImage(backdrop, Math.round(geometry.x * dpr), Math.round(geometry.y * dpr));
  const s = geometry.scale * dpr;
  g.setTransform(s, 0, 0, s, geometry.x * dpr, geometry.y * dpr);
  g.save();
  g.beginPath();
  g.rect(0, 0, image.width, image.height);
  g.clip();
  for (const m of marks) drawMark(g, m);
  if (drawing) drawMark(g, drawing);
  g.restore();
}

function drawMark(g: CanvasRenderingContext2D, m: Mark) {
  g.strokeStyle = m.color;
  g.fillStyle = m.color;
  g.lineWidth = m.width;
  g.lineCap = "round";
  g.lineJoin = "round";
  const pts = m.points;
  if (m.tool === "pen") {
    if (pts.length === 1 || pts.every((p) => p.x === pts[0].x && p.y === pts[0].y)) {
      g.beginPath();
      g.arc(pts[0].x, pts[0].y, m.width / 2, 0, Math.PI * 2);
      g.fill();
      return;
    }
    // Through the midpoints between samples, so a quick stroke comes out smooth.
    g.beginPath();
    g.moveTo(pts[0].x, pts[0].y);
    for (let i = 1; i < pts.length - 1; i++) {
      const mid = { x: (pts[i].x + pts[i + 1].x) / 2, y: (pts[i].y + pts[i + 1].y) / 2 };
      g.quadraticCurveTo(pts[i].x, pts[i].y, mid.x, mid.y);
    }
    const last = pts[pts.length - 1];
    g.lineTo(last.x, last.y);
    g.stroke();
    return;
  }
  const [from, to] = [pts[0], pts[pts.length - 1]];
  const head = arrowHead(from, to, m.width);
  if (!head) return;
  // The shaft stops where the head starts, so its round cap can't poke through the tip.
  const base = { x: (head[0].x + head[1].x) / 2, y: (head[0].y + head[1].y) / 2 };
  g.beginPath();
  g.moveTo(from.x, from.y);
  g.lineTo(base.x, base.y);
  g.stroke();
  g.beginPath();
  g.moveTo(to.x, to.y);
  g.lineTo(head[0].x, head[0].y);
  g.lineTo(head[1].x, head[1].y);
  g.closePath();
  g.fill();
}

function undo() {
  if (busy || marks.length === 0) return;
  marks.pop();
  updateUndo();
  schedule();
}

function updateUndo() {
  root.querySelector<HTMLButtonElement>("#mk-undo")!.disabled = marks.length === 0;
}

function status(text: string) {
  root.querySelector("#mk-status")!.textContent = text;
}

function reset() {
  shotId = null;
  marks = [];
  drawing = null;
  image?.close();
  image = null;
  updateUndo();
  draw();
}

async function cancel() {
  if (busy) return;
  reset();
  await invoke("markup_close").catch(() => {});
}

/** Save over the original and copy it. With nothing drawn, just copy and close. */
async function done() {
  if (busy || shotId == null || !image) return;
  busy = true;
  root.classList.add("mk-busy");
  status("");
  const id = shotId;
  try {
    if (marks.length === 0) {
      await invoke("shot_copy", { id });
      await invoke("markup_close");
    } else {
      const out = document.createElement("canvas");
      out.width = image.width;
      out.height = image.height;
      const g = out.getContext("2d")!;
      g.drawImage(image, 0, 0);
      for (const m of marks) drawMark(g, m);
      const blob = await new Promise<Blob | null>((resolve) => out.toBlob(resolve, "image/png"));
      if (!blob) throw new Error("the image could not be encoded");
      const bytes = new Uint8Array(await blob.arrayBuffer());
      await invoke("shot_save", bytes, { headers: { "shot-id": String(id) } });
    }
    reset();
  } catch (e) {
    status("Couldn't save. Try again.");
    console.error("[markup] save failed", e);
  } finally {
    busy = false;
    root.classList.remove("mk-busy");
  }
}
