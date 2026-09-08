// Copyright 2026 the deno_audio authors. MIT license.

import { installGlobal } from "ext:deno_audio/00_webidl.js";
import { AudioBuffer } from "ext:deno_audio/01_audio_buffer.js";
import {
  AudioBufferSourceNode,
  AudioContext,
  ConstantSourceNode,
  AudioDestinationNode,
  AudioNode,
  AudioParam,
  AudioScheduledSourceNode,
  BaseAudioContext,
  GainNode,
  OfflineAudioCompletionEvent,
  OfflineAudioContext,
  OscillatorNode,
  PeriodicWave,
} from "ext:deno_audio/02_offline_audio.js";

installGlobal("AudioBuffer", AudioBuffer);
installGlobal("AudioBufferSourceNode", AudioBufferSourceNode);
installGlobal("AudioContext", AudioContext);
installGlobal("ConstantSourceNode", ConstantSourceNode);
installGlobal("AudioDestinationNode", AudioDestinationNode);
installGlobal("AudioNode", AudioNode);
installGlobal("AudioParam", AudioParam);
installGlobal("AudioScheduledSourceNode", AudioScheduledSourceNode);
installGlobal("BaseAudioContext", BaseAudioContext);
installGlobal("GainNode", GainNode);
installGlobal("OfflineAudioCompletionEvent", OfflineAudioCompletionEvent);
installGlobal("OfflineAudioContext", OfflineAudioContext);
installGlobal("OscillatorNode", OscillatorNode);
installGlobal("PeriodicWave", PeriodicWave);
