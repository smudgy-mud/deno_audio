# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project intends to follow Semantic Versioning once it exposes a public
Web Audio surface.

## [Unreleased]

### Changed

- Pin the hosted-output engine to `75dfdeb992f9ae8e63a76e3fcf471ac56f31836d`, rebased
  onto upstream `c01bb99712c1a68387d2898cfcbab55caf92d0b1`. This incorporates
  upstream's borrowed-reader decoding support, bounded native PCM streaming,
  incremental common-format file decoding, and paused-source reclamation. It preserves the hosted resource
  and lifecycle contracts. The Deno dependency tuple and JavaScript API are unchanged.
- Update the transitive `rtrb` lockfile entry to 0.3.5, addressing
  [RUSTSEC-2026-0274](https://rustsec.org/advisories/RUSTSEC-2026-0274.html).

### Added

- Extension-private graph integration for embedder-owned file-source modules.
  Web Audio globals and prototypes have no custom file factory or constructor.
  Context close drains native decoder workers, including sources never started.
  Hosts install an isolate-aware resolver; absent resolvers deny access.
  Encoded files default to a 256 MiB size limit.
- Separate `max_streaming_jobs` admission and `AudioHostUsage::streaming_jobs()`
  accounting from whole-buffer decoding. Per-isolate defaults allow eight streams
  alongside four buffer decodes; both still share the audio byte budget. Cancelled
  workers retain streaming slots until joined.
- Experimental Rust `AudioFileStream` host API: reserve shared/per-isolate jobs,
  working bytes, queue bytes, node, and control admission before opening or spawning.
  An independent observer joins each decoder before releasing its job/workspace
  charges. Source/isolate/context drop and generation retirement cancel workers;
  generation receipts include their joins even when completion observers disappear.
  The working charge is not an allocator-enforced memory ceiling.

- Embedder-owned runtime-generation lifecycle drain through
  `AudioLifecycleOwner`, cloneable `AudioLifecycleRegistrar`, and an exact
  asynchronous retirement receipt. Sealing rejects later online contexts
  before output preparation and waits for confirmed shutdown of every context
  admitted across the generation's extension instances.

## [0.1.0-alpha.1] - 2026-08-19

### Added

- The first public online Web Audio slice: `AudioContext`, destination, Gain,
  fixed-wave Oscillator, one-shot `AudioBufferSourceNode`, scalar
  `AudioParam.value`, exact connect/disconnect, suspend/resume/close,
  acknowledged state changes, and exactly-once `ended` over built-in or
  host-injected output.
- Public offline and hosted `AudioBufferSourceNode` playback with bounded native
  construction and mutation reservations, canonical PCM acquisition, exposed
  view detachment, playback-rate/detune parameters, loop and scheduled playback
  controls, native completion rooting, and generation-safe `ended`
  reconciliation.
- Public `BaseAudioContext.decodeAudioData`, including synchronous encoded-input
  detachment, off-isolate bounded decoding, promise and legacy callback forms,
  target-rate resampling, and decoder-owned PCM reservations.
- Explicit `AudioEventPumpLiveness` policy, defaulting to an unreferenced event
  observer while allowing session embedders to keep Deno's loop alive until
  online contexts retire.
- A bounded public offline Web Audio graph with `OfflineAudioContext`, the base
  node hierarchy, destination, Gain, four fixed Oscillator waveforms,
  `AudioParam` automation, explicit connect/disconnect overloads, off-isolate
  rendering, and generation-local `complete`/`ended` event delivery.
- Per-isolate connection and automation-event limits plus conservative native
  graph/control ownership retained through rendering and eventual wrapper
  teardown.
- A minimal Web Platform Test harness pinned to commit
  `fbc1b90e28b38e919acabeeb14d1b074f2c125ec`, including shell-global aliases,
  a checkout-confined fetch shim, CI execution, and reviewed reasons for every
  skipped Phase 1 file. The initial reviewed subset has 4 passing files, 0
  failing files, and 7 explicitly skipped files.
- The first Phase 1 public Web Audio surface: a primordial-safe, snapshot-safe
  `AudioBuffer` global with Web IDL construction, descriptors, brand checks,
  typed errors, channel-copy methods, same-object channel views, and explicit
  duplicate-global handling.
- Rust-owned canonical `AudioBuffer` storage with separately reserved V8
  channel exposures, garbage-collection recovery, overlap-safe copies, shared
  typed-array rejection, and `DataCloneError` rather than lossy direct
  structured cloning.
- Configurable per-buffer logical-sample and sample-rate limits, with the
  existing channel and live-byte ceilings enforced before native allocation.
- Phase 0 repository skeleton and provenance records.
- Exact Deno 2.9.5-compatible dependency tuple.
- Exact-revision Git dependency on the Smudgy `web-audio-api-rs` fork.
- Direct re-exports of the fork's neutral hosted-output contracts, allowing an
  embedder to implement a logical output factory without adding its own direct
  engine dependency or creating wrapper type identities.
- Empty-extension `JsRuntime` compatibility smoke test.
- Silent/offline executable characterizations of the five blocking
  `web-audio-api` 1.7.0 seams.
- Exact-version buy-versus-build survey of existing Deno audio packages.
- Cross-platform baseline CI and dependency-source/license policy.

### Compatibility

- Requires Rust 1.91 or newer and the coordinated Deno dependency tuple
  `deno_core` 0.410.0, `deno_webidl` 0.257.0, `deno_web` 0.288.0,
  `deno_error` 0.7.1, and `serde_v8` 0.319.0.
- Pins `smudgy-mud/web-audio-api-rs` commit
  `d0e967a136472389312c475ac1851b6836ad7292`, based on
  `web-audio-api` 1.7.0.

## [0.0.1]

### Added

- Initial crate-name reservation.
