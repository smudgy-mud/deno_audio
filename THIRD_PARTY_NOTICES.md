# Third-party notices

This project depends directly on the following third-party software in its
Phase 0 baseline. The dependency lockfile records the complete transitive set;
each package remains governed by its own license.

| Package | Version | License | Source |
| --- | ---: | --- | --- |
| `deno_core` | 0.410.0 | MIT | <https://crates.io/crates/deno_core/0.410.0> |
| `deno_error` | 0.7.1 | MIT | <https://crates.io/crates/deno_error/0.7.1> |
| `deno_web` | 0.288.0 | MIT | <https://crates.io/crates/deno_web/0.288.0> |
| `deno_webidl` | 0.257.0 | MIT | <https://crates.io/crates/deno_webidl/0.257.0> |
| `serde_v8` | 0.319.0 | MIT | <https://crates.io/crates/serde_v8/0.319.0> |
| `web-audio-api` | 1.7.0 fork baseline | MIT | <https://github.com/smudgy-mud/web-audio-api-rs/tree/2af21af6ff2bbee17201b2dd2a5ebc56f55c3511> |
| `trybuild` (development only) | 1.0.116 | MIT OR Apache-2.0 | <https://crates.io/crates/trybuild/1.0.116> |

The Deno tuple corresponds to [Deno 2.9.5 commit
`17fadf33a8df3af9488b9f42efd1f2290d6dc7a3`](https://github.com/denoland/deno/commit/17fadf33a8df3af9488b9f42efd1f2290d6dc7a3).
The Smudgy fork currently pins upstream `web-audio-api` 1.7.0 commit
`2af21af6ff2bbee17201b2dd2a5ebc56f55c3511`; it is still the
defect-reproduction baseline rather than a completed production engine.

`web-audio-api` enables Symphonia 0.6.1 with its complete decoder set; Creek
also brings Symphonia 0.5.5 into the locked graph. Symphonia is licensed under
MPL-2.0; no Symphonia source is copied or modified here. AAC, M4A, and ALAC
support is compiled in. Patent and redistribution obligations for codecs and
containers can vary by jurisdiction, so distributors should perform their own
review rather than infer patent clearance from this project's MIT license.

No source, generated bindings, tests, or prose in this repository are currently
adapted from `node-web-audio-api`. If that changes, its BSD-3-Clause notice and
the adapted files must be identified here.
