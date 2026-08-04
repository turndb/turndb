import native from './index.cjs';

// Keep ESM exports explicit. The CommonJS facade is assembled from native exports at runtime, so
// relying on Node's static CommonJS named-export detection would make `import { NativeStore }`
// dependent on an implementation heuristic rather than the package contract.
export const capabilities = native.capabilities;
export const retainedCommits = native.retainedCommits;
export const recoverManifest = native.recoverManifest;
export const restoreBackup = native.restoreBackup;
export const NativeSqlQuery = native.NativeSqlQuery;
export const NativeSnapshot = native.NativeSnapshot;
export const NativeStore = native.NativeStore;
export const TurnDbError = native.TurnDbError;

export default native;
