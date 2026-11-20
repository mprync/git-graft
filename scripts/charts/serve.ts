// Renders the README charts with Excalidraw itself, so they keep its hand-drawn look.
//
//   node scripts/charts/serve.ts
//
// Then open http://localhost:4517 in a browser. The page draws every chart from the
// measurements in charts.js, exports light and dark SVGs plus the editable
// .excalidraw scenes, and posts them back; this server writes them to docs/assets
// and exits once the page reports it is done. Nothing is written outside docs/assets.

import { createServer } from 'node:http';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';

const PORT = 4517;
const here = import.meta.dirname;
const assets = resolve(here, '../../docs/assets');
/** Only chart outputs may be written: a lowercase name and one of the export suffixes. */
const OUTPUT = /^[a-z0-9-]+(?:\.light\.svg|\.dark\.svg|\.excalidraw)$/;
const pages: Record<string, [string, string]> = {
  '/': ['render.html', 'text/html; charset=utf-8'],
  '/charts.js': ['charts.js', 'text/javascript; charset=utf-8'],
};

mkdirSync(assets, { recursive: true });
const written: string[] = [];

const server = createServer((request, response) => {
  const url = new URL(request.url ?? '/', `http://localhost:${PORT}`);
  const page = pages[url.pathname];
  if (request.method === 'GET' && page) {
    response.writeHead(200, { 'content-type': page[1], 'cache-control': 'no-store' });
    response.end(readFileSync(join(here, page[0])));
    return;
  }
  if (request.method === 'POST' && url.pathname === '/done') {
    response.end('ok');
    console.log(`Wrote ${written.length} files to docs/assets:\n  ${written.join('\n  ')}`);
    server.close();
    return;
  }
  const name = url.pathname.startsWith('/save/') ? url.pathname.slice('/save/'.length) : '';
  if (request.method === 'POST' && OUTPUT.test(name)) {
    const chunks: Buffer[] = [];
    request.on('data', (chunk: Buffer) => chunks.push(chunk));
    request.on('end', () => {
      writeFileSync(join(assets, name), Buffer.concat(chunks));
      written.push(name);
      response.end('ok');
    });
    return;
  }
  response.writeHead(404).end();
});

server.listen(PORT, '127.0.0.1', () => console.log(`Open http://localhost:${PORT} to render the charts.`));
