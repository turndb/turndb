import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { createServer } from 'node:http';
import { chromium, firefox } from 'playwright';

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, '../../..');
const selected = process.env.TURNDB_BROWSER;
const browsers = selected
  ? [[selected, { chromium, firefox }[selected]]]
  : [['chromium', chromium], ['firefox', firefox]];

for (const [name, browserType] of browsers) {
  if (!browserType) throw new Error(`unknown TURNDB_BROWSER ${JSON.stringify(selected)}`);
  const browser = await browserType.launch({ headless: true });
  const scratch = await mkdtemp(join(tmpdir(), 'turndb-browser-viewer-'));
  try {
    const hex = await readFile(join(root, 'conformance/v1/fixture.turndb.hex'), 'utf8');
    const fixture = join(scratch, 'fixture.turndb');
    await writeFile(fixture, Buffer.from(hex.replaceAll(/\s/g, ''), 'hex'));
    const page = await browser.newPage();
    const localNetwork = [];
    page.on('request', (request) => {
      if (/^https?:/.test(request.url())) localNetwork.push(request.url());
    });
    await page.goto(pathToFileURL(join(root, 'bindings/browser/turndb-viewer.html')).href);
    await page.locator('#file').setInputFiles(fixture);
    await page.locator('#run').waitFor({ state: 'visible' });
    await assert.doesNotReject(() => page.locator('#run').click());
    await page.waitForFunction(() => document.querySelector('#metrics')?.textContent.includes('3 rows'));
    assert.match(await page.locator('#status').textContent(), /Open: fixture\.turndb · 53406 bytes/);
    assert.match(await page.locator('#metrics').textContent(), /3 rows · examined 3/);
    assert.equal(await page.locator('#error').textContent(), '');
    assert.deepEqual(localNetwork, [], 'local-file mode must make no network requests');

    const viewer = await readFile(join(root, 'bindings/browser/turndb-viewer.html'));
    const fixtureBytes = await readFile(fixture);
    let rangeRequests = 0;
    const server = createServer((request, response) => {
      if (request.url === '/') {
        response.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
        response.end(viewer);
        return;
      }
      if (request.url !== '/fixture.turndb') {
        response.writeHead(404).end();
        return;
      }
      const match = /^bytes=(\d+)-(\d+)$/.exec(request.headers.range ?? '');
      if (!match) {
        response.writeHead(416).end();
        return;
      }
      const start = Number(match[1]);
      const end = Math.min(Number(match[2]), fixtureBytes.length - 1);
      rangeRequests++;
      response.writeHead(206, {
        'Accept-Ranges': 'bytes',
        'Access-Control-Allow-Origin': '*',
        'Content-Length': end - start + 1,
        'Content-Range': `bytes ${start}-${end}/${fixtureBytes.length}`,
      });
      response.end(fixtureBytes.subarray(start, end + 1));
    });
    await new Promise((resolveListen) => server.listen(0, '127.0.0.1', resolveListen));
    try {
      const address = server.address();
      const origin = `http://127.0.0.1:${address.port}`;
      await page.goto(origin);
      await page.locator('#url').fill(`${origin}/fixture.turndb`);
      await page.locator('#open-url').click();
      await page.waitForFunction(() =>
        document.querySelector('#status')?.textContent.includes('53406 bytes')
          || document.querySelector('#error')?.textContent.length > 0);
      assert.equal(await page.locator('#error').textContent(), '');
      assert.match(await page.locator('#status').textContent(), /53406 bytes/);
      await page.locator('#run').click();
      await page.waitForFunction(() => document.querySelector('#metrics')?.textContent.includes('3 rows'));
      assert(rangeRequests > 1, 'URL mode must perform positioned range requests');
      assert.match(await page.locator('#metrics').textContent(), /fetched \d+ bytes/);
      assert.equal(await page.locator('#error').textContent(), '');
    } finally {
      await new Promise((resolveClose, rejectClose) => server.close((error) => error ? rejectClose(error) : resolveClose()));
    }
    console.log(`${name}: local and HTTP-range fixtures opened and queried`);
  } finally {
    await browser.close();
    await rm(scratch, { recursive: true, force: true });
  }
}
