// Refuse to test a STALE `.wasm` instead of silently reporting the old engine's behaviour.
//
// `turndb.wasm` is generated, never committed — it is gitignored, has never appeared in any commit,
// and `npm/build.sh` produces it. That is deliberate: the repository carries source, not a 1.1 MB
// binary. The cost is a trap, and it has already caught someone: running `node --test` directly
// after changing Rust exercises whatever artifact happens to be on disk, so an engine fix looks
// absent and a verifier concludes the commit is broken. It is the same shape as the stale built-SDK
// trap that cost two people time on CommandSuite, and it fails the same way — a plausible result
// rather than an error.
//
// So: compare the artifact against the newest engine source and refuse if it is older. Build with
// `bash npm/build.sh`, which rebuilds and then runs these tests in the right order.
import { statSync, readdirSync, existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const pkg = dirname(here);
const root = dirname(dirname(pkg));
const WASM = join(pkg, 'turndb.wasm');

function newestSource(dir, newest = 0) {
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, e.name);
    if (e.isDirectory()) newest = newestSource(p, newest);
    else if (e.name.endsWith('.rs')) newest = Math.max(newest, statSync(p).mtimeMs);
  }
  return newest;
}

export function assertFreshArtifact() {
  if (!existsSync(WASM)) {
    throw new Error(`turndb.wasm is missing — it is generated, not committed. Run: bash npm/build.sh`);
  }
  const built = statSync(WASM).mtimeMs;
  let src = 0;
  for (const d of [join(root, 'src'), join(root, 'bindings')]) {
    if (existsSync(d)) src = newestSource(d, src);
  }
  if (src > built) {
    throw new Error(
      `turndb.wasm is STALE: engine source is newer than the built artifact, so these tests would ` +
        `report the old engine's behaviour and a real fix would look absent. Run: bash npm/build.sh`,
    );
  }
}

assertFreshArtifact();
