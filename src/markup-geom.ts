/**
 * The markup window's geometry, kept apart from the DOM so it can be tested:
 * fitting the screenshot into the window, mapping the pointer onto the image's
 * own pixels, and shaping an arrowhead.
 */

export type Point = { x: number; y: number };
/** Where the image sits in a box: its scale, and its top-left and size in the box. */
export type Fit = { scale: number; x: number; y: number; w: number; h: number };

/** The image scaled to fit inside the box, never enlarged, and centred. */
export function fit(imageW: number, imageH: number, boxW: number, boxH: number): Fit {
  const scale = Math.max(Math.min(boxW / imageW, boxH / imageH, 1), 1e-6);
  const w = imageW * scale;
  const h = imageH * scale;
  return { scale, x: (boxW - w) / 2, y: (boxH - h) / 2, w, h };
}

/** A point in the box, in the image's own pixels. */
export function toImage(p: Point, f: Fit): Point {
  return { x: (p.x - f.x) / f.scale, y: (p.y - f.y) / f.scale };
}

/** How long an arrowhead is for a line of the given width. */
export const headLength = (width: number) => Math.max(width * 4, 12);

/**
 * The two back corners of an arrowhead pointing at `to`, or null for an arrow
 * with no length. The head is a little narrower than it is long.
 */
export function arrowHead(from: Point, to: Point, width: number): [Point, Point] | null {
  const dx = to.x - from.x;
  const dy = to.y - from.y;
  const len = Math.hypot(dx, dy);
  if (len < 1e-6) return null;
  const ux = dx / len;
  const uy = dy / len;
  const size = headLength(width);
  const baseX = to.x - ux * size;
  const baseY = to.y - uy * size;
  const half = size * 0.55;
  return [
    { x: baseX - uy * half, y: baseY + ux * half },
    { x: baseX + uy * half, y: baseY - ux * half },
  ];
}
