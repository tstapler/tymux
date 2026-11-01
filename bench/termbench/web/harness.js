// Browser half of termbench. The Go harness sends pty output as binary
// WebSocket frames; each is written to its pane's terminal and acked after
// the next animation frame, which is the closest portable proxy for "painted".
//
//   Go -> page, text:   {"t":"init","engine":...,"panes":N,"cols":C,"rows":R}
//                       {"t":"dispose"}
//   Go -> page, binary: u16 pane | u16 reserved | u32 seq | payload (little-endian)
//   page -> Go, text:   {"t":"ready"} after init; {"t":"ack","a":[[pane,seq,parseMs,paintMs]...],"lost":webglContextsLost}

const engines = {
  async xterm() {
    const { Terminal } = await import('/node_modules/@xterm/xterm/lib/xterm.mjs');
    return { create: (o) => new Terminal(o) };
  },
  async 'xterm-webgl'() {
    const { Terminal } = await import('/node_modules/@xterm/xterm/lib/xterm.mjs');
    const { WebglAddon } = await import('/node_modules/@xterm/addon-webgl/lib/addon-webgl.mjs');
    return {
      create: (o) => new Terminal({ ...o, allowProposedApi: true }),
      afterOpen: (t) => {
        const a = new WebglAddon();
        // Chrome caps live WebGL contexts per page; past it the oldest are lost
        // and xterm falls back to its slower renderer, so those runs aren't WebGL numbers.
        t.loadAddon(a);
        // The addon's own onContextLoss never fired for evictions Chrome logged,
        // so listen on the canvases directly.
        for (const c of t.element.querySelectorAll('canvas')) {
          c.addEventListener('webglcontextlost', () => { contextLoss++; });
        }
      },
    };
  },
  async ghostty() {
    const g = await import('/node_modules/ghostty-web/dist/ghostty-web.js');
    await g.init();
    return { create: (o) => new g.Terminal(o) };
  },
};

const ws = new WebSocket(`ws://${location.host}/ws`);
ws.binaryType = 'arraybuffer';
const grid = document.getElementById('grid');
let terms = [];
let pending = [];
let frameQueued = false;
let contextLoss = 0;

const send = (m) => ws.send(JSON.stringify(m));

function flushAcks() {
  frameQueued = false;
  const painted = performance.now();
  const a = pending.map((p) => [p.pane, p.seq, p.parsed - p.start, painted - p.parsed]);
  pending = [];
  send({ t: 'ack', a, lost: contextLoss });
}

function onParsed(pane, seq, start) {
  pending.push({ pane, seq, start, parsed: performance.now() });
  if (!frameQueued) {
    frameQueued = true;
    requestAnimationFrame(flushAcks);
  }
}

async function init(m) {
  dispose();
  contextLoss = 0;
  const eng = await engines[m.engine]();
  const cols = m.cols, rows = m.rows;
  // Tiles are laid out at natural size, then the whole grid is scaled to fit
  // the viewport. Scaling keeps every terminal intersecting the viewport,
  // which xterm.js needs to keep rendering, without shrinking its canvases.
  const side = Math.ceil(Math.sqrt(m.panes));
  grid.style.gridTemplateColumns = `repeat(${side}, max-content)`;
  for (let i = 0; i < m.panes; i++) {
    const el = document.createElement('div');
    el.className = 'tile';
    grid.appendChild(el);
    const t = eng.create({ cols, rows, fontSize: 14, fontFamily: 'monospace', scrollback: 1000 });
    t.open(el);
    eng.afterOpen?.(t);
    terms.push(t);
  }
  const g = grid.getBoundingClientRect();
  const s = Math.min(1, innerWidth / g.width, innerHeight / g.height);
  grid.style.transform = `scale(${s})`;
  // One frame so the first measured write doesn't pay for initial layout.
  await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
  send({ t: 'ready', scale: s, dpr: devicePixelRatio, ua: navigator.userAgent });
}

function dispose() {
  for (const t of terms) t.dispose();
  terms = [];
  grid.replaceChildren();
  pending = [];
}

ws.onopen = () => send({ t: 'hello' });
ws.onmessage = (ev) => {
  if (typeof ev.data === 'string') {
    const m = JSON.parse(ev.data);
    if (m.t === 'init') init(m);
    else if (m.t === 'dispose') { dispose(); send({ t: 'disposed' }); }
    return;
  }
  const dv = new DataView(ev.data);
  const pane = dv.getUint16(0, true);
  const seq = dv.getUint32(4, true);
  const start = performance.now();
  terms[pane].write(new Uint8Array(ev.data, 8), () => onParsed(pane, seq, start));
};
