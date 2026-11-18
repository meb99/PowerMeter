/*
  PowerMeter overlay renderer, shared by the OBS overlay (/overlay, /o/<id>),
  the Overlay-Studio preview (/studio) and the start page (/).

    const ov = PowerOverlay.mount(element, design);   // design = {cfg} or a cfg
    ov.update(snapshot);                               // from /events
    ov.setDesign(otherDesign);

  Everything lives in a shadow root, so several overlays (and the studio's own
  styles) never mix. Text from designs and devices only ever goes in through
  textContent. The server puts the font list in front of this file
  (window.__POWERMETER_FONTS__, from src/designs.rs).
*/
(function () {
  'use strict';

  const FONTS = window.__POWERMETER_FONTS__ || [];
  const FONT = {};
  FONTS.forEach(f => { FONT[f.id] = f; });
  const MONO = '"SF Mono", "Cascadia Mono", Consolas, Menlo, "DejaVu Sans Mono", monospace';
  const HEAD = new Set(['logo', 'title', 'mode', 'model', 'set', 'alarms']);
  const VALUES = new Set(['v', 'i', 'p']);
  const UNIT = { v: 'V', i: 'A', p: 'W' };
  const LABEL = { v: 'Spannung', i: 'Strom', p: 'Leistung' };
  const INT_DIGITS = { v: 2, i: 1, p: 2 };
  const SERIES = { v: 1, i: 2, p: 3 };
  const LIVE = new Set();

  // ------------------------------------------------------------ helpers

  function el(tag, cls, text) {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text != null) e.textContent = text;
    return e;
  }

  function rgba(hex, a) {
    const m = /^#?([0-9a-f]{6})$/i.exec(hex || '');
    const n = m ? parseInt(m[1], 16) : 0xffffff;
    return `rgba(${n >> 16},${(n >> 8) & 255},${n & 255},${a})`;
  }

  function fontCss(id) {
    const f = FONT[id];
    return f ? f.css : MONO;
  }

  function stretch(id) {
    const f = FONT[id];
    return f && f.stretch ? f.stretch : 'normal';
  }

  function dashes(dec, int) {
    return '-'.repeat(int) + (dec > 0 ? '.' + '-'.repeat(dec) : '');
  }

  function setText(node, text) {
    if (node.textContent !== text) node.textContent = text;
  }

  function show(node, on) {
    node.classList.toggle('hide', !on);
  }

  /** A reading, with the decimal point in its own span (narrowed for monospace fonts). */
  function setNum(node, text) {
    if (node.pmText === text) return;
    node.pmText = text;
    while (node.firstChild) node.removeChild(node.firstChild);
    const k = text.indexOf('.');
    if (k < 0) { node.textContent = text; return; }
    node.append(document.createTextNode(text.slice(0, k)), el('span', 'dp', '.'), document.createTextNode(text.slice(k + 1)));
  }
  const MONO_FONTS = new Set(['b612', 'mono', 'mono-classic', 'typewriter']);

  // The bundled B612 Mono (SIL OFL, served by the app) works offline in OBS.
  let fontsRequested = false;
  function loadBundledFonts() {
    if (fontsRequested || !window.FontFace || !document.fonts) return;
    fontsRequested = true;
    [['700', 'B612Mono-Bold.ttf'], ['400', 'B612Mono-Regular.ttf']].forEach(([weight, file]) => {
      try {
        const face = new FontFace('B612 Mono', `url(/fonts/${file})`, { weight });
        document.fonts.add(face);
        face.load().then(() => LIVE.forEach(o => o.remeasure())).catch(() => {});
      } catch (e) { /* old browser: the fallback fonts take over */ }
    });
  }

  // --------------------------------------------------------- 7 segments

  // Cell 64 × 100, slanted like a real display; unlit segments stay faintly
  // visible as on the real thing.
  const SEG_ON = {
    '0': 'abcdef', '1': 'bc', '2': 'abdeg', '3': 'abcdg', '4': 'bcfg', '5': 'acdfg', '6': 'acdefg', '7': 'abc',
    '8': 'abcdefg', '9': 'abcdfg', '-': 'g', ' ': '', 'O': 'abcdef', 'L': 'def', 'E': 'adefg', 'r': 'eg',
  };
  const SEG_SHAPE = (() => {
    const t = 9;
    const h = (cx, cy, L) => [[cx - L / 2, cy], [cx - L / 2 + t / 2, cy - t / 2], [cx + L / 2 - t / 2, cy - t / 2],
      [cx + L / 2, cy], [cx + L / 2 - t / 2, cy + t / 2], [cx - L / 2 + t / 2, cy + t / 2]];
    const v = (cx, cy, L) => [[cx, cy - L / 2], [cx + t / 2, cy - L / 2 + t / 2], [cx + t / 2, cy + L / 2 - t / 2],
      [cx, cy + L / 2], [cx - t / 2, cy + L / 2 - t / 2], [cx - t / 2, cy - L / 2 + t / 2]];
    const pts = a => a.map(p => p.map(x => x.toFixed(1)).join(',')).join(' ');
    return {
      a: pts(h(28, 6, 37)), g: pts(h(28, 50, 37)), d: pts(h(28, 94, 37)),
      f: pts(v(8, 28, 41)), b: pts(v(48, 28, 41)), e: pts(v(8, 72, 41)), c: pts(v(48, 72, 41)),
    };
  })();
  const SVG = 'http://www.w3.org/2000/svg';

  function segCell() {
    const svg = document.createElementNS(SVG, 'svg');
    svg.setAttribute('viewBox', '0 0 64 100');
    svg.setAttribute('class', 'cell');
    const g = document.createElementNS(SVG, 'g');
    g.setAttribute('transform', 'translate(11 0) skewX(-7)');
    const segs = {};
    for (const k of 'abcdefg') {
      const p = document.createElementNS(SVG, 'polygon');
      p.setAttribute('points', SEG_SHAPE[k]);
      p.setAttribute('class', 's');
      g.appendChild(p);
      segs[k] = p;
    }
    svg.appendChild(g);
    const dp = document.createElementNS(SVG, 'circle');
    dp.setAttribute('cx', '56');
    dp.setAttribute('cy', '93');
    dp.setAttribute('r', '5.5');
    dp.setAttribute('class', 's');
    svg.appendChild(dp);
    return { svg, segs, dp, state: '' };
  }

  /** Shows `text` on a row of 7-segment cells; the row only ever grows. */
  function segRender(box, text) {
    const cells = [];
    for (const ch of text) {
      if (ch === '.' || ch === ',') {
        if (cells.length) cells[cells.length - 1].dp = true;
        else cells.push({ ch: ' ', dp: true });
      } else cells.push({ ch, dp: false });
    }
    box.seg = box.seg || [];
    while (box.seg.length < cells.length) {
      const c = segCell();
      box.insertBefore(c.svg, box.firstChild);
      box.seg.unshift(c);
    }
    const pad = box.seg.length - cells.length;
    box.seg.forEach((c, k) => {
      const want = k < pad ? { ch: ' ', dp: false } : cells[k - pad];
      const state = want.ch + (want.dp ? '.' : '');
      if (c.state === state) return;
      c.state = state;
      const on = SEG_ON[want.ch] != null ? SEG_ON[want.ch] : '';
      for (const s of 'abcdefg') c.segs[s].classList.toggle('on', on.indexOf(s) >= 0);
      c.dp.classList.toggle('on', want.dp);
    });
  }

  // ---------------------------------------------------------------- css

  const CSS = `
:host { all: initial; display: block; position: relative; }
.ov { position: absolute; left: 0; top: 0; width: max-content; padding: 8px; transform-origin: 0 0;
  font-family: var(--ft); color: var(--fg); line-height: 1.15; -webkit-font-smoothing: antialiased;
  transition: opacity .5s; box-sizing: border-box; }
.ov *, .ov *::before, .ov *::after { box-sizing: border-box; }
.ov.gone { opacity: 0; }
.panel { position: relative; background: var(--bg); border: var(--bw) solid var(--bc); border-radius: var(--r);
  transition: opacity .3s; }
.ov.shadow .panel { box-shadow: 0 8px 30px rgba(0,0,0,.35); }
.ov.glow.bordered .panel { box-shadow: 0 0 18px var(--gb), inset 0 0 14px var(--gb2); }
.ov.shadow.glow.bordered .panel { box-shadow: 0 0 18px var(--gb), inset 0 0 14px var(--gb2), 0 8px 30px rgba(0,0,0,.35); }
.ov.stale .panel { opacity: .5; }
.hide { display: none !important; }

.run + .run { margin-top: 8px; }
.head { display: flex; align-items: center; flex-wrap: wrap; gap: 6px 10px; font-size: 13px; color: var(--muted);
  letter-spacing: .06em; text-transform: uppercase; font-weight: 600; }
.head.empty { display: none; }
/* status rows and the event line fill the panel but never widen it, so a
   warning that pops up doesn't make the panel jump */
.ov:not(.L-bar) .run.head, .ov:not(.L-bar) .run.events { width: 0; min-width: 100%; }
.title { color: var(--fg); opacity: .85; }
.logo { display: block; height: var(--logo-h); width: auto; }
.model { text-transform: none; letter-spacing: .01em; font-weight: 500; }
.set { font-family: var(--fnt); text-transform: none; letter-spacing: 0; font-weight: 500; margin-left: auto;
  font-variant-numeric: tabular-nums; white-space: nowrap; }
.pill { font-family: var(--fnt); font-weight: 800; font-size: 13px; padding: 2px 9px; border-radius: 999px;
  letter-spacing: .05em; color: #fff; background: #5c5f66; white-space: nowrap; }
.pill.CV { background: var(--cv); color: #04130a; }
.pill.CC { background: var(--cc); color: #160b00; }
.pill.prot { background: var(--alarm); color: #fff; animation: blink .6s steps(2) infinite; }
.leds { display: none; gap: 10px; }
.led { display: inline-flex; align-items: center; gap: 5px; font-size: 11px; letter-spacing: .12em; color: var(--muted); }
.led::before { content: ""; width: 9px; height: 9px; border-radius: 50%; background: #3a3c40;
  box-shadow: inset 0 1px 2px rgba(0,0,0,.7); }
.led.on.cv::before { background: var(--cv); box-shadow: 0 0 8px var(--cv), 0 0 2px var(--cv); }
.led.on.cc::before { background: var(--cc); box-shadow: 0 0 8px var(--cc), 0 0 2px var(--cc); }
.led.on.out::before { background: var(--cv); box-shadow: 0 0 8px var(--cv); }
.flag { font-family: var(--fnt); font-weight: 800; font-size: 13px; padding: 2px 8px; border-radius: 6px;
  background: var(--alarm); color: #fff; letter-spacing: .04em; animation: blink .6s steps(2) infinite; white-space: nowrap; }
.flag.off { background: #5c5f66; animation: none; }
.banner { flex-basis: 100%; order: 99; padding: 4px 10px; border-radius: 8px; background: var(--alarm); color: #fff;
  font-family: var(--fnt); font-weight: 800; font-size: 14px; letter-spacing: .03em; text-transform: none; }
.ov.flash .banner { animation: blink .6s steps(2) infinite; }
@keyframes blink { 50% { opacity: .35; } }
@keyframes ring { 0%, 100% { box-shadow: 0 0 0 2px var(--alarm), 0 0 18px var(--ga); } 50% { box-shadow: 0 0 0 4px var(--ga), 0 0 0 var(--ga); } }
.ov.alarm.flash:not(.L-minimal) .panel { animation: ring .8s ease-in-out infinite; border-color: var(--alarm); }

.values { display: grid; grid-template-columns: auto auto; column-gap: 10px; align-items: baseline; }
.ov.labels .values { grid-template-columns: auto auto auto; }
.vrow, .disp { display: contents; }
.lbl { font-size: 12px; color: var(--muted); letter-spacing: .1em; text-transform: uppercase; font-weight: 600;
  white-space: nowrap; }
.num { font-family: var(--fn); font-stretch: var(--fs); font-weight: var(--w); font-variant-numeric: tabular-nums;
  text-align: right; white-space: pre; line-height: 1.05; display: inline-block; }
.unit { font-weight: 600; color: var(--muted); white-space: nowrap; }
/* monospace fonts give "." a whole digit cell; B612 Mono puts it at the left */
.ov.tight .num .dp { margin: 0 -.14em; }
.ov.tight.b612 .num .dp { margin: 0 -.3em 0 0; }
.ov.au .k-i .unit, .ov.au .k-p .unit { min-width: 1.7em; }
.ghost { position: absolute; visibility: hidden; left: -9999px; top: 0; }
.k-v .num { color: var(--v); } .k-i .num { color: var(--i); } .k-p .num { color: var(--p); } .dmm .num { color: var(--dmm); }
.ov.glow .k-v .num { text-shadow: 0 0 8px var(--gv), 0 0 22px var(--gv2); }
.ov.glow .k-i .num { text-shadow: 0 0 8px var(--gi), 0 0 22px var(--gi2); }
.ov.glow .k-p .num { text-shadow: 0 0 8px var(--gp), 0 0 22px var(--gp2); }
.ov.glow .dmm .num { text-shadow: 0 0 8px var(--gd), 0 0 22px var(--gd2); }
.ov.out-off .values .num, .ov.out-off .values .unit { opacity: .38; }
.big .num { font-size: 56px; } .big .unit { font-size: 26px; }
.mid .num { font-size: 40px; } .mid .unit { font-size: 20px; }
.eq .num { font-size: 46px; } .eq .unit { font-size: 22px; }

/* 7-segment digits */
.num.seg { display: inline-flex; align-items: flex-end; text-shadow: none !important; }
.cell { height: .9em; width: .58em; overflow: visible; }
.cell .s { fill: currentColor; opacity: .09; }
.cell .s.on { opacity: 1; }
.ov.glow .num.seg { filter: drop-shadow(0 0 3px var(--glow-c, currentColor)); }
.k-v .num.seg { --glow-c: var(--gv); } .k-i .num.seg { --glow-c: var(--gi); } .k-p .num.seg { --glow-c: var(--gp); }
.dmm .num.seg { --glow-c: var(--gd); }

/* the canvas sits absolutely in its box: its pixel size must never feed
   back into the panel's width (HiDPI screens, zoomed previews) */
.plot { position: relative; }
.plot canvas { position: absolute; left: 0; top: 0; width: 100%; height: 100%; display: block; }
.energy { display: flex; gap: 14px; font-family: var(--fnt); font-size: 13px; color: var(--muted); white-space: nowrap; }
.energy b { color: var(--fg); font-weight: 600; }
.events { font-family: var(--fnt); font-size: 12px; color: var(--alarm); white-space: nowrap; overflow: hidden;
  text-overflow: ellipsis; max-width: 460px; }
.dmm { display: flex; align-items: baseline; gap: 10px; }
.dmm .lbl { min-width: 84px; }
.dmm .num { font-size: 38px; } .dmm .unit { font-size: 18px; }
.dmm .tag { font-size: 13px; color: var(--muted); font-weight: 600; }
.ov.L-card .dmm, .ov.L-minimal .dmm { padding-top: 8px; border-top: 1px solid rgba(255,255,255,.1); }

/* ------------------------------------------------ Kachel */
.L-card .panel { padding: 14px 18px 12px; min-width: 320px; }

/* ------------------------------------------------ Messgerät */
.L-meter .panel { padding: 14px 6px 12px; min-width: 300px; }
.L-meter .values { display: flex; align-items: flex-start; }
.L-meter .vrow { display: flex; flex-direction: column; align-items: center; padding: 2px 18px 0; flex: 1 1 0; }
.L-meter .vrow + .vrow { border-left: 1px solid rgba(255,255,255,.08); }
.L-meter .disp { display: flex; flex-direction: column; align-items: center; }
.L-meter .num { text-align: center; }
.L-meter .unit { margin-top: 3px; letter-spacing: .06em; font-weight: 700; }
.L-meter .k-v .unit { color: var(--v); } .L-meter .k-i .unit { color: var(--i); } .L-meter .k-p .unit { color: var(--p); }
.L-meter .unit { opacity: .72; }
.L-meter .lbl { margin-bottom: 4px; font-size: 11px; }
.L-meter .eq .num { font-size: 50px; } .L-meter .eq .unit { font-size: 14px; }
.L-meter .big .num { font-size: 60px; } .L-meter .big .unit { font-size: 15px; }
.L-meter .mid .num { font-size: 42px; } .L-meter .mid .unit { font-size: 13px; }
.L-meter .run { padding: 0 12px; }
.L-meter .values { padding: 0; }
.L-meter .values + .head, .L-meter .graph + .head, .L-meter .dmm + .head { margin-top: 10px; padding-top: 9px;
  border-top: 1px solid rgba(255,255,255,.07); }
.L-meter .head { font-size: 14px; }
.L-meter .graph { padding: 0 14px; }
.L-meter .dmm { justify-content: center; }

/* ------------------------------------------------ Leiste */
.L-bar .panel { display: flex; align-items: center; gap: 20px; padding: 10px 18px; }
.L-bar .run + .run { margin-top: 0; }
.L-bar .head { flex-direction: column; align-items: flex-start; flex-wrap: nowrap; gap: 5px; }
.L-bar .set { margin-left: 0; }
.L-bar .banner { flex-basis: auto; order: 0; }
.L-bar .values { display: flex; gap: 20px; align-items: baseline; }
.L-bar .vrow { display: flex; align-items: baseline; gap: 6px; flex-wrap: wrap; }
.ov.L-bar.labels .vrow { display: grid; grid-template-columns: auto auto; column-gap: 6px; }
.L-bar .vrow .lbl { grid-column: 1 / -1; font-size: 11px; }
.L-bar .eq .num { font-size: 40px; } .L-bar .eq .unit { font-size: 20px; }
.L-bar .big .num { font-size: 46px; } .L-bar .big .unit { font-size: 22px; }
.L-bar .mid .num { font-size: 34px; } .L-bar .mid .unit { font-size: 17px; }
.L-bar .dmm { padding-left: 20px; border-left: 1px solid rgba(255,255,255,.12); }
.L-bar .dmm .lbl { min-width: 0; }
.L-bar .dmm .num { font-size: 34px; }
.L-bar .energy { flex-direction: column; gap: 2px; }
.L-bar .events { max-width: 260px; }

/* ------------------------------------------------ Seitenleiste */
.L-column .panel { display: flex; flex-direction: column; padding: 14px 16px; min-width: 170px; }
.L-column .run + .run { margin-top: 12px; }
.L-column .title { flex-basis: 100%; }
.L-column .set { margin-left: 0; flex-basis: 100%; }
.L-column .values { display: flex; flex-direction: column; gap: 10px; }
.L-column .vrow { display: grid; grid-template-columns: 1fr auto; column-gap: 6px; align-items: baseline;
  padding-left: 10px; border-left: 3px solid var(--kc); }
.L-column .vrow .lbl { grid-column: 1 / -1; font-size: 11px; margin-bottom: 1px; }
.L-column .k-v { --kc: var(--v); } .L-column .k-i { --kc: var(--i); } .L-column .k-p { --kc: var(--p); }
.L-column .eq .num { font-size: 36px; } .L-column .eq .unit { font-size: 16px; }
.L-column .big .num { font-size: 44px; } .L-column .big .unit { font-size: 19px; }
.L-column .mid .num { font-size: 30px; } .L-column .mid .unit { font-size: 14px; }
.L-column .dmm { flex-direction: column; align-items: flex-start; gap: 2px; }
.L-column .dmm .num { font-size: 30px; }
.L-column .energy { flex-direction: column; gap: 2px; }

/* ------------------------------------------------ Ohne Hintergrund */
.L-minimal .panel { background: none !important; border-color: transparent !important; box-shadow: none !important;
  padding: 2px 4px; min-width: 200px; }
.L-minimal .num, .L-minimal .unit, .L-minimal .lbl, .L-minimal .title, .L-minimal .set, .L-minimal .model,
.L-minimal .energy, .L-minimal .tag { text-shadow: -1.5px -1.5px 0 rgba(0,0,0,.85), 1.5px -1.5px 0 rgba(0,0,0,.85),
  -1.5px 1.5px 0 rgba(0,0,0,.85), 1.5px 1.5px 0 rgba(0,0,0,.85), 0 0 4px #000, 0 3px 12px rgba(0,0,0,.6) !important; }
.L-minimal .num.seg { filter: drop-shadow(0 0 2px rgba(0,0,0,.9)) drop-shadow(0 2px 6px rgba(0,0,0,.6)); }
.L-minimal .unit, .L-minimal .lbl { color: var(--fg); }
.L-minimal .eq .num { font-size: 62px; } .L-minimal .eq .unit { font-size: 28px; }
.L-minimal .big .num { font-size: 76px; } .L-minimal .big .unit { font-size: 32px; }
.L-minimal .mid .num { font-size: 50px; } .L-minimal .mid .unit { font-size: 23px; }
.L-minimal .pill, .L-minimal .flag { box-shadow: 0 2px 8px rgba(0,0,0,.5); }

/* ------------------------------------------------ Labornetzteil */
.L-bench .panel { padding: 16px 20px 14px; min-width: 300px; background-image: linear-gradient(180deg, rgba(255,255,255,.075), rgba(0,0,0,.2));
  box-shadow: inset 0 1px 0 rgba(255,255,255,.14), inset 0 -2px 0 rgba(0,0,0,.35), 0 10px 30px rgba(0,0,0,.45) !important; }
.L-bench .panel::before { content: ""; position: absolute; inset: 7px; pointer-events: none;
  background: radial-gradient(circle at 4px 4px, #8d9096 0 1.6px, #4b4e53 2.2px 3.4px, transparent 3.8px) 0 0 / 100% 100% no-repeat,
    radial-gradient(circle at calc(100% - 4px) 4px, #8d9096 0 1.6px, #4b4e53 2.2px 3.4px, transparent 3.8px) 0 0 / 100% 100% no-repeat,
    radial-gradient(circle at 4px calc(100% - 4px), #8d9096 0 1.6px, #4b4e53 2.2px 3.4px, transparent 3.8px) 0 0 / 100% 100% no-repeat,
    radial-gradient(circle at calc(100% - 4px) calc(100% - 4px), #8d9096 0 1.6px, #4b4e53 2.2px 3.4px, transparent 3.8px) 0 0 / 100% 100% no-repeat; }
.L-bench .run { position: relative; }
.L-bench .head { font-size: 12px; letter-spacing: .2em; padding: 0 8px; }
.L-bench .title { color: var(--fg); opacity: .8; text-shadow: 0 -1px 0 rgba(0,0,0,.6); }
.L-bench .pill { display: none; }
.L-bench .leds { display: inline-flex; margin-left: auto; }
.L-bench .set { color: var(--muted); }
.L-bench .values { display: flex; gap: 14px; }
.L-bench .vrow { display: flex; flex-direction: column; gap: 5px; }
.L-bench .lbl { font-size: 11px; letter-spacing: .2em; color: var(--fg); opacity: .62; padding-left: 4px; }
.L-bench .disp, .L-bench .dmm .disp { display: flex; align-items: flex-end; gap: 8px; background: #060708;
  border-radius: 6px; padding: 12px 14px 10px;
  box-shadow: inset 0 2px 8px rgba(0,0,0,.95), inset 0 0 0 1px rgba(0,0,0,.9), 0 1px 0 rgba(255,255,255,.09); }
.L-bench .unit { font-size: 18px !important; font-weight: 700; margin-bottom: 2px; }
.L-bench .k-v .unit { color: var(--v); } .L-bench .k-i .unit { color: var(--i); } .L-bench .k-p .unit { color: var(--p); }
.L-bench .dmm .unit { color: var(--dmm); }
.L-bench .eq .num { font-size: 56px; } .L-bench .big .num { font-size: 62px; } .L-bench .mid .num { font-size: 44px; }
.L-bench .graph { background: #060708; border-radius: 6px; padding: 6px;
  box-shadow: inset 0 2px 8px rgba(0,0,0,.95), 0 1px 0 rgba(255,255,255,.09); }
.L-bench .dmm { flex-direction: column; align-items: flex-start; gap: 5px; }
.L-bench .dmm .lbl { min-width: 0; }
.L-bench .energy { padding: 0 8px; }
`;

  // ----------------------------------------------------------- renderer

  function unpack(design) {
    if (!design) return {};
    return design.cfg ? design.cfg : design;
  }

  function mount(host, design, opts) {
    opts = opts || {};
    loadBundledFonts();
    const root = host.shadowRoot || host.attachShadow({ mode: 'open' });
    while (root.firstChild) root.removeChild(root.firstChild);
    const base = el('style', null, CSS);
    const custom = el('style');
    const ov = el('div', 'ov');
    const panel = el('div', 'panel');
    ov.appendChild(panel);
    root.append(base, ov, custom);

    let cfg = null;
    let last = null;
    let spark = [];
    let linkDown = false;
    let parts = {};
    let reserve = {};
    let raf = 0;
    let fit = 1;

    function on(key) {
      return cfg.elements.some(e => e.key === key && e.on);
    }

    function build() {
      while (panel.firstChild) panel.removeChild(panel.firstChild);
      parts = { heads: [], vrows: {} };
      reserve = {};
      const runs = [];
      for (const e of cfg.elements) {
        if (!e.on) continue;
        const kind = HEAD.has(e.key) ? 'head' : VALUES.has(e.key) ? 'values' : e.key;
        const prev = runs[runs.length - 1];
        if (prev && prev.kind === kind && (kind === 'head' || kind === 'values')) prev.keys.push(e.key);
        else runs.push({ kind, keys: [e.key] });
      }
      const numsSeg = cfg.font_numbers === 'seg7';
      for (const run of runs) {
        const box = el('div', 'run ' + run.kind);
        if (run.kind === 'head') {
          const items = [];
          for (const k of run.keys) items.push(...headItem(k));
          items.forEach(n => box.appendChild(n));
          parts.heads.push({ box, items });
        } else if (run.kind === 'values') {
          for (const k of run.keys) {
            const size = cfg.accent === 'none' ? 'eq' : cfg.accent === k ? 'big' : 'mid';
            const row = el('div', `vrow k-${k} ${size}`);
            const lbl = el('span', 'lbl', LABEL[k]);
            show(lbl, cfg.labels);
            const disp = el('span', 'disp');
            const num = el('span', 'num' + (numsSeg ? ' seg' : ''));
            const ghost = el('span', 'num ghost');
            const unit = el('span', 'unit', UNIT[k]);
            show(unit, cfg.units);
            disp.append(num, unit);
            row.append(lbl, disp, ghost);
            box.appendChild(row);
            parts.vrows[k] = { row, num, ghost, unit, text: null };
          }
        } else if (run.kind === 'graph') {
          const plot = el('div', 'plot');
          const c = el('canvas');
          plot.appendChild(c);
          box.appendChild(plot);
          parts.canvas = c;
          parts.plot = plot;
        } else if (run.kind === 'energy') {
          const pair = (label, sub) => {
            const s = el('span');
            s.append(document.createTextNode(label));
            if (sub) s.appendChild(el('sub', null, sub));
            s.append(document.createTextNode(' '));
            const b = el('b', null, '–');
            s.appendChild(b);
            box.appendChild(s);
            return b;
          };
          parts.wh = pair('E', null);
          parts.mah = pair('Q', null);
          parts.peak = pair('I', 'max');
        } else if (run.kind === 'events') {
          parts.events = box;
        } else if (run.kind === 'dmm') {
          const lbl = el('span', 'lbl', 'Multimeter');
          const disp = el('span', 'disp');
          const num = el('span', 'num' + (numsSeg ? ' seg' : ''));
          const unit = el('span', 'unit');
          const tag = el('span', 'tag');
          disp.append(num, unit);
          box.append(lbl, disp, tag);
          parts.dmm = { box, lbl, num, unit, tag, text: null };
        }
        panel.appendChild(box);
      }
    }

    function headItem(k) {
      if (k === 'title') {
        const t = el('span', 'title', cfg.title);
        show(t, !!cfg.title);
        return [t];
      }
      if (k === 'logo') {
        const img = el('img', 'logo');
        img.alt = '';
        if (cfg.logo) img.src = '/assets/' + encodeURIComponent(cfg.logo);
        show(img, !!cfg.logo);
        img.addEventListener('load', () => syncSize());
        return [img];
      }
      if (k === 'mode') {
        const pill = el('span', 'pill', '--');
        const leds = el('span', 'leds');
        const cv = el('span', 'led cv', 'CV');
        const cc = el('span', 'led cc', 'CC');
        const out = el('span', 'led out', 'OUT');
        leds.append(cv, cc, out);
        parts.mode = { pill, cv, cc, out };
        return [pill, leds];
      }
      if (k === 'model') {
        const m = el('span', 'model');
        parts.model = m;
        return [m];
      }
      if (k === 'set') {
        const s = el('span', 'set');
        parts.set = s;
        return [s];
      }
      // alarms
      const short = el('span', 'flag', 'KURZSCHLUSS');
      const dip = el('span', 'flag', 'EINBRUCH');
      const off = el('span', 'flag off', 'AUSGANG AUS');
      const banner = el('div', 'banner');
      parts.alarms = { short, dip, off, banner };
      [short, dip, off, banner].forEach(n => show(n, false));
      return [short, dip, off, banner];
    }

    function applyVars() {
      const c = cfg.colors;
      const st = ov.style;
      const vars = {
        '--v': c.v, '--i': c.i, '--p': c.p, '--dmm': c.dmm, '--cv': c.cv, '--cc': c.cc, '--alarm': c.alarm,
        '--fg': c.text, '--muted': c.muted, '--bg': rgba(c.bg, cfg.bg_opacity),
        '--bc': rgba(c.border, cfg.border_opacity), '--bw': cfg.border_width + 'px', '--r': cfg.radius + 'px',
        '--ft': fontCss(cfg.font_text), '--fn': fontCss(cfg.font_numbers), '--fs': stretch(cfg.font_numbers),
        '--fnt': cfg.font_numbers === 'seg7' ? MONO : fontCss(cfg.font_numbers), '--w': cfg.weight,
        '--logo-h': cfg.logo_height + 'px',
        '--gv': rgba(c.v, .7), '--gv2': rgba(c.v, .3), '--gi': rgba(c.i, .7), '--gi2': rgba(c.i, .3),
        '--gp': rgba(c.p, .7), '--gp2': rgba(c.p, .3), '--gd': rgba(c.dmm, .7), '--gd2': rgba(c.dmm, .3),
        '--gb': rgba(c.border, .55), '--gb2': rgba(c.border, .22), '--ga': rgba(c.alarm, .45),
      };
      for (const k in vars) st.setProperty(k, String(vars[k]));
      st.transform = cfg.scale === 1 ? '' : `scale(${cfg.scale})`;
      ov.className = 'ov L-' + cfg.layout;
      ov.classList.toggle('labels', !!cfg.labels);
      ov.classList.toggle('shadow', !!cfg.shadow);
      ov.classList.toggle('glow', !!cfg.glow);
      ov.classList.toggle('bordered', cfg.border_width > 0 && cfg.border_opacity > 0);
      ov.classList.toggle('flash', !!cfg.alarm_flash);
      ov.classList.toggle('au', !!cfg.auto_units);
      ov.classList.toggle('tight', MONO_FONTS.has(cfg.font_numbers));
      ov.classList.toggle('b612', cfg.font_numbers === 'b612');
      custom.textContent = cfg.custom_css || '';
    }

    function setDesign(design) {
      cfg = unpack(design);
      applyVars();
      build();
      if (parts.plot) {
        const g = cfg.graph;
        parts.plot.style.height = g.height + 'px';
        if (cfg.layout === 'bar') parts.plot.style.width = g.width + 'px';
        else parts.plot.style.minWidth = g.width + 'px';
      }
      render();
    }

    // ------------------------------------------------------- formatting

    function decimals(key, d) {
      const want = cfg.decimals ? cfg.decimals[key] : 'auto';
      if (typeof want === 'number') return want;
      const auto = d && d[key + '_decimals'];
      return typeof auto === 'number' ? auto : { v: 3, i: 4, p: 2 }[key];
    }

    function reading(key, d) {
      let dec = decimals(key, d);
      let unit = UNIT[key];
      const x = d ? d[key] : null;
      if (x == null || !isFinite(x) || linkDown) return { text: dashes(dec, INT_DIGITS[key]), unit };
      let y = x;
      if (cfg.auto_units && key !== 'v' && Math.abs(+x.toFixed(dec)) < 1) {
        y = x * 1000;
        dec = Math.max(0, dec - 3);
        unit = 'm' + unit;
      }
      if (Math.abs(y) < 0.5 * Math.pow(10, -dec)) y = 0;
      return { text: y.toFixed(dec), unit };
    }

    // The widest reading so far (digits as zeros) sets the minimum width,
    // so the panel never jumps when numbers change.
    function reserveWidth(slot, key, text, isSeg) {
      const pattern = text.replace(/[0-9-]/g, '0');
      if (isSeg) return;
      const cur = reserve[key];
      if (cur && cur.length >= pattern.length) return;
      reserve[key] = pattern;
      setNum(slot.ghost, pattern);
      const w = slot.ghost.offsetWidth;
      if (w > 0) slot.num.style.minWidth = w + 'px';
    }

    function initialPattern(key) {
      const dec = decimals(key, last);
      let p = '0'.repeat(INT_DIGITS[key]) + (dec > 0 ? '.' + '0'.repeat(dec) : '');
      if (cfg.auto_units && key !== 'v') {
        const md = Math.max(0, dec - 3);
        const mp = '000' + (md > 0 ? '.' + '0'.repeat(md) : '');
        if (mp.length > p.length) p = mp;
      }
      return p;
    }

    function remeasure() {
      reserve = {};
      for (const k in parts.vrows) {
        const slot = parts.vrows[k];
        slot.num.style.minWidth = '';
        if (cfg.font_numbers !== 'seg7') reserveWidth(slot, k, initialPattern(k), false);
        if (slot.text) reserveWidth(slot, k, slot.text, false);
      }
      syncSize();
    }

    // ---------------------------------------------------------- render

    function render() {
      const d = last || {};
      const noDevice = !last || (!d.power_connected && !d.dmm_connected);
      const stale = linkDown || noDevice;
      ov.classList.toggle('stale', stale && !opts.never_stale);
      ov.classList.toggle('gone', !!cfg.hide_when_disconnected && stale);
      const alarm = !!(d.short || d.dropout || d.protection);
      ov.classList.toggle('alarm', alarm && !stale);
      const outOff = d.output_on === false;
      ov.classList.toggle('out-off', outOff && !stale);
      const isSeg = cfg.font_numbers === 'seg7';

      for (const k in parts.vrows) {
        const slot = parts.vrows[k];
        const r = reading(k, last);
        if (slot.text !== r.text) {
          slot.text = r.text;
          if (isSeg) {
            const cells = Math.max(r.text.replace(/[.,]/g, '').length, initialPattern(k).replace('.', '').length);
            segRender(slot.num, r.text.padStart(cells + (r.text.match(/[.,]/g) || []).length, ' '));
          } else {
            setNum(slot.num, r.text);
            if (!reserve[k]) reserveWidth(slot, k, initialPattern(k), false);
            reserveWidth(slot, k, r.text, false);
          }
        }
        setText(slot.unit, r.unit);
      }

      if (parts.mode) {
        const m = parts.mode;
        let text = d.mode || '--';
        let cls = text;
        if (d.protection) { text = d.protection; cls = 'prot'; }
        else if (outOff || text === 'OFF') { text = 'AUS'; cls = 'off'; }
        if (stale) { text = '--'; cls = ''; }
        setText(m.pill, text);
        m.pill.className = 'pill ' + cls;
        m.cv.classList.toggle('on', !stale && cls === 'CV');
        m.cc.classList.toggle('on', !stale && cls === 'CC');
        m.out.classList.toggle('on', !stale && !outOff && !!d.power_connected);
      }
      if (parts.model) {
        setText(parts.model, d.psu_model || '');
        show(parts.model, !!d.psu_model);
      }
      if (parts.set) {
        const bits = [];
        if (d.v_set_text) bits.push((d.set_learned ? '≈ Soll ' : 'Soll ') + d.v_set_text + ' V');
        if (d.i_set_text) bits.push((d.i_set_learned ? '≈ Limit ' : 'Limit ') + d.i_set_text + ' A');
        setText(parts.set, bits.join(' · '));
        show(parts.set, bits.length > 0 && !stale);
      }
      if (parts.alarms) {
        const a = parts.alarms;
        show(a.short, !stale && !!d.short);
        show(a.dip, !stale && !!d.dropout && !d.short);
        show(a.off, !stale && outOff && !d.protection && !parts.mode);
        // the meter's mode badge already flashes "OVP" in red
        const banner = !stale && !!d.protection && !(cfg.layout === 'meter' && parts.mode);
        setText(a.banner, d.protection ? 'SCHUTZABSCHALTUNG · ' + d.protection : '');
        show(a.banner, banner);
      }
      for (const h of parts.heads) {
        h.box.classList.toggle('empty', h.items.every(n => n.classList.contains('hide') || n.tagName === 'SPAN' && n.className === 'leds' && cfg.layout !== 'bench'));
      }
      if (parts.wh) {
        setText(parts.wh, d.energy_text || '–');
        setText(parts.mah, d.charge_text || '–');
        setText(parts.peak, d.peak_i_text || '–');
      }
      if (parts.events) {
        const t = d.dropouts ? `${d.dropouts}× Einbruch · ${d.last_event || ''}` : (d.last_event || '');
        setText(parts.events, t);
        show(parts.events, !!t);
      }
      if (parts.dmm) {
        const p = parts.dmm;
        show(p.box, !!d.dmm_connected);
        setText(p.lbl, d.dmm_function ? 'Multimeter · ' + d.dmm_function : 'Multimeter');
        const txt = stale ? '-----' : (d.dmm_value_text || '-----');
        if (p.text !== txt) {
          p.text = txt;
          if (isSeg) segRender(p.num, txt.padStart(6, ' '));
          else setNum(p.num, txt);
        }
        setText(p.unit, d.dmm_unit || '');
        setText(p.tag, d.dmm_tag && d.dmm_tag !== (d.dmm_function || '').split(' ').pop() ? d.dmm_tag : '');
      }
      if (d.spark) spark = d.spark;
      drawSoon();
    }

    function drawSoon() {
      if (!parts.canvas || raf) return;
      raf = requestAnimationFrame(() => { raf = 0; draw(); });
    }

    // ----------------------------------------------------------- graph

    function niceMax(x) {
      if (!(x > 0)) return 1;
      const e = Math.pow(10, Math.floor(Math.log10(x)));
      for (const m of [1, 1.2, 1.5, 2, 2.5, 3, 4, 5, 6, 8, 10]) if (x <= m * e + 1e-12) return m * e;
      return 10 * e;
    }

    function axisLabel(x, unit) {
      if (x === 0) return '0';
      if (x < 1 && unit !== 'V') return (x * 1000).toFixed(0) + ' m' + unit;
      return (x >= 10 || x === Math.round(x) ? x.toFixed(x < 10 ? (x % 1 ? 1 : 0) : 0) : x.toFixed(1)) + ' ' + unit;
    }

    function draw() {
      const c = parts.canvas;
      if (!c || !c.isConnected) return;
      const g = cfg.graph;
      const w = c.clientWidth;
      const h = g.height;
      if (!w) return;
      const k = (window.devicePixelRatio || 1) * cfg.scale * fit;
      const W = Math.round(w * k), H = Math.round(h * k);
      if (c.width !== W || c.height !== H) { c.width = W; c.height = H; }
      const x2 = c.getContext('2d');
      x2.setTransform(k, 0, 0, k, 0, 0);
      x2.clearRect(0, 0, w, h);
      const d = last || {};
      const series = g.series.filter(s => SERIES[s]);
      const rows = spark.filter(r => r[0] >= -g.seconds);
      const bench = cfg.layout === 'bench';
      const minimal = cfg.layout === 'minimal';
      const font = `600 11px ${cfg.font_numbers === 'seg7' ? MONO : fontCss(cfg.font_numbers)}`;
      x2.font = font;
      const showV = g.axes && series.indexOf('v') >= 0;
      const showI = g.axes && series.indexOf('i') >= 0;
      const padL = showV ? 42 : 2, padR = showI ? 50 : 2;
      const padT = g.legend ? 18 : 4, padB = g.axes ? 8 : 4;
      const pw = Math.max(10, w - padL - padR), ph = Math.max(10, h - padT - padB);
      const scaleMax = {};
      for (const s of series) {
        let m = s === 'v' ? (d.v_set || 0) : s === 'i' ? (d.i_set || 0) : 0;
        for (const r of rows) if (r[SERIES[s]] > m) m = r[SERIES[s]];
        scaleMax[s] = g.axes ? niceMax(m * 1.05) : (m > 0 ? m * 1.12 : 1);
      }
      // grid
      if (!minimal) {
        x2.strokeStyle = bench ? 'rgba(61,255,122,.13)' : 'rgba(255,255,255,.08)';
        x2.lineWidth = 1;
        const lines = g.axes ? 4 : 4;
        for (let n = 0; n <= lines; n++) {
          if (!g.axes && (n === 0 || n === lines)) continue;
          const y = Math.round(padT + (ph * n) / lines) + 0.5;
          x2.beginPath(); x2.moveTo(padL, y); x2.lineTo(padL + pw, y); x2.stroke();
        }
        if (bench) {
          for (let n = 1; n < 6; n++) {
            const x = Math.round(padL + (pw * n) / 6) + 0.5;
            x2.beginPath(); x2.moveTo(x, padT); x2.lineTo(x, padT + ph); x2.stroke();
          }
        }
      }
      const yOf = (s, val) => padT + ph - (val / scaleMax[s]) * ph;
      const xOf = t => padL + ((t + g.seconds) / g.seconds) * pw;
      // axes labels
      if (g.axes) {
        x2.textBaseline = 'middle';
        const tick = (s, side) => {
          x2.fillStyle = rgba(cfg.colors[s], 0.85);
          x2.textAlign = side === 'l' ? 'right' : 'left';
          for (let n = 0; n <= 2; n++) {
            const val = (scaleMax[s] * n) / 2;
            const y = yOf(s, val);
            x2.fillText(axisLabel(val, UNIT[s]), side === 'l' ? padL - 6 : padL + pw + 6, y);
          }
        };
        if (showV) tick('v', 'l');
        if (showI) tick('i', 'r');
      }
      // set points
      if (g.setpoints) {
        x2.setLineDash([5, 4]);
        x2.lineWidth = 1.2;
        for (const [s, val] of [['v', d.v_set], ['i', d.i_set]]) {
          if (series.indexOf(s) < 0 || !(val > 0)) continue;
          const y = Math.round(yOf(s, val)) + 0.5;
          x2.strokeStyle = rgba(cfg.colors[s], 0.7);
          x2.beginPath(); x2.moveTo(padL, y); x2.lineTo(padL + pw, y); x2.stroke();
        }
        x2.setLineDash([]);
      }
      // lines
      x2.lineJoin = 'round';
      x2.lineCap = 'round';
      if (rows.length >= 2) {
        for (const s of series) {
          const col = SERIES[s];
          const color = cfg.colors[s];
          x2.beginPath();
          rows.forEach((r, n) => {
            const x = xOf(r[0]), y = yOf(s, Math.max(0, r[col]));
            if (n) x2.lineTo(x, y); else x2.moveTo(x, y);
          });
          if (g.fill) {
            const grad = x2.createLinearGradient(0, padT, 0, padT + ph);
            grad.addColorStop(0, rgba(color, 0.28));
            grad.addColorStop(1, rgba(color, 0));
            x2.save();
            x2.lineTo(xOf(rows[rows.length - 1][0]), padT + ph);
            x2.lineTo(xOf(rows[0][0]), padT + ph);
            x2.closePath();
            x2.fillStyle = grad;
            x2.fill();
            x2.restore();
            x2.beginPath();
            rows.forEach((r, n) => {
              const x = xOf(r[0]), y = yOf(s, Math.max(0, r[col]));
              if (n) x2.lineTo(x, y); else x2.moveTo(x, y);
            });
          }
          if (minimal) { x2.shadowColor = 'rgba(0,0,0,.85)'; x2.shadowBlur = 4; }
          if (cfg.glow && !minimal) { x2.shadowColor = rgba(color, 0.8); x2.shadowBlur = 6; }
          x2.strokeStyle = color;
          x2.lineWidth = g.line;
          x2.stroke();
          x2.shadowBlur = 0;
        }
      }
      // legend
      if (g.legend) {
        x2.font = `600 11px ${fontCss(cfg.font_text)}`;
        x2.textBaseline = 'middle';
        x2.textAlign = 'left';
        let x = padL + 2;
        for (const s of series) {
          x2.fillStyle = cfg.colors[s];
          x2.fillRect(x, 6, 12, 3);
          x2.fillStyle = cfg.colors.muted;
          x2.fillText(LABEL[s], x + 16, 8);
          x += 16 + x2.measureText(LABEL[s]).width + 14;
        }
        if (g.setpoints && (d.v_set > 0 || d.i_set > 0)) {
          x2.strokeStyle = cfg.colors.muted;
          x2.setLineDash([4, 3]);
          x2.beginPath(); x2.moveTo(x, 8); x2.lineTo(x + 14, 8); x2.stroke();
          x2.setLineDash([]);
          x2.fillStyle = cfg.colors.muted;
          x2.fillText('Soll / Limit', x + 18, 8);
        }
        x2.textAlign = 'right';
        x2.fillStyle = rgba(cfg.colors.muted, 0.8);
        x2.fillText(g.seconds + ' s', padL + pw, 8);
      }
    }

    // ----------------------------------------------------------- sizing

    let size = { w: 0, h: 0 };
    function syncSize() {
      const w = Math.ceil(ov.offsetWidth * cfg.scale);
      const h = Math.ceil(ov.offsetHeight * cfg.scale);
      if (w !== size.w || h !== size.h) {
        size = { w, h };
        host.style.width = w + 'px';
        host.style.height = h + 'px';
        if (opts.onResize) opts.onResize(size);
        drawSoon();
      }
    }
    const sizer = setInterval(syncSize, 400);

    const api = {
      update(d) { last = d; render(); },
      setDesign(design) { setDesign(design); syncSize(); },
      setLinkDown(down) { if (linkDown !== down) { linkDown = down; render(); } },
      /** Extra zoom of the page around it (studio preview), for a sharp graph. */
      setFit(f) { fit = f || 1; drawSoon(); },
      size() { syncSize(); return { w: size.w, h: size.h }; },
      sparkSeconds() { return on('graph') ? cfg.graph.seconds : 0; },
      remeasure,
      destroy() {
        clearInterval(sizer);
        LIVE.delete(api);
        window.removeEventListener('resize', drawSoon);
        while (root.firstChild) root.removeChild(root.firstChild);
      },
    };
    LIVE.add(api);
    setDesign(design);
    syncSize();
    if (document.fonts && document.fonts.ready) document.fonts.ready.then(() => { if (LIVE.has(api)) remeasure(); });
    window.addEventListener('resize', drawSoon);
    return api;
  }

  // ---------------------------------------------------------- live data

  /** One EventSource with reconnect; `spark` = seconds of graph data. */
  function connect(onSnap, opts) {
    opts = opts || {};
    let es = null, timer = 0, closed = false, spark = opts.spark == null ? 20 : opts.spark;
    function open() {
      if (closed) return;
      es = new EventSource('/events?spark=' + spark);
      es.onmessage = ev => {
        let d;
        try { d = JSON.parse(ev.data); } catch (e) { return; }
        if (opts.onUp) opts.onUp();
        onSnap(d);
      };
      es.onerror = () => {
        es.close();
        if (opts.onDown) opts.onDown();
        clearTimeout(timer);
        timer = setTimeout(open, 1500);
      };
    }
    open();
    return {
      setSpark(s) {
        if (s === spark) return;
        spark = s;
        if (es) es.close();
        clearTimeout(timer);
        open();
      },
      close() { closed = true; clearTimeout(timer); if (es) es.close(); },
    };
  }

  // ---------------------------------------------------------- demo data

  /**
   * Realistic example values for the studio: 19 V in CV with load steps, a
   * short CC phase, a dip, a short, output off for a moment, a multimeter.
   * Resolution like an OWON SPE (10 mV / 1 mA).
   */
  function demo(opts) {
    const LEN = 34;
    const start = opts && opts.start != null ? opts.start : 4;
    const t0 = performance.now() / 1000;
    const noise = t => Math.sin(t * 91.7) * 0.6 + Math.sin(t * 37.3) * 0.4;
    function at(t) {
      const c = ((t % LEN) + LEN) % LEN;
      let v = 19.0, i = 0, mode = 'CV', short = false, dropout = false, on = true;
      if (c < 3) i = 0;
      else if (c < 9) i = 0.42 + 0.03 * Math.sin(c * 5);
      else if (c < 14) i = 0.86 + 0.05 * Math.sin(c * 7) + 0.02 * Math.sin(c * 23);
      else if (c < 17) { i = 1.0; v = 15.6 + 0.3 * Math.sin(c * 3); mode = 'CC'; }
      else if (c < 21) i = 0.58;
      else if (c < 21.35) { v = 13.4; i = 0.41; dropout = true; }
      else if (c < 25) i = 0.58 + 0.02 * Math.sin(c * 9);
      else if (c < 26.2) { v = 0.04; i = 1.0; mode = 'CC'; short = true; }
      else if (c < 30.5) i = 0.118 + 0.004 * Math.sin(c * 4);
      else { on = false; v = c < 30.9 ? 19 * (30.9 - c) / 0.4 : 0; i = 0; mode = 'OFF'; }
      if (mode === 'CV' && on) v -= 0.012 * i;
      v = Math.max(0, v + 0.003 * noise(t));
      i = Math.max(0, i + (i > 0 ? 0.0012 * noise(t * 1.3) : 0));
      return { c, v, i, mode, short, dropout, on };
    }
    function energyAt(c) {
      // rough integral of the scenario, enough for a believable readout
      return (c * 11.5) / 3600;
    }
    let lastSpark = -1;
    let sparkRows = [];
    return {
      next() {
        const t = performance.now() / 1000 - t0 + start;
        const s = at(t);
        const p = s.v * s.i;
        if (t - lastSpark > 0.2) {
          lastSpark = t;
          sparkRows = [];
          for (let k = 600; k >= 0; k--) {
            const tt = t - k / 10;
            const r = at(tt);
            sparkRows.push([-(k / 10), +r.v.toFixed(3), +r.i.toFixed(4), +(r.v * r.i).toFixed(3)]);
          }
        }
        const lastEvent = s.c > 26.2 ? 'Kurzschluss 1.20 s · min 0.04 V' : s.c > 21.35 ? 'Spannungseinbruch 350 ms · min 13.40 V' : null;
        const dmm = 3.3012 + 0.0004 * noise(t * 0.7);
        const e = energyAt(s.c);
        return {
          power_connected: true, psu_connected: true, psu_model: 'OWON SPE3103',
          v: s.v, i: s.i, p, v_text: s.v.toFixed(2), i_text: s.i.toFixed(3), p_text: p.toFixed(p >= 100 ? 1 : 2),
          v_decimals: 2, i_decimals: 3, p_decimals: p >= 100 ? 1 : 2,
          mode: s.mode, v_set: 19, i_set: 1, v_set_text: '19.00', i_set_text: '1.000',
          set_from_device: true, set_learned: false, i_set_learned: false, output_on: s.on, protection: null,
          energy_text: e < 1 ? (e * 1000).toFixed(2) + ' mWh' : e.toFixed(3) + ' Wh',
          charge_text: (e / 19 * 1000 * 1000 / 1000).toFixed(1) + ' mAh', peak_i_text: '1.000 A',
          dropout: s.dropout, short: s.short, dropouts: s.c > 21.35 ? 1 : 0, last_event: lastEvent,
          dmm_connected: true, dmm_value: dmm, dmm_text: dmm.toFixed(4) + ' V', dmm_value_text: dmm.toFixed(4),
          dmm_unit: 'V', dmm_function: 'V DC', dmm_tag: 'DC', design_rev: 0, spark: sparkRows,
        };
      },
    };
  }

  /** The fullest a design can get (all warnings, long texts). */
  function worstCase(small) {
    return {
      power_connected: true, psu_connected: true, psu_model: 'OWON SPE3103',
      v: 28.888, i: small ? 0.888 : 8.888, p: small ? 0.888 : 256.789,
      v_decimals: 2, i_decimals: 3, p_decimals: small ? 2 : 1,
      mode: 'CC', v_set: 30, i_set: 10, v_set_text: '28.88', i_set_text: '8.888', output_on: true,
      protection: 'OVP · OCP', short: true, dropout: false, dropouts: 12,
      last_event: 'Spannungseinbruch 1.25 s · min 13.40 V', energy_text: '123.456 Wh', charge_text: '8888.8 mAh',
      peak_i_text: '8.888 A', dmm_connected: true, dmm_value_text: '33.012', dmm_unit: 'mV', dmm_function: 'V DC',
      dmm_tag: 'DC', spark: [],
    };
  }

  /**
   * Size the design needs in OBS (Browser-Quelle), at its scale and with room
   * for warnings, rounded up to even numbers.
   */
  function measure(design) {
    let box = document.getElementById('pm-measure');
    if (!box) {
      box = el('div');
      box.id = 'pm-measure';
      box.style.cssText = 'position:absolute;left:-20000px;top:0;visibility:hidden;pointer-events:none;';
      document.body.appendChild(box);
    }
    const host = el('div');
    box.appendChild(host);
    const o = mount(host, design, { never_stale: true });
    o.update(worstCase(true));
    o.update(worstCase(false));
    const s = o.size();
    o.destroy();
    box.removeChild(host);
    const even = x => Math.max(2, Math.ceil(x / 2) * 2);
    return { w: even(s.w), h: even(s.h) };
  }

  // ------------------------------------------------------- overlay page

  /** The OBS page: loads its design, follows live data and design edits. */
  function boot(rootEl, noticeEl) {
    const m = /^\/o\/([a-z0-9-]{1,40})\/?$/.exec(location.pathname);
    const api = m ? '/api/designs/' + m[1] : '/api/overlay-design' + location.search;
    let ov = null, rev = null, tried = null, loading = false, conn = null;
    function notice(text) {
      if (!noticeEl) return;
      noticeEl.textContent = text || '';
      noticeEl.style.display = text ? 'inline-block' : 'none';
    }
    async function load() {
      loading = true;
      try {
        const r = await fetch(api, { cache: 'no-store' });
        if (r.status === 404) {
          notice('Dieses Design gibt es nicht mehr. Im Overlay-Studio ein Design wählen und dessen URL in OBS eintragen.');
          if (ov) { ov.destroy(); ov = null; }
          return;
        }
        if (!r.ok) throw new Error(r.status);
        const j = await r.json();
        rev = j.rev;
        notice('');
        if (ov) ov.setDesign(j); else ov = mount(rootEl, j);
        if (conn) conn.setSpark(ov.sparkSeconds());
      } catch (e) {
        setTimeout(load, 2000); // app not running (yet)
      } finally {
        loading = false;
      }
    }
    conn = connect(d => {
      if (ov) ov.update(d);
      if (rev !== null && d.design_rev !== rev && d.design_rev !== tried && !loading) {
        tried = d.design_rev;
        load();
      }
    }, {
      spark: 20,
      onDown: () => ov && ov.setLinkDown(true),
      onUp: () => ov && ov.setLinkDown(false),
    });
    load();
  }

  window.PowerOverlay = { mount, connect, demo, measure, boot, fonts: FONTS, loadBundledFonts };
})();
