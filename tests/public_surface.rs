//! Host integration must not extend the standard Web Audio global surface.
use deno_core::{JsRuntime, RuntimeOptions};

#[test]
fn file_helpers_are_absent_from_web_audio_globals_and_prototypes() {
    // V8 can schedule delayed GC tasks even in a synchronous bootstrap test.
    let tokio_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _tokio_context = tokio_runtime.enter();
    let mut runtime = JsRuntime::new(RuntimeOptions {
        extensions: vec![
            deno_webidl::deno_webidl::init(),
            deno_web::deno_web::init(
                deno_web::BlobStore::default_arc(),
                None,
                Default::default(),
                deno_web::InMemoryBroadcastChannel::default(),
            ),
            deno_audio::deno_audio::init(deno_audio::AudioExtensionOptions::default()),
        ],
        ..Default::default()
    });
    runtime.execute_script("surface.js", r"
        for (const name of ['AudioFileSourceNode', 'HostAudioNode', 'createFileSource']) {
            if (name in globalThis) throw new Error(`Nonstandard global: ${name}`);
        }
        for (const ctor of [AudioContext, BaseAudioContext, OfflineAudioContext, AudioNode]) {
            if ('createFileSource' in ctor.prototype) throw new Error('Nonstandard Web Audio method');
        }
    ").unwrap();
}
