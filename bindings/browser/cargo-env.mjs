import { homedir } from 'node:os';
import { join, resolve } from 'node:path';

export function reproducibleCargoEnv(root) {
  const cargoHome = resolve(root, process.env.CARGO_HOME ?? join(homedir(), '.cargo'));
  const remaps = [
    `--remap-path-prefix=${root}=/workspace`,
    `--remap-path-prefix=${cargoHome}=/cargo`,
  ];
  const env = { ...process.env };

  if (env.CARGO_ENCODED_RUSTFLAGS) {
    env.CARGO_ENCODED_RUSTFLAGS += `\x1f${remaps.join('\x1f')}`;
  } else {
    env.RUSTFLAGS = [env.RUSTFLAGS, ...remaps].filter(Boolean).join(' ');
  }

  return env;
}
