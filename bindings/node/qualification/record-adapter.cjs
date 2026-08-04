'use strict';

// This adapter deliberately owns no vocabulary. A consumer supplies ordered, explicitly typed
// fields and independently named byte values; the adapter only translates that self-described
// envelope to the native binding. Keeping it outside the package surface is part of the proof: a
// useful consumer mapping does not require a trace concept in TurnDB core.

function nativeAttr(field) {
  if (!field || typeof field.name !== 'string' || field.name.length === 0) {
    throw new TypeError('a field needs a non-empty string name');
  }
  switch (field.type) {
    case 'string':
      return { name: field.name, kind: 'string', stringValue: field.value };
    case 'int':
      return { name: field.name, kind: 'int', intValue: field.value };
    case 'uint':
      return { name: field.name, kind: 'uint', uintValue: field.value };
    case 'float':
      return { name: field.name, kind: 'float', floatValue: field.value };
    case 'bool':
      return { name: field.name, kind: 'bool', boolValue: field.value };
    case 'binary':
      return { name: field.name, kind: 'binary', binaryValue: field.value };
    case 'timestamp_ns':
      return { name: field.name, kind: 'timestamp_ns', timestampNsValue: field.value };
    case 'null':
      return { name: field.name, kind: 'null' };
    default:
      throw new TypeError(`unsupported field type: ${field.type}`);
  }
}

function putRecord(record) {
  if (!record || typeof record.id !== 'string' || record.id.length === 0) {
    throw new TypeError('a record needs a non-empty string id');
  }
  return {
    kind: 'put',
    id: record.id,
    attrs: (record.fields ?? []).map(nativeAttr),
    contents: (record.contents ?? []).map(({ name, bytes }) => ({ name, bytes })),
  };
}

module.exports = { nativeAttr, putRecord };
