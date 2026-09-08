//! File-source script graph, lifecycle, and quota integration.
use deno_audio::{
    install_audio_file_resolver, AudioEventPumpLiveness, AudioExtensionOptions, AudioFileOpener,
    AudioFileResolver, AudioHost, AudioHostLimits, AudioLimits, SilentAudioOutput,
};
use deno_core::{JsRuntime, OpState, PollEventLoopOptions, RuntimeOptions};
use deno_error::JsErrorBox;
use std::fs::File;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

struct Fixtures(PathBuf);
impl Fixtures {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "audio-file-binding-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        for (name, frames) in [("short", 256_u32), ("long", 480_000)] {
            let mut wav = Vec::new();
            wav.extend_from_slice(b"RIFF");
            wav.extend_from_slice(&(36 + frames * 2).to_le_bytes());
            wav.extend_from_slice(b"WAVEfmt ");
            wav.extend_from_slice(&16_u32.to_le_bytes());
            wav.extend_from_slice(&1_u16.to_le_bytes());
            wav.extend_from_slice(&1_u16.to_le_bytes());
            wav.extend_from_slice(&48_000_u32.to_le_bytes());
            wav.extend_from_slice(&96_000_u32.to_le_bytes());
            wav.extend_from_slice(&2_u16.to_le_bytes());
            wav.extend_from_slice(&16_u16.to_le_bytes());
            wav.extend_from_slice(b"data");
            wav.extend_from_slice(&(frames * 2).to_le_bytes());
            for _ in 0..frames {
                wav.extend_from_slice(&8192_i16.to_le_bytes());
            }
            std::fs::write(root.join(name), wav).unwrap();
        }
        std::fs::write(root.join("bad"), b"not audio").unwrap();
        Self(root)
    }
}
impl Drop for Fixtures {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}
struct Resolver(PathBuf);
impl AudioFileResolver for Resolver {
    fn resolve(&self, _state: &OpState, source: &str) -> Result<AudioFileOpener, JsErrorBox> {
        if !["short", "long", "bad"].contains(&source) {
            return Err(JsErrorBox::type_error("denied source"));
        }
        let path = self.0.join(source);
        Ok(Box::new(move || File::open(path)))
    }
}
fn runtime(host: Arc<AudioHost>, limits: AudioLimits, fixtures: Option<&Fixtures>) -> JsRuntime {
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
                AudioExtensionOptions::new(host)
                    .limits(limits)
                    .event_pump_liveness(AudioEventPumpLiveness::Ref)
                    .output_factory(Arc::new(SilentAudioOutput::new())),
            ),
        ],
        ..RuntimeOptions::default()
    });
    if let Some(fixtures) = fixtures {
        install_audio_file_resolver(
            &mut runtime.op_state().borrow_mut(),
            Arc::new(Resolver(fixtures.0.clone())),
        );
    }
    runtime
}
async fn run(runtime: &mut JsRuntime, script: &'static str) {
    runtime.execute_script("file_source.js", format!("globalThis.done = false; (async () => {{ {script}\n globalThis.done = true; }})();")).unwrap();
    tokio::time::timeout(
        Duration::from_secs(15),
        runtime.run_event_loop(PollEventLoopOptions::default()),
    )
    .await
    .unwrap()
    .unwrap();
    runtime
        .execute_script(
            "assert_done.js",
            "if (!done) throw new Error('file source test did not complete');",
        )
        .unwrap();
}

#[tokio::test]
async fn file_node_connects_ends_once_and_rejects_cross_context_and_restarts() {
    let fixtures = Fixtures::new();
    let host = Arc::new(AudioHost::default());
    let mut runtime = runtime(Arc::clone(&host), AudioLimits::default(), Some(&fixtures));
    run(&mut runtime, r"
        const context = new AudioContext(), other = new AudioContext();
        const source = await context.createFileSource('short');
        if (!(source instanceof AudioFileSourceNode) || !(source instanceof AudioNode)) throw new Error('wrong prototype');
        if (source.numberOfInputs !== 0 || source.numberOfOutputs !== 1 || source.error !== null) throw new Error('wrong source shape');
        let refused = false;
        try { source.connect(other.destination); } catch (e) { refused = e.name === 'InvalidAccessError'; }
        if (!refused) throw new Error('cross-context connect accepted');
        const gain = context.createGain(); gain.gain.value = .5;
        source.connect(gain); gain.connect(context.destination);
        let count = 0;
        const ended = new Promise(resolve => { source.onended = () => { count++; resolve(); }; });
        source.start();
        try { source.start(); throw new Error('restart accepted'); } catch(e) { if (e.name !== 'InvalidStateError') throw e; }
        await ended;
        source.stop(); source.stop();
        await context.close(); await other.close();
        if (count !== 1 || source.error !== null) throw new Error('incorrect completion');
    ").await;
    assert_eq!(host.usage().streaming_jobs(), 0);
    assert_eq!(host.usage().scheduled_sources(), 0);
}

#[tokio::test]
async fn file_refusals_and_cancelled_full_queue_leave_slots_reusable() {
    let fixtures = Fixtures::new();
    let host = Arc::new(AudioHost::new(
        AudioHostLimits::unlimited().max_streaming_jobs(Some(1)),
    ));
    let mut runtime = runtime(Arc::clone(&host), AudioLimits::default(), Some(&fixtures));
    run(
        &mut runtime,
        r"
        const c = new AudioContext();
        for (const name of ['bad', 'denied']) {
            let refused = false; try { await c.createFileSource(name); } catch { refused = true; }
            if (!refused) throw new Error('invalid source accepted');
        }
        const long = await c.createFileSource('long');
        let refused = false; try { await c.createFileSource('long'); } catch { refused = true; }
        if (!refused) throw new Error('stream quota ignored');
        long.connect(c.destination);
        const ended = new Promise(resolve => { long.onended = resolve; });
        long.start(); long.stop(); await ended;
        const replacement = await c.createFileSource('short'); replacement.stop();
        await c.close();
    ",
    )
    .await;
    assert_eq!(host.usage().streaming_jobs(), 0);
    assert_eq!(host.usage().scheduled_sources(), 0);
}

#[tokio::test]
async fn close_drains_running_file_and_paused_load_cancels() {
    let fixtures = Fixtures::new();
    let host = Arc::new(AudioHost::default());
    let mut runtime = runtime(Arc::clone(&host), AudioLimits::default(), Some(&fixtures));
    run(&mut runtime, r"
        const c = new AudioContext(); const source = await c.createFileSource('long');
        source.connect(c.destination); let ended = 0; source.onended = () => ended++;
        source.start(); await c.close();
        if (ended !== 1) throw new Error('close lost file completion');
        const d = new AudioContext();
        const pending = d.createFileSource('long').then(() => { throw new Error('closed load succeeded'); }, () => {});
        await d.close(); await pending;
    ").await;
    assert_eq!(host.usage().streaming_jobs(), 0);
}

#[tokio::test]
async fn absent_resolver_and_file_size_limit_fail_closed() {
    let fixtures = Fixtures::new();
    for enabled in [false, true] {
        let host = Arc::new(AudioHost::default());
        let mut runtime = runtime(
            Arc::clone(&host),
            AudioLimits::default().max_streaming_file_bytes(Some(8)),
            enabled.then_some(&fixtures),
        );
        run(
            &mut runtime,
            r"
            const c = new AudioContext(); let refused = false;
            try { await c.createFileSource('short'); } catch { refused = true; }
            if (!refused) throw new Error('source should be refused'); await c.close();
        ",
        )
        .await;
        assert_eq!(host.usage().streaming_jobs(), 0);
    }
}

#[tokio::test]
async fn close_joins_unstarted_sources_and_failed_start_keeps_source_reusable() {
    let fixtures = Fixtures::new();
    let host = Arc::new(AudioHost::new(
        AudioHostLimits::unlimited().max_scheduled_sources(Some(1)),
    ));
    let mut runtime = runtime(Arc::clone(&host), AudioLimits::default(), Some(&fixtures));
    run(
        &mut runtime,
        r"
        const c = new AudioContext();
        const first = await c.createFileSource('long');
        const second = await c.createFileSource('long');
        first.start();
        let refused = false; try { second.start(); } catch { refused = true; }
        if (!refused) throw new Error('scheduled source limit ignored');
        const ended = new Promise(resolve => { first.onended = resolve; });
        first.stop(); await ended;
        second.start(); second.stop(); await c.close();
        const d = new AudioContext();
        await d.createFileSource('long');
        await d.close();
    ",
    )
    .await;
    assert_eq!(host.usage().streaming_jobs(), 0);
    assert_eq!(host.usage().scheduled_sources(), 0);
}
