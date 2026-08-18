# Existing Deno audio packages

Survey date: 2026-08-17

This is a buy-versus-build gate for an embedder-native `deno_core::Extension`.
Package versions are exact and all source links are versioned or include the
surveyed commit. None of the packages below supplies a host-injected output
factory or a narrow embedder permission boundary.

## Comparison

| Package | Exact baseline | API scope | Native loading and permissions | Claimed/published targets | Device and mixer ownership | Teardown | Reuse decision |
| --- | --- | --- | --- | --- | --- | --- | --- |
| [`@brass/audio`](https://jsr.io/@brass/audio/0.1.0) | JSR 0.1.0, MIT in `deno.json` | A Web-Audio-styled `AudioContext` plus file-backed `Sound`: one-shots, 64 concurrent sounds, events, fades, looping, pan, pitch, and spatial controls. It is not a Web Audio graph implementation. | A module-global [`Deno.dlopen`](https://jsr.io/@brass/audio/0.1.0/src/ffi.ts) handle loads an embedded miniaudio library. It requires `--allow-ffi`; native `audio_load(path)` also opens author-provided paths outside Deno's ordinary `read` checks. | The README says Windows only. The package contains Linux/macOS loaders, but the published Linux and macOS build modules throw stored build failures, so they are not usable targets in 0.1.0. | The FFI library exposes one global `audio_init`/`audio_uninit` engine. Multiple JS `AudioContext` objects therefore address the same native engine rather than independent contexts or an injected host mixer. | Explicit, idempotent `AudioContext.close()` stops polling, disposes tracked sounds, and calls `audio_uninit`. `Sound.dispose()` unloads one handle. There is no `Symbol.dispose` or finalizer on the context, and the module-level dynamic library is never closed. | Do not reuse. The explicit idempotent close and one-shot cleanup are useful UX references; the process-global engine, direct path access, and absent graph semantics violate the embedder contract. |
| [`@music/audio`](https://jsr.io/@music/audio/0.2.1) | JSR 0.2.1; repository commit [`02e6be98`](https://github.com/music-ts/audio/tree/02e6be98e503443983c29821a148422f689459d7); MIT | Low-level PortAudio device/host enumeration, input/output streams, generators, sample conversion, FFT, and WAV utilities. It is not Web Audio. | [`initAudioApi`](https://jsr.io/@music/audio/0.2.1/audioApi/mod.ts) checks for a native library, downloads a binary from the mutable repository `main` branch when absent, writes it, then calls `Deno.dlopen`. First use therefore needs network, write/read metadata, and FFI authority; later use still needs file and FFI authority. | The loader maps Windows, macOS, and Linux suffixes. Published code does not encode an architecture check, integrity digest, or immutable download revision. | Every `AudioApi` calls process-wide `Pa_Initialize`/`Pa_Terminate`; every `play()`/`record()` opens its own PortAudio stream. There is no process mixer or logical-input abstraction. | `AudioApi` and streams implement `Symbol.dispose`. Stream `stop()` drains and closes; `abort()` discards and closes. The API can terminate PortAudio while streams still reference it, and no ownership guard is visible. | Do not reuse. Device enumeration, explicit `using`, and stop-versus-abort are useful API references. Mutable binary installation, broad authority, global initialization, and direct stream ownership are incompatible. |
| [`@mutefish/web-audio-api`](https://jsr.io/@mutefish/web-audio-api/0.1.4) | JSR 0.1.4; repository commit [`0d5b9764`](https://github.com/Mutefish0/web-audio-api/tree/0d5b976451b8232044d32bfa6f5981b795baa3da); package declares MIT | Re-exports 20 Web Audio constructors from a bundled `node-web-audio-api` native addon, including online/offline contexts and common nodes. | A 17.5 MB base64 ZIP is unpacked in memory. [`deno-assets-module` 0.0.4](https://jsr.io/@mutefish/deno-assets-module/0.0.4/mod.ts) writes the selected `.node` addon to a temporary file, loads it through Node compatibility, and removes the file. This needs native-addon/FFI and temporary-file authority. | The bundled repository archive contains aarch64 macOS/Linux plus x86_64 macOS/Linux/Windows addons. Selection uses exact `Deno.build.target`; other targets fail. | Ownership and lifecycle remain inside the native binding's `AudioContext`; the package provides no extension state, permission callback, device permit, or injected mixer. | It exposes the binding's context methods but adds no package-level lifetime controller, host acknowledgement, or isolate teardown integration. | Do not reuse. It demonstrates Deno can load the Node binding and is an API-parity reference. It cannot satisfy host ownership or permission requirements. |
| [`littledivy/deno_audio`](https://github.com/littledivy/deno_audio/tree/ba12323ed94ef064096597f5774a640ff25329d5) | deno.land/x import 0.2.0; crate manifest 0.1.0; commit `ba12323e`; README says MIT but the repository has no license file | One async `play(filename)` function; MP3, WAV, Vorbis, and FLAC through rodio 0.12.0. | A `deno_bindgen` 0.5.0 native library opens the path with Rust `std::fs`, so examples use unstable `-A` and file access occurs beyond Deno's ordinary `read` checks. | Rodio/CPAL targets depend on their backend; the repository names Linux's `libasound2-dev` prerequisite and publishes no current target/support matrix. | Every `play` call constructs a fresh default `rodio::OutputStream` and `Sink`, hence a fresh default-device endpoint rather than a shared process mixer. | The binding sleeps until playback ends; Rust drops the stream and sink on return. It exposes no cancel, close, device selection, error channel, or reusable context. | Do not reuse. It is useful historical evidence of demand and of the retired native-binding approach only. |

## Provenance and permission findings

- `@mutefish/web-audio-api` 0.1.4 does not identify which
  `node-web-audio-api` release or commit produced its bundled JS/addons. Its JSR
  metadata declares MIT, while upstream `node-web-audio-api` uses
  BSD-3-Clause. Treat the bundle's provenance and notice completeness as
  unresolved, not as an attributable vendoring route.
- `@music/audio` 0.2.1 downloads a platform library from a mutable branch with
  no hash check. This is a supply-chain blocker independently of its API fit.
- Native code that receives a filename can open it without Deno consulting the
  caller's `--allow-read` scope. FFI or addon authority is consequently much
  broader than the host-supplied, operation-specific playback permission this
  project requires.

## Gate result

Build the `deno_core::Extension` and output abstraction directly. Borrow only
API ideas: explicit/idempotent close, `using` support, stop-versus-abort, device
enumeration shapes, and Deno-loadability test cases. Do not import any surveyed
package into the extension or expose direct FFI/addon loading to JavaScript.
