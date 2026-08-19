# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project intends to follow Semantic Versioning once it exposes a public
Web Audio surface.

## [Unreleased]

### Added

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
