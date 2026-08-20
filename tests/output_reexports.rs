use deno_audio::{
    AudioContextLatencyCategory, AudioContextRenderSizeCategory, AudioOutputConfig,
    AudioOutputContextId, AudioOutputEndpointShutdown, AudioOutputError, AudioOutputEventSink,
    AudioOutputFactory, AudioOutputRequest, AudioOutputStartFailure, AudioRenderCallback,
    AudioRenderFormat, PreparedAudioOutput, RunningAudioOutput,
};

struct ExternalFactory;

struct ExternalPrepared {
    config: AudioOutputConfig,
}

struct ExternalRunning {
    callback: Option<AudioRenderCallback>,
}

impl AudioOutputFactory for ExternalFactory {
    fn prepare(
        &self,
        request: &AudioOutputRequest,
    ) -> Result<Box<dyn PreparedAudioOutput>, AudioOutputError> {
        let format = AudioRenderFormat::new(
            request.requested_sample_rate().unwrap_or(48_000.0),
            request.number_of_channels(),
            128,
        )?;
        Ok(Box::new(ExternalPrepared {
            config: AudioOutputConfig::new(format, request.sink_id(), 0.0)?,
        }))
    }
}

impl PreparedAudioOutput for ExternalPrepared {
    fn config(&self) -> &AudioOutputConfig {
        &self.config
    }

    fn start(
        self: Box<Self>,
        callback: AudioRenderCallback,
        _events: AudioOutputEventSink,
    ) -> Result<Box<dyn RunningAudioOutput>, AudioOutputStartFailure> {
        Ok(Box::new(ExternalRunning {
            callback: Some(callback),
        }))
    }

    fn abort(self: Box<Self>) -> AudioOutputEndpointShutdown {
        AudioOutputEndpointShutdown::ready(Ok(()))
    }
}

impl RunningAudioOutput for ExternalRunning {
    fn resume(&mut self) -> Result<(), AudioOutputError> {
        Ok(())
    }

    fn suspend(&mut self) -> Result<(), AudioOutputError> {
        Ok(())
    }

    fn shutdown(mut self: Box<Self>) -> AudioOutputEndpointShutdown {
        drop(self.callback.take());
        AudioOutputEndpointShutdown::ready(Ok(()))
    }
}

#[test]
fn external_factory_can_use_only_deno_audio_contract_names() {
    fn assert_factory<T: AudioOutputFactory>() {}
    fn assert_request_getters(
        _context_id: fn(&AudioOutputRequest) -> AudioOutputContextId,
        _latency: fn(&AudioOutputRequest) -> AudioContextLatencyCategory,
        _render_size: fn(&AudioOutputRequest) -> AudioContextRenderSizeCategory,
    ) {
    }
    assert_factory::<ExternalFactory>();
    assert_request_getters(
        AudioOutputRequest::context_id,
        AudioOutputRequest::latency_hint,
        AudioOutputRequest::render_size_hint,
    );

    let prepared: Box<dyn PreparedAudioOutput> = Box::new(ExternalPrepared {
        config: AudioOutputConfig::new(
            AudioRenderFormat::new(48_000.0, 2, 128).expect("valid logical format"),
            "logical-mixer",
            0.0,
        )
        .expect("valid output config"),
    });
    assert_eq!(prepared.config().accepted_sink_id(), "logical-mixer");
    let _shutdown = prepared.abort();
}
