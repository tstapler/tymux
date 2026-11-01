// Regenerates ../golden/<name>.json (the case's steps plus xterm.js's checkpoint after each) from cases.mjs.
//
// Each case is a list of steps ({write: "<text>"} or {resize: [rows, cols]}) replayed into
// xterm.js headless - the browser terminal's engine, i.e. the reference for what a user sees. After
// every step a checkpoint records the visible grid, the scrollback and the cursor/scroll-region/tab
// state, so crates/vt100/tests/fidelity_resize.rs can diff the Rust emulator step by step.
//
// Usage: bun install && bun generate.mjs [name-substring]
import xterm from "@xterm/headless";
import { Unicode11Addon } from "@xterm/addon-unicode11";
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { cases } from "./cases.mjs";

const { Terminal } = xterm;
const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const filter = process.argv[2] ?? "";

const UNDERLINE_STYLES = ["", "single", "double", "curly", "dotted", "dashed"];

function color(mode, value) {
  if (mode === 0) return "default";
  return mode === 0x03000000 ? `rgb:${value.toString(16).padStart(6, "0")}` : `p:${value}`;
}

// [text, fg, bg, flags, ulStyle, ulColor, link] - same tuple as the corpus goldens.
function tuple(c, links) {
  const urlId = c.extended.urlId;
  const linkData = urlId ? links.getLinkData(urlId) : undefined;
  const ulStyle = urlId || !c.isUnderline() ? 0 : c.getUnderlineStyle() || 1;
  const flags = [
    c.isBold() && "bold", c.isDim() && "faint", c.isItalic() && "italic",
    ulStyle !== 0 && "underline", c.isInverse() && "reverse", c.isBlink() && "blink",
    c.isInvisible() && "conceal", c.isStrikethrough() && "strike", c.isOverline() && "overline",
  ].filter(Boolean).join(",");
  return [
    c.getWidth() === 0 ? "" : c.getChars() || " ",
    color(c.getFgColorMode(), c.getFgColor()),
    color(c.getBgColorMode(), c.getBgColor()),
    flags,
    UNDERLINE_STYLES[ulStyle],
    urlId || ulStyle === 0 ? "default" : color(c.getUnderlineColorMode(), c.getUnderlineColor()),
    linkData ? `${linkData.uri}\u0000${linkData.id ?? ""}` : "",
  ];
}

const isDefaultAttrs = (t) => t[1] === "default" && t[2] === "default" && t[3] === "" && t[4] === "" && t[5] === "default" && t[6] === "";

// One line as a positional array, trailing blanks trimmed. A cell is its bare text when it has no
// attributes (" " = blank, "" = the second half of a wide glyph) and the full tuple otherwise.
function encodeLine(line, cols, links) {
  const out = [];
  for (let x = 0; x < Math.min(cols, line.length); x++) {
    const t = tuple(line.getCell(x), links);
    out.push(isDefaultAttrs(t) ? t[0] : t);
  }
  while (out.length > 0 && out[out.length - 1] === " ") out.pop();
  return out;
}

function checkpoint(term, step, rows, cols) {
  const buf = term.buffer.active;
  const core = term._core;
  const cb = core.buffer;
  const links = core._oscLinkService;
  const grid = [];
  for (let y = 0; y < rows; y++) grid.push(encodeLine(buf.getLine(buf.baseY + y), cols, links));
  const scrollback = [];
  for (let y = 0; y < buf.baseY; y++) scrollback.push(encodeLine(buf.getLine(y), cols, links));
  const wholeScreen = cb.scrollTop === 0 && cb.scrollBottom === rows - 1;
  return {
    step, rows, cols, grid, scrollback,
    // soft-wrap continuation flag per scrollback+screen line (xterm: line.isWrapped), debugging aid
    wrapped: Array.from({ length: buf.baseY + rows }, (_, y) => (buf.getLine(y).isWrapped ? "1" : "0")).join(""),
    cursor: { row: buf.cursorY, col: Math.min(cb.x, cols - 1), pendingWrap: cb.x >= cols },
    altScreen: buf.type === "alternate",
    scrollTop: wholeScreen ? 0 : cb.scrollTop + 1,
    scrollBottom: wholeScreen ? 0 : cb.scrollBottom + 1,
    tabStops: Object.keys(cb.tabs).map(Number).filter((c) => c !== 0).sort((a, b) => a - b),
  };
}

async function run(spec) {
  let { rows, cols } = spec;
  const term = new Terminal({ cols, rows, allowProposedApi: true, scrollback: spec.scrollback });
  term.loadAddon(new Unicode11Addon());
  term.unicode.activeVersion = "11";
  const checkpoints = [];
  for (const [i, s] of spec.steps.entries()) {
    if (s.resize) {
      [rows, cols] = s.resize;
      term.resize(cols, rows);
    } else {
      await new Promise((resolve) => term.write(s.write, resolve));
    }
    checkpoints.push(checkpoint(term, i, rows, cols));
  }
  term.dispose();
  return checkpoints;
}

mkdirSync(join(root, "golden"), { recursive: true });
let n = 0;
for (const c of cases) {
  if (!c.name.includes(filter)) continue;
  const spec = { rows: c.rows ?? 24, cols: c.cols ?? 80, scrollback: c.scrollback ?? 1000, note: c.note ?? "", steps: c.steps };
  writeFileSync(join(root, "golden", `${c.name}.json`), JSON.stringify({ ...spec, checkpoints: await run(spec) }) + "\n");
  n++;
}
console.log(`wrote ${n} cases`);
