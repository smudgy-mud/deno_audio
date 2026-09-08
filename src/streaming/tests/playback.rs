use super::*;
use web_audio_api::context::{AudioContextOptions, OfflineAudioContext};
use web_audio_api::node::AudioScheduledSourceNode;
use web_audio_api::output::*;

#[derive(Default)]
struct Probe {
    stop: AtomicBool,
    after_cancel: AtomicBool,
    full_mix: AtomicBool,
    cancelled_mix: AtomicBool,
}
struct Factory(Arc<Probe>);
struct Prepared {
    config: AudioOutputConfig,
    probe: Arc<Probe>,
}
struct Running {
    probe: Arc<Probe>,
    join: thread::JoinHandle<()>,
}

impl AudioOutputFactory for Factory {
    fn prepare(
        &self,
        request: &AudioOutputRequest,
    ) -> Result<Box<dyn PreparedAudioOutput>, AudioOutputError> {
        Ok(Box::new(Prepared {
            config: AudioOutputConfig::new(
                AudioRenderFormat::new(48_000.0, 2, 128)?,
                request.sink_id(),
                0.0,
            )?,
            probe: Arc::clone(&self.0),
        }))
    }
}
impl PreparedAudioOutput for Prepared {
    fn config(&self) -> &AudioOutputConfig {
        &self.config
    }
    fn start(
        self: Box<Self>,
        mut callback: AudioRenderCallback,
        _events: AudioOutputEventSink,
    ) -> Result<Box<dyn RunningAudioOutput>, AudioOutputStartFailure> {
        let probe = Arc::clone(&self.probe);
        let join = thread::spawn(move || {
            let mut output = [0.0; 256];
            while !probe.stop.load(Ordering::Acquire) {
                let _ = callback.render_interleaved_f32(&mut output);
                if output.contains(&0.625) {
                    probe.full_mix.store(true, Ordering::Release);
                }
                if probe.after_cancel.load(Ordering::Acquire) && output.contains(&0.59375) {
                    probe.cancelled_mix.store(true, Ordering::Release);
                }
                thread::sleep(Duration::from_micros(2667));
            }
        });
        Ok(Box::new(Running {
            probe: self.probe,
            join,
        }))
    }
    fn abort(self: Box<Self>) -> AudioOutputEndpointShutdown {
        AudioOutputEndpointShutdown::ready(Ok(()))
    }
}
impl RunningAudioOutput for Running {
    fn resume(&mut self) -> Result<(), AudioOutputError> {
        Ok(())
    }
    fn suspend(&mut self) -> Result<(), AudioOutputError> {
        Ok(())
    }
    fn shutdown(self: Box<Self>) -> AudioOutputEndpointShutdown {
        self.probe.stop.store(true, Ordering::Release);
        AudioOutputEndpointShutdown::ready(self.join.join().map_err(|_| {
            AudioOutputError::new(AudioOutputErrorKind::Shutdown, "test callback panicked")
        }))
    }
}

#[tokio::test]
async fn eight_speech_streams_mix_with_cached_earcons_and_cancel_independently() {
    let host = Arc::new(AudioHost::new(
        AudioHostLimits::unlimited()
            .max_streaming_jobs(Some(8))
            .max_decode_jobs(Some(4))
            .max_live_audio_bytes(Some(64 * 1024 * 1024)),
    ));
    let (owner, registrar) = AudioLifecycleOwner::new();
    let mut state = state(Arc::clone(&host), AudioLimits::default(), registrar);
    let probe = Arc::new(Probe::default());
    let context = AudioContext::builder(Arc::new(Factory(Arc::clone(&probe))))
        .options(AudioContextOptions {
            sample_rate: Some(48_000.0),
            ..Default::default()
        })
        .build()
        .unwrap();
    let speech = Fixture::wav(480_000);
    let cue = Fixture::wav(48_000);
    let cached = OfflineAudioContext::new(1, 128, 48_000.0)
        .decode_audio_data_sync(File::open(&cue.0).unwrap())
        .unwrap();
    let mut streams = Vec::new();
    let mut gains = Vec::new();
    for _ in 0..8 {
        let stream = AudioFileStream::open(&mut state, &context, speech.opener()).unwrap();
        let gain = context.create_gain();
        gain.gain().set_value(0.125);
        stream.node().connect(&gain);
        gain.connect(&context.destination());
        streams.push(stream);
        gains.push(gain);
    }
    wait_for(|| {
        streams
            .iter()
            .all(|stream| stream.queued_frames() >= PCM_SOURCE_CAPACITY)
    })
    .await;
    assert_eq!(host.usage().streaming_jobs(), 8);
    assert_eq!(host.usage().decode_jobs(), 0);
    let cue_gain = context.create_gain();
    cue_gain.gain().set_value(0.5);
    cue_gain.connect(&context.destination());
    let start = context.current_time() + 0.025;
    let mut voices = Vec::new();
    for _ in 0..3 {
        let mut source = context.create_buffer_source();
        source.set_buffer(cached.clone());
        source.connect(&cue_gain);
        source.start_at(start);
        voices.push(source);
    }
    for stream in &streams {
        stream.node().start();
    }
    // Eight speech streams contribute 0.25, and three cached voices contribute 0.375.
    wait_for(|| probe.full_mix.load(Ordering::Acquire)).await;
    streams[0].stop();
    assert_eq!(
        completed(&mut streams[0]).await.unwrap(),
        AudioFileStreamOutcome::Cancelled
    );
    probe.after_cancel.store(true, Ordering::Release);
    wait_for(|| probe.cancelled_mix.load(Ordering::Acquire)).await;
    assert_eq!(host.usage().streaming_jobs(), 7);
    // A replacement speech stream is admitted without interrupting the cached voices.
    let mut replacement = AudioFileStream::open(&mut state, &context, speech.opener()).unwrap();
    replacement.stop();
    completed(&mut replacement).await.unwrap();
    for stream in &streams[1..] {
        stream.stop();
    }
    for stream in &mut streams[1..] {
        completed(stream).await.unwrap();
    }
    assert_eq!(host.usage().streaming_jobs(), 0);
    drop(replacement);
    drop(streams);
    drop(voices);
    drop(gains);
    owner.retire().await.unwrap();
    context.close_sync();
}
