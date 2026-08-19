// Copyright 2026 the deno_audio authors. MIT license.

import { core, primordials } from "ext:core/mod.js";

const webidl = core.loadExtScript("ext:deno_webidl/00_webidl.js");
const { DOMException } = core.loadExtScript(
  "ext:deno_web/01_dom_exception.js",
);

const {
  ObjectDefineProperty,
  ObjectGetOwnPropertyDescriptor,
  TypeError,
} = primordials;

function installGlobal(name, value) {
  const existing = ObjectGetOwnPropertyDescriptor(globalThis, name);
  if (existing !== undefined) {
    if (existing.value === value) return;
    throw new TypeError(
      `deno_audio cannot install '${name}' because the global already exists`,
    );
  }
  ObjectDefineProperty(globalThis, name, {
    __proto__: null,
    value,
    writable: true,
    enumerable: false,
    configurable: true,
  });
}

function domException(message, name) {
  return new DOMException(message, name);
}

export { core, domException, installGlobal, primordials, webidl };
