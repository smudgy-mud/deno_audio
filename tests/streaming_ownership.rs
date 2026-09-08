//! The real extension drops its worker owner even when native stream handles survive.

use std::sync::{mpsc, Arc};
use std::time::Duration;

use deno_audio::{
    AudioEventPumpLiveness, AudioExtensionOptions, AudioFileStream, AudioFileStreamOutcome,
    AudioHost, AudioHostLimits, AudioLifecycleOwner, AudioLimits, SilentAudioOutput,
};
use deno_core::{JsRuntime, PollEventLoopOptions, RuntimeOptions};
use web_audio_api::context::AudioContext;

const EARCONS_WHILE_STREAMING: &str = r#"
        globalThis.earconsDone = false;
        globalThis.earconsTask = (async () => {
            const context = new AudioContext();
            const bytes = new ArrayBuffer(44 + 256 * 2);
            const view = new DataView(bytes);
            function text(offset, value) {
                for (let i = 0; i < value.length; i++) view.setUint8(offset + i, value.charCodeAt(i));
            }
            text(0, "RIFF"); view.setUint32(4, bytes.byteLength - 8, true);
            text(8, "WAVEfmt "); view.setUint32(16, 16, true);
            view.setUint16(20, 1, true); view.setUint16(22, 1, true);
            view.setUint32(24, 48000, true); view.setUint32(28, 96000, true);
            view.setUint16(32, 2, true); view.setUint16(34, 16, true);
            text(36, "data"); view.setUint32(40, 512, true);
            for (let i = 0; i < 256; i++) view.setInt16(44 + i * 2, 8192, true);
            // All four buffer-loading jobs remain available while eight stream jobs are held.
            const buffers = await Promise.all(Array.from({length: 4}, () => context.decodeAudioData(bytes.slice(0))));
            const cue = buffers[0];
            if (cue.length === 0 || cue.getChannelData(0)[0] !== 0.25) throw new Error("bad earcon decode");
            const ended = [];
            for (let i = 0; i < 8; i++) {
                const source = context.createBufferSource();
                source.buffer = cue;
                source.connect(context.destination);
                ended.push(new Promise(resolve => { source.onended = resolve; }));
                source.start();
            }
            await Promise.all(ended);
            await context.close();
            globalThis.earconsDone = true;
        })();
    "#;

#[tokio::test]
async fn eight_streams_leave_public_earcon_decoding_and_cached_playback_available() {
    let host = Arc::new(AudioHost::new(
        AudioHostLimits::unlimited()
            .max_streaming_jobs(Some(8))
            .max_decode_jobs(Some(4))
            .max_live_audio_bytes(Some(64 * 1024 * 1024)),
    ));
    let (owner, registrar) = AudioLifecycleOwner::new();
    let mut runtime = JsRuntime::new(RuntimeOptions {
        extensions: vec![
            deno_webidl::deno_webidl::init(),
            deno_web::deno_web::init(
                deno_web::BlobStore::default_arc(),
                None,
                Default::default(),
                deno_web::InMemoryBroadcastChannel::default(),
            ),
            deno_audio::deno_audio::init(
                AudioExtensionOptions::new(Arc::clone(&host))
                    .event_pump_liveness(AudioEventPumpLiveness::Ref)
                    .limits(AudioLimits::default())
                    .output_factory(Arc::new(SilentAudioOutput::new()))
                    .lifecycle_registrar(registrar),
            ),
        ],
        ..RuntimeOptions::default()
    });
    let context = AudioContext::builder(Arc::new(SilentAudioOutput::new()))
        .build()
        .unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let mut releases: Vec<mpsc::Sender<()>> = Vec::new();
    let mut streams = Vec::new();
    for _ in 0..8 {
        let entered_tx = entered_tx.clone();
        let (release_tx, release_rx) = mpsc::channel();
        releases.push(release_tx);
        streams.push(
            AudioFileStream::open(&mut runtime.op_state().borrow_mut(), &context, move || {
                entered_tx.send(()).unwrap();
                let _ = release_rx.recv_timeout(Duration::from_secs(5));
                Err(std::io::Error::other("test stream released"))
            })
            .unwrap(),
        );
    }
    for _ in 0..8 {
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    assert_eq!(host.usage().streaming_jobs(), 8);
    assert_eq!(host.usage().decode_jobs(), 0);
    assert!(
        AudioFileStream::open(&mut runtime.op_state().borrow_mut(), &context, || panic!(
            "ninth stream must be refused before opening"
        ))
        .is_err()
    );
    runtime
        .execute_script("earcons_while_streaming.js", EARCONS_WHILE_STREAMING)
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        runtime.run_event_loop(PollEventLoopOptions::default()),
    )
    .await
    .unwrap()
    .unwrap();
    runtime
        .execute_script(
            "assert_earcons_done.js",
            "if (!earconsDone) throw new Error('earcons did not complete');",
        )
        .unwrap();
    assert_eq!(host.usage().streaming_jobs(), 8);
    assert_eq!(host.usage().decode_jobs(), 0);
    for stream in &streams {
        stream.stop();
    }
    drop(releases);
    for stream in &mut streams {
        assert!(
            tokio::time::timeout(Duration::from_secs(5), stream.take_completion().unwrap())
                .await
                .unwrap()
                .is_err()
        );
    }
    assert_eq!(host.usage().streaming_jobs(), 0);
    drop(streams);
    drop(runtime);
    owner.retire().await.unwrap();
    context.close_sync();
}

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
    assert_eq!(host.usage().streaming_jobs(), 1);
    release_tx.send(()).unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), stream.take_completion().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, AudioFileStreamOutcome::Cancelled);
    assert_eq!(host.usage().streaming_jobs(), 0);
    owner.retire().await.unwrap();
    drop(stream);
    context.close_sync();
    std::fs::remove_file(path).unwrap();
}
