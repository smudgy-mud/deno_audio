# Contributing

`deno_audio` is still in a compatibility-spike phase. Please open an issue
before investing in a broad feature implementation so dependency and lifecycle
work is not duplicated.

Before submitting a change, run:

```console
cargo fmt --check
cargo check --all-targets --locked
cargo test --all-targets --locked
cargo test --all-targets --no-default-features --locked
cargo test --doc --locked
cargo clippy --all-targets --locked -- -D warnings
cargo deny check
```

Changes to `deno_core`, `deno_webidl`, `deno_web`, `deno_error`, or `serde_v8`
must update the complete compatibility tuple in one reviewed change. Changes to
the audio engine must include lifecycle and resource-safety evidence.

Contributions are accepted under the repository's MIT license.
