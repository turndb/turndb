'use strict';

const fs = require('node:fs');
const path = require('node:path');

function readPrebuildManifestSet(dist) {
  const manifestNames = fs.readdirSync(dist)
    .filter((name) => /^prebuild-manifest-.+\.json$/.test(name))
    .sort();
  if (manifestNames.length === 0) {
    throw new Error(`no target-qualified prebuild manifests found in ${dist}`);
  }

  const manifests = new Map();
  const entries = new Map();
  for (const name of manifestNames) {
    const manifest = JSON.parse(fs.readFileSync(path.join(dist, name), 'utf8'));
    const expectedName = `prebuild-manifest-${manifest.npmTarget}.json`;
    if (manifest.schema !== 2 || name !== expectedName || !manifest.tarballs?.length) {
      throw new Error(`unsupported or inconsistent prebuild manifest at ${path.join(dist, name)}`);
    }
    if (manifests.has(manifest.npmTarget)) {
      throw new Error(`duplicate prebuild manifest for ${manifest.npmTarget}`);
    }
    manifests.set(manifest.npmTarget, manifest);
    for (const entry of manifest.tarballs) {
      const prior = entries.get(entry.file);
      if (prior && JSON.stringify(prior) !== JSON.stringify(entry)) {
        throw new Error(`prebuild manifests disagree on duplicate tarball ${entry.file}`);
      }
      entries.set(entry.file, entry);
    }
  }
  const versions = new Set([...manifests.values()].map(({ version }) => version));
  const commits = new Set([...manifests.values()].map(({ sourceCommit }) => sourceCommit));
  if (versions.size !== 1 || commits.size !== 1) {
    throw new Error(
      `prebuild manifests disagree on version/source commit: ${[...versions]} / ${[...commits]}`,
    );
  }
  return { manifests, entries };
}

function selectInstallTarballs(entries, hostTarget, version) {
  const rootTarball = entries.get(`turndb-native-${version}.tgz`);
  const targetTarball = entries.get(`turndb-native-${hostTarget}-${version}.tgz`);
  if (!rootTarball || !targetTarball) {
    throw new Error(`prebuild manifest does not contain both root and ${hostTarget} packages`);
  }
  return { rootTarball, targetTarball };
}

module.exports = { readPrebuildManifestSet, selectInstallTarballs };
