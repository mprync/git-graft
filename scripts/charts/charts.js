// Draws the README charts in Excalidraw from the measured results below,
// then posts light and dark SVGs and the editable .excalidraw scenes to serve.ts.
// Update MEASUREMENTS when the benchmarks change, then rerun `node scripts/charts/serve.ts`.

import { convertToExcalidrawElements, exportToSvg, serializeAsJSON, FONT_FAMILY } from '@excalidraw/excalidraw';

/**
 * Measured results; docs/benchmarks.md describes each benchmark and how to rerun it.
 * Each chart compares git worktree with git graft for the same work.
 */
const MEASUREMENTS = {
  scale: { worktree: 2854.69, graft: 27.76, unit: 'MiB' },
  disk: { worktree: 28.55, graft: 0.29, unit: 'MiB' },
  dependencies: { worktree: 85.38, graft: 0.16, unit: 'MiB' },
  ready: { worktree: 550, graft: 454, unit: 'ms' },
  readyWithDependencies: { worktree: 742, graft: 237, unit: 'ms' },
};

const INK = '#1e1e1e';
const MUTED = '#868e96';
const WORKTREE = { stroke: '#495057', fill: '#ced4da' };
const GRAFT = { stroke: '#2f9e44', fill: '#b2f2bb' };
const font = FONT_FAMILY.Excalifont;

/**
 * Percentage saved by git-graft relative to git worktree, rounded down so a chart never
 * overstates it: one decimal above 90%, where whole numbers would hide the difference.
 */
function saving({ worktree, graft }) {
  const percent = (1 - graft / worktree) * 100;
  return percent >= 90 ? (Math.floor(percent * 10) / 10).toFixed(1) : String(Math.floor(percent));
}

/** How many times faster git-graft is, rounded down to one decimal. */
function speedup({ worktree, graft }) {
  return (Math.floor((worktree / graft) * 10) / 10).toFixed(1);
}

function text(id, x, y, value, size, color = INK, align = 'left') {
  return { type: 'text', id, x, y, text: value, fontSize: size, fontFamily: font, strokeColor: color, textAlign: align };
}

/**
 * One panel: a title, two hand-drawn bars on a baseline, their values and labels, and a
 * callout with an arrow pointing at the git-graft bar. Returns skeletons and the ids of
 * text that must be centred on a point once Excalidraw has measured it.
 */
function panel(prefix, left, { title, subtitle, values, format, callout }) {
  const top = 0;
  // The band between the subtitle and the tallest bar holds the callout.
  const chartTop = top + 200;
  const chartHeight = 270;
  const baseline = chartTop + chartHeight;
  const barWidth = 130;
  const bars = [
    { key: 'worktree', label: 'git worktree', colors: WORKTREE, x: left + 40 },
    { key: 'graft', label: 'git graft', colors: GRAFT, x: left + 250 },
  ];
  const max = Math.max(values.worktree, values.graft);
  const skeletons = [
    text(`${prefix}-title`, left, top, title, 34),
    text(`${prefix}-subtitle`, left, top + 46, subtitle, 20, MUTED),
    { type: 'line', x: left + 10, y: baseline, points: [[0, 0], [420, 0]], strokeColor: INK, strokeWidth: 2, roughness: 1 },
  ];
  const centred = [];
  for (const bar of bars) {
    const height = Math.max(6, (values[bar.key] / max) * chartHeight);
    const middle = bar.x + barWidth / 2;
    skeletons.push(
      {
        type: 'rectangle', x: bar.x, y: baseline - height, width: barWidth, height,
        strokeColor: bar.colors.stroke, backgroundColor: bar.colors.fill, fillStyle: 'hachure',
        strokeWidth: 2, roughness: 1,
      },
      text(`${prefix}-${bar.key}-value`, middle, baseline - height - 40, format(values[bar.key]), 26, bar.colors.stroke, 'center'),
      text(`${prefix}-${bar.key}-label`, middle, baseline + 14, bar.label, 22, INK, 'center'),
    );
    centred.push([`${prefix}-${bar.key}-value`, middle], [`${prefix}-${bar.key}-label`, middle]);
  }
  const graftMiddle = bars[1].x + barWidth / 2;
  const graftTop = baseline - Math.max(6, (values.graft / max) * chartHeight);
  // A short git-graft bar gets the callout above it with an arrow down to the bar;
  // a tall one gets the callout in the band above the bars.
  const calloutY = callout.arrow ? Math.max(110, graftTop - 52 - 140) : 105;
  skeletons.push(text(`${prefix}-callout`, graftMiddle, calloutY, callout.text, 44, GRAFT.stroke, 'center'));
  centred.push([`${prefix}-callout`, graftMiddle]);
  if (callout.arrow) {
    skeletons.push({
      type: 'arrow', x: graftMiddle, y: calloutY + 62,
      points: [[0, 0], [0, graftTop - 52 - (calloutY + 62)]],
      strokeColor: GRAFT.stroke, strokeWidth: 2, roughness: 1, endArrowhead: 'arrow',
    });
  }
  return { skeletons, centred };
}

function chart(name, panels) {
  const parts = panels.map(([prefix, left, spec]) => panel(prefix, left, spec));
  const skeletons = parts.flatMap((part) => part.skeletons);
  const elements = convertToExcalidrawElements(skeletons, { regenerateIds: false });
  const byId = new Map(elements.map((element) => [element.id, element]));
  for (const [id, middle] of parts.flatMap((part) => part.centred)) {
    const element = byId.get(id);
    element.x = middle - element.width / 2;
  }
  // Fixed seeds keep the hand-drawn strokes identical between renders, so regenerating
  // a chart only changes the files when the numbers or layout change.
  elements.forEach((element, index) => {
    element.seed = 1000 + index;
    element.versionNonce = 2000 + index;
    element.updated = 1;
  });
  return { name, elements };
}

const mib = (value) => (value >= 1024 ? `${(value / 1024).toFixed(1)} GiB` : `${value < 10 ? value.toFixed(2) : value.toFixed(1)} MiB`);
const m = MEASUREMENTS;
const CHARTS = [
  chart('scale', [['scale', 0, {
    title: '100 checkouts',
    subtitle: 'Total disk for 100 copies of one repository',
    values: m.scale, format: mib,
    callout: { text: `${saving(m.scale)}% less`, arrow: true },
  }]]),
  chart('disk', [['disk', 0, {
    title: 'Disk per checkout',
    subtitle: 'A graft stores only what you change',
    values: m.disk, format: mib,
    callout: { text: `${saving(m.disk)}% less`, arrow: true },
  }]]),
  chart('dependencies', [['dependencies', 0, {
    title: 'node_modules per checkout',
    subtitle: 'Grafts share one install; no npm ci',
    values: m.dependencies, format: mib,
    callout: { text: `${saving(m.dependencies)}% less`, arrow: true },
  }]]),
  chart('speed', [['speed', 0, {
    title: 'Ready to work',
    subtitle: 'New checkout plus its first git status',
    values: m.ready, format: (value) => `${value} ms`,
    callout: { text: `${saving(m.ready)}% faster`, arrow: false },
  }]]),
  chart('speed-dependencies', [['speedDependencies', 0, {
    title: 'Ready to test',
    subtitle: 'New checkout with node_modules installed',
    values: m.readyWithDependencies, format: (value) => `${value} ms`,
    callout: { text: `${speedup(m.readyWithDependencies)}× faster`, arrow: true },
  }]]),
];

async function save(file, body) {
  const response = await fetch(`/save/${file}`, { method: 'POST', body });
  if (!response.ok) throw new Error(`saving ${file} failed: ${response.status}`);
}

const status = document.getElementById('status');
try {
  for (const { name, elements } of CHARTS) {
    for (const dark of [false, true]) {
      const svg = await exportToSvg({
        elements,
        appState: { exportBackground: true, viewBackgroundColor: '#ffffff', exportWithDarkMode: dark, exportPadding: 36 },
        files: null,
      });
      await save(`${name}.${dark ? 'dark' : 'light'}.svg`, new XMLSerializer().serializeToString(svg));
      if (!dark) document.getElementById('preview').append(svg);
    }
    await save(`${name}.excalidraw`, serializeAsJSON(elements, { viewBackgroundColor: '#ffffff' }, {}, 'local'));
  }
  await fetch('/done', { method: 'POST' });
  status.textContent = 'Done';
} catch (error) {
  status.textContent = `Failed: ${error.stack ?? error}`;
  throw error;
}
