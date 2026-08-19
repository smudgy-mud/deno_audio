// Copyright 2026 the deno_audio authors. MIT license.

(() => {
  "use strict";

  const core = Deno.core;
  const { DOMException } = core.loadExtScript(
    "ext:deno_web/01_dom_exception.js",
  );
  const { Event, EventTarget } = core.loadExtScript(
    "ext:deno_web/02_event.js",
  );
  const { TextDecoder } = core.loadExtScript(
    "ext:deno_web/08_text_encoding.js",
  );

  const define = (name, value) => {
    Object.defineProperty(globalThis, name, {
      value,
      writable: true,
      configurable: true,
    });
  };

  define("self", globalThis);
  define("window", globalThis);
  define("DOMException", DOMException);
  define("Event", Event);
  define("EventTarget", EventTarget);
  define("TextDecoder", TextDecoder);
  if (globalThis.console === undefined) {
    const ignore = () => {};
    define("console", Object.freeze({ log: ignore, info: ignore, warn: ignore, error: ignore }));
  }

  const decoder = new TextDecoder();
  const resolveRequest = (request) => {
    if (request.includes("://") || request.startsWith("/")) return request;
    const suffixIndex = request.search(/[?#]/);
    const suffix = suffixIndex === -1 ? "" : request.slice(suffixIndex);
    const path = suffixIndex === -1 ? request : request.slice(0, suffixIndex);
    const segments = typeof globalThis.META_TITLE === "string"
      ? globalThis.META_TITLE.split("/")
      : [];
    segments.pop();
    for (const segment of path.split("/")) {
      if (segment === "" || segment === ".") continue;
      if (segment === "..") {
        if (segments.length === 0) {
          throw new TypeError("WPT fetch path escapes the pinned checkout");
        }
        segments.pop();
      } else {
        segments.push(segment);
      }
    }
    return `/${segments.join("/")}${suffix}`;
  };
  define("fetch", (input) => {
    const request = typeof input === "string" ? input : input?.url;
    if (typeof request !== "string") {
      return Promise.reject(new TypeError("WPT fetch requires a URL string"));
    }
    try {
      const resolved = resolveRequest(request);
      const bytes = core.ops.op_wpt_read_file(resolved);
      const decode = () => decoder.decode(bytes);
      return Promise.resolve(Object.freeze({
        ok: true,
        status: 200,
        statusText: "OK",
        url: resolved,
        text: async () => decode(),
        json: async () => JSON.parse(decode()),
        arrayBuffer: async () => bytes.slice().buffer,
      }));
    } catch (error) {
      return Promise.reject(error);
    }
  });

  define("__denoAudioInstallWptCallbacks", (title) => {
    define("META_TITLE", title);
    define("location", Object.freeze({ pathname: `/${title}` }));
    define("__denoAudioWptResults", []);
    define("__denoAudioWptDone", false);
    define("__denoAudioWptHarnessStatus", null);

    setup({ output: false, explicit_done: false });
    add_result_callback((result) => {
      __denoAudioWptResults.push({
        name: result.name,
        status: result.status,
        message: result.message || "",
        stack: result.stack || "",
      });
    });
    add_completion_callback((_tests, status) => {
      globalThis.__denoAudioWptHarnessStatus = {
        status: status.status,
        message: status.message || "",
        stack: status.stack || "",
      };
      globalThis.__denoAudioWptDone = true;
    });
  });
})();
