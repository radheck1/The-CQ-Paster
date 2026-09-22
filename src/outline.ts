/**
 * The Jotter note as data: a flat list of lines, each with an indent depth.
 *
 * Structure is implied by depth rather than stored as a tree. A line's
 * sub-lines are the lines straight after it that sit deeper — exactly what is
 * on screen — so every operation here can be checked by eye. Nothing in this
 * file touches the DOM.
 *
 * Every function returns new arrays and new line objects and never mutates its
 * input, so an undo snapshot is just a reference to an old array.
 */

export type Line = { text: string; depth: number; done: boolean };
export type Pos = { line: number; offset: number };
/** The result of an edit: the new lines and where the caret goes. */
export type Edit = { lines: Line[]; caret: Pos };

/** Deepest indent. Four levels: 0 through 3. */
export const MAX_DEPTH = 3;

export const blankLine = (depth = 0): Line => ({ text: "", depth, done: false });

/** A note with nothing in it: the single empty line a new folder starts with. */
export const isPristine = (lines: Line[]) => lines.length === 1 && lines[0].text === "";

export function comparePos(a: Pos, b: Pos): number {
  return a.line - b.line || a.offset - b.offset;
}

export function ordered(a: Pos, b: Pos): [Pos, Pos] {
  return comparePos(a, b) <= 0 ? [a, b] : [b, a];
}

/** Index just past line `i`'s sub-lines — everything after it that sits deeper. */
export function subtreeEnd(lines: Line[], i: number): number {
  let j = i + 1;
  while (j < lines.length && lines[j].depth > lines[i].depth) j++;
  return j;
}

/**
 * Which lines read as crossed out.
 *
 * Crossing out a line crosses out its whole group, so a line is `crossed` when
 * it or any line it sits under is marked done. `inherited` is the part that
 * comes from above: those lines can't be brought back on their own, only by
 * bringing back the line that crossed them out.
 */
export function crossings(lines: Line[]): { crossed: boolean[]; inherited: boolean[] } {
  const crossed: boolean[] = [];
  const inherited: boolean[] = [];
  const above: { depth: number; crossed: boolean }[] = [];
  for (const l of lines) {
    while (above.length && above[above.length - 1].depth >= l.depth) above.pop();
    const fromAbove = above.length > 0 && above[above.length - 1].crossed;
    const c = fromAbove || l.done;
    crossed.push(c);
    inherited.push(fromAbove);
    above.push({ depth: l.depth, crossed: c });
  }
  return { crossed, inherited };
}

/** Lines with something written on them that aren't crossed out. */
export function openCount(lines: Line[]): number {
  const { crossed } = crossings(lines);
  return lines.filter((l, i) => l.text.trim() !== "" && !crossed[i]).length;
}

/**
 * Repair indentation so every line sits at most one level under the line above
 * it, keeping siblings as siblings.
 *
 * Deleting or joining lines can leave a line two levels under its new
 * neighbour. Clamping each line to "the line above, plus one" would fix that but
 * also turn a run of siblings into a staircase, so this re-derives each depth
 * from the stack of lines it originally sat under instead. Well-formed input
 * comes back unchanged.
 */
export function normalize(lines: Line[]): Line[] {
  if (lines.length === 0) return [blankLine()];
  const above: { was: number; now: number }[] = [];
  return lines.map((l) => {
    const was = Math.max(0, Math.floor(l.depth) || 0);
    while (above.length && above[above.length - 1].was >= was) above.pop();
    const now = above.length ? Math.min(above[above.length - 1].now + 1, MAX_DEPTH) : 0;
    above.push({ was, now });
    return now === l.depth ? l : { ...l, depth: now };
  });
}

/** Rebuild lines from untrusted stored data, dropping nothing that has text. */
export function repairLines(raw: unknown): Line[] {
  const items = Array.isArray(raw) ? raw : [];
  const lines: Line[] = [];
  for (const r of items) {
    if (!r || typeof r !== "object") continue;
    const o = r as Record<string, unknown>;
    // A stored newline would put two lines in one element; split it instead.
    const parts = (typeof o.text === "string" ? o.text : "").split(/\r\n?|\n/);
    const depth = Number.isInteger(o.depth) ? (o.depth as number) : 0;
    for (const text of parts) lines.push({ text, depth, done: o.done === true });
  }
  return normalize(lines);
}

const replaceRange = (lines: Line[], from: number, to: number, add: Line[]) => [
  ...lines.slice(0, from),
  ...add,
  ...lines.slice(to),
];

/** Remove the text between two positions, joining the lines at either end. */
export function deleteRange(lines: Line[], a: Pos, b: Pos): Edit {
  const [s, e] = ordered(a, b);
  const text = lines[s.line].text.slice(0, s.offset) + lines[e.line].text.slice(e.offset);
  const next = replaceRange(lines, s.line, e.line + 1, [{ ...lines[s.line], text }]);
  // Selecting everything and deleting it should leave a fresh note, not an
  // empty line that is still crossed out.
  if (next.length === 1 && text === "") return { lines: [blankLine()], caret: { line: 0, offset: 0 } };
  return { lines: normalize(next), caret: { ...s } };
}

/**
 * Replace the text between two positions with `text`, which may span lines.
 *
 * Leading tabs on pasted lines become indentation, relative to the first pasted
 * line. That is the format `serialize` writes a copied block in, so copying and
 * pasting within the Jotter keeps a group's shape.
 */
export function insertText(lines: Line[], a: Pos, b: Pos, text: string): Edit {
  const base = comparePos(a, b) === 0 ? { lines, caret: a } : deleteRange(lines, a, b);
  const at = base.caret;
  const cur = base.lines[at.line];
  const head = cur.text.slice(0, at.offset);
  const tail = cur.text.slice(at.offset);
  const parts = text.split(/\r\n?|\n/);

  if (parts.length === 1) {
    const line = { ...cur, text: head + parts[0] + tail };
    return {
      lines: replaceRange(base.lines, at.line, at.line + 1, [line]),
      caret: { line: at.line, offset: at.offset + parts[0].length },
    };
  }

  const tabs = parts.map((p) => /^\t*/.exec(p)![0].length);
  const bodies = parts.map((p, i) => p.slice(tabs[i]));
  const last = parts.length - 1;
  const added = bodies.map((body, i): Line => {
    if (i === 0) return { ...cur, text: head + body };
    const depth = Math.max(0, Math.min(MAX_DEPTH, cur.depth + tabs[i] - tabs[0]));
    return { text: i === last ? body + tail : body, depth, done: false };
  });
  return {
    lines: normalize(replaceRange(base.lines, at.line, at.line + 1, added)),
    caret: { line: at.line + last, offset: bodies[last].length },
  };
}

/**
 * Return: break the line at the caret.
 *
 * The new line lands at the same depth, with one exception that keeps a group
 * together: breaking a line that has sub-lines makes the new line the first of
 * them. At the parent's depth it would sit between the parent and its
 * sub-lines and take them over as its own.
 *
 * At the very start of a line that has text, the new line goes above instead,
 * so the line keeps its crossed-out state and its sub-lines and simply moves
 * down.
 */
export function splitLine(lines: Line[], a: Pos, b: Pos): Edit {
  const base = comparePos(a, b) === 0 ? { lines, caret: a } : deleteRange(lines, a, b);
  const at = base.caret;
  const cur = base.lines[at.line];

  if (at.offset === 0 && cur.text !== "") {
    return {
      lines: replaceRange(base.lines, at.line, at.line, [blankLine(cur.depth)]),
      caret: { line: at.line + 1, offset: 0 },
    };
  }

  const next = base.lines[at.line + 1];
  const hasSubLines = next !== undefined && next.depth > cur.depth;
  const tail = cur.text.slice(at.offset);
  const kept: Line = { ...cur, text: cur.text.slice(0, at.offset) };
  const added: Line = {
    text: tail,
    depth: hasSubLines ? cur.depth + 1 : cur.depth,
    done: tail !== "" && cur.done,
  };
  return {
    lines: replaceRange(base.lines, at.line, at.line + 1, [kept, added]),
    caret: { line: at.line + 1, offset: 0 },
  };
}

/**
 * Tab / Shift+Tab over lines `from`..`to`: move them one level in or out,
 * together with the sub-lines of the last of them, so a group moves as one.
 *
 * Returns null when the move isn't possible: indenting the first line, a line
 * that is already under the line above, or past the deepest level.
 */
export function shiftDepth(lines: Line[], from: number, to: number, delta: 1 | -1): Line[] | null {
  const end = subtreeEnd(lines, to);
  const moving = lines.slice(from, end);
  if (delta === 1) {
    if (from === 0 || lines[from].depth > lines[from - 1].depth) return null;
    if (moving.some((l) => l.depth + 1 > MAX_DEPTH)) return null;
  } else if (!moving.some((l) => l.depth > 0)) {
    return null;
  }
  return normalize(
    lines.map((l, k) => (k >= from && k < end ? { ...l, depth: Math.max(0, l.depth + delta) } : l)),
  );
}

/**
 * Backspace with the caret at the start of a line.
 *
 * An indented line steps out one level first, with its sub-lines — the same as
 * Shift+Tab. A line already at the left edge joins the line above. If that line
 * is empty, the empty line is what goes, so the line being backspaced keeps its
 * indent and its crossed-out state.
 */
export function backspaceAtStart(lines: Line[], i: number): Edit | null {
  const cur = lines[i];
  if (cur.depth > 0) {
    const shifted = shiftDepth(lines, i, i, -1);
    return shifted && { lines: shifted, caret: { line: i, offset: 0 } };
  }
  if (i === 0) return null;
  const prev = lines[i - 1];
  if (prev.text === "") {
    return { lines: normalize(replaceRange(lines, i - 1, i, [])), caret: { line: i - 1, offset: 0 } };
  }
  return {
    lines: normalize(replaceRange(lines, i - 1, i + 1, [{ ...prev, text: prev.text + cur.text }])),
    caret: { line: i - 1, offset: prev.text.length },
  };
}

/** Forward delete with the caret at the end of a line: pull the next line up. */
export function deleteAtEnd(lines: Line[], i: number): Edit | null {
  if (i + 1 >= lines.length) return null;
  const cur = lines[i];
  const next = lines[i + 1];
  if (cur.text === "") {
    return { lines: normalize(replaceRange(lines, i, i + 1, [])), caret: { line: i, offset: 0 } };
  }
  return {
    lines: normalize(replaceRange(lines, i, i + 2, [{ ...cur, text: cur.text + next.text }])),
    caret: { line: i, offset: cur.text.length },
  };
}

/**
 * Click a dot: cross the line out, or bring it back.
 *
 * Returns null for a line crossed out by a line above it — the group is
 * brought back from the top, not piecemeal.
 */
export function toggleDone(lines: Line[], i: number): Line[] | null {
  if (crossings(lines).inherited[i]) return null;
  return lines.map((l, k) => (k === i ? { ...l, done: !l.done } : l));
}

/** A selection as plain text, with sub-lines written as leading tabs. */
export function serialize(lines: Line[], a: Pos, b: Pos): string {
  const [s, e] = ordered(a, b);
  if (s.line === e.line) return lines[s.line].text.slice(s.offset, e.offset);
  const picked = lines.slice(s.line, e.line + 1);
  const min = Math.min(...picked.map((l) => l.depth));
  const last = picked.length - 1;
  return picked
    .map((l, k) => {
      const end = k === last ? e.offset : l.text.length;
      const start = k === 0 ? s.offset : 0;
      return "\t".repeat(l.depth - min) + l.text.slice(start, end);
    })
    .join("\n");
}

/** Whether anything in the note is crossed out: what Clear Jots would remove. */
export const hasCrossed = (lines: Line[]) => crossings(lines).crossed.some(Boolean);

/**
 * Clear Jots: the note without its crossed-out lines. A crossed-out group goes
 * as a whole, since everything under a crossed-out line reads as crossed out.
 */
export function removeCrossed(lines: Line[]): Line[] {
  const { crossed } = crossings(lines);
  const kept = lines.filter((_, i) => !crossed[i]);
  return kept.length ? normalize(kept) : [blankLine()];
}

/**
 * Undo for Clear Jots: put the lines it removed from `before` back into the
 * note as it is `now`.
 *
 * Each run of removed lines goes back straight after the line it followed. The
 * note may have been edited since, so the lines that stayed are found again in
 * `now` by their text and indent; a run whose line has since been changed or
 * deleted follows the nearest earlier line that is still there. Everything in
 * `now` is kept, so Undo never costs text.
 */
export function restoreCrossed(before: Line[], now: Line[]): Line[] {
  const { crossed } = crossings(before);
  const kept: Line[] = [];
  // runs[0] is what came before the first kept line; runs[k + 1] follows kept[k].
  const runs: Line[][] = [[]];
  before.forEach((l, i) => {
    if (crossed[i]) {
      runs[runs.length - 1].push(l);
    } else {
      kept.push(l);
      runs.push([]);
    }
  });
  // Everything crossed out leaves a blank note, which isn't text to keep.
  const current = kept.length === 0 && isPristine(now) ? [] : now;
  const found = matchLines(kept, current);
  // What goes back after each line of `current`; -1 is the top of the note.
  const after = new Map<number, Line[]>([[-1, [...runs[0]]]]);
  let home = -1;
  kept.forEach((_, k) => {
    if (found[k] >= 0) home = found[k];
    after.set(home, [...(after.get(home) ?? []), ...runs[k + 1]]);
  });
  const out = [...after.get(-1)!];
  current.forEach((l, i) => out.push(l, ...(after.get(i) ?? [])));
  return normalize(out);
}

/**
 * Pair lines of `a` with lines of `b` that have the same text and indent,
 * keeping their order (a longest common subsequence). `found[i]` is the index
 * of `a[i]` in `b`, or -1. An unchanged top and bottom are paired directly, so
 * only the edited middle needs the table.
 */
function matchLines(a: Line[], b: Line[]): number[] {
  const ka = a.map((l) => `${l.depth}:${l.text}`);
  const kb = b.map((l) => `${l.depth}:${l.text}`);
  const found = ka.map(() => -1);
  let s = 0;
  while (s < ka.length && s < kb.length && ka[s] === kb[s]) {
    found[s] = s;
    s++;
  }
  let ea = ka.length;
  let eb = kb.length;
  while (ea > s && eb > s && ka[ea - 1] === kb[eb - 1]) {
    ea--;
    eb--;
    found[ea] = eb;
  }
  const n = ea - s;
  const m = eb - s;
  // lcs[i][j]: how many lines ka[s + i ..] and kb[s + j ..] have in common.
  const lcs = Array.from({ length: n + 1 }, () => new Uint32Array(m + 1));
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      lcs[i][j] = ka[s + i] === kb[s + j] ? lcs[i + 1][j + 1] + 1 : Math.max(lcs[i + 1][j], lcs[i][j + 1]);
    }
  }
  for (let i = 0, j = 0; i < n && j < m; ) {
    if (ka[s + i] === kb[s + j]) {
      found[s + i] = s + j;
      i++;
      j++;
    } else if (lcs[i + 1][j] >= lcs[i][j + 1]) {
      i++;
    } else {
      j++;
    }
  }
  return found;
}

/**
 * Put a dictation at the top of a note.
 *
 * Newest first, with a blank line between entries: the pad is a log you go
 * back to, and what you want is nearly always the last thing you said. A
 * multi-line transcript keeps its lines, each flat — a transcript has no
 * outline to it.
 *
 * Separate from the editor so it can be tested without a window.
 */
export function withDictation(existing: Line[], text: string): Line[] {
  const added = text
    .split("\n")
    .map((t) => t.trim())
    .filter((t) => t !== "")
    .map((t) => ({ text: t, depth: 0, done: false }));
  if (added.length === 0) return existing;
  // An untouched note is a single empty line; replacing it keeps a blank first
  // line from sitting above every dictation forever.
  if (isPristine(existing)) return added;
  return [...added, blankLine(), ...existing];
}
