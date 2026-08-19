# Web Platform Test subset

The Phase 1 conformance runner executes selected upstream Web Platform Tests
without copying them into this repository. It is pinned to WPT commit
`fbc1b90e28b38e919acabeeb14d1b074f2c125ec`; the same revision appears in
`expectations.json` and CI.

Every listed test is either `pass` or has a specific `skip` reason. A skipped
file is not partially rewritten or counted as conformance. Unexpected failures
fail the Rust test and CI.

For a local run, create a sparse checkout and point the runner at it:

```console
git clone --filter=blob:none --no-checkout https://github.com/web-platform-tests/wpt target/wpt
git -C target/wpt sparse-checkout set resources webaudio/resources webaudio/the-audio-api/the-audiobuffer-interface webaudio/the-audio-api/the-audionode-interface webaudio/the-audio-api/the-gainnode-interface webaudio/the-audio-api/the-offlineaudiocontext-interface webaudio/the-audio-api/the-oscillatornode-interface
git -C target/wpt checkout fbc1b90e28b38e919acabeeb14d1b074f2c125ec
DENO_AUDIO_WPT_ROOT=target/wpt cargo test --test wpt --no-default-features --locked -- --nocapture
```

Without `DENO_AUDIO_WPT_ROOT` (and without `target/wpt`), the integration test
prints the checkout command and returns. CI always supplies the pinned checkout,
so the conformance gate cannot be skipped there.
