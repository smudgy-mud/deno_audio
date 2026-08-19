# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project intends to follow Semantic Versioning once it exposes a public
Web Audio surface.

## [Unreleased]

### Added

- The first public online Web Audio slice: `AudioContext`, destination, Gain,
  fixed-wave Oscillator, scalar `AudioParam.value`, exact connect/disconnect,
  suspend/resume/close, acknowledged state changes, and exactly-once `ended`
  over built-in or host-injected output.
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
- Empty-extension `JsRuntime` compatibility smoke test.
- Silent/offline executable characterizations of the five blocking
  `web-audio-api` 1.7.0 seams.
- Exact-version buy-versus-build survey of existing Deno audio packages.
- Cross-platform baseline CI and dependency-source/license policy.

## [0.0.1]

### Added

- Initial crate-name reservation.
