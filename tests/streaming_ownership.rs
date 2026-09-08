//! The real extension drops its worker owner even when native stream handles survive.

use std::sync::{mpsc, Arc};
use std::time::Duration;

use deno_audio::{
    AudioExtensionOptions, AudioFileStream, AudioFileStreamOutcome, AudioHost, AudioLifecycleOwner,
    SilentAudioOutput,
};
use deno_core::{JsRuntime, RuntimeOptions};
use web_audio_api::context::AudioContext;

#[tokio::test]
async fn dropping_runtime_cancels_admitted_open_without_releasing_its_join_obligation() {
    let host = Arc::new(AudioHost::default());
    let (owner, registrar) = AudioLifecycleOwner::new();
    let runtime = JsRuntime::new(RuntimeOptions {
        extensions: vec![
            deno_webidl::deno_webidl::init(),
            deno_web::deno_web::init(
                deno_web::BlobStore::default_arc(),
                None,
                Default::default(),
                deno_web::InMemoryBroadcastChannel::default(),
            ),
            deno_audio::deno_audio::init(
                AudioExtensionOptions::new(Arc::clone(&host)).lifecycle_registrar(registrar),
            ),
        ],
        ..RuntimeOptions::default()
    });
    let context = AudioContext::builder(Arc::new(SilentAudioOutput::new()))
        .build()
        .unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    // A temporary empty regular file is enough: cancellation must win before probing it.
    let path = std::env::temp_dir().join(format!(
        "deno-stream-runtime-drop-{}.bin",
        std::process::id()
    ));
    std::fs::write(&path, []).unwrap();
    let opened_path = path.clone();
    let mut stream =
        AudioFileStream::open(&mut runtime.op_state().borrow_mut(), &context, move || {
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            std::fs::File::open(opened_path)
        })
        .unwrap();
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    drop(runtime);
    assert_eq!(host.usage().decode_jobs(), 1);
    release_tx.send(()).unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), stream.take_completion().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, AudioFileStreamOutcome::Cancelled);
    assert_eq!(host.usage().decode_jobs(), 0);
    owner.retire().await.unwrap();
    drop(stream);
    context.close_sync();
    std::fs::remove_file(path).unwrap();
}
