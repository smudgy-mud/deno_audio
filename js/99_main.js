// Copyright 2026 the deno_audio authors. MIT license.

import { installGlobal } from "ext:deno_audio/00_webidl.js";
import { AudioBuffer } from "ext:deno_audio/01_audio_buffer.js";
import {
  AudioContext,
  AudioDestinationNode,
  AudioNode,
  AudioParam,
  AudioScheduledSourceNode,
  BaseAudioContext,
  GainNode,
  OfflineAudioCompletionEvent,
  OfflineAudioContext,
  OscillatorNode,
} from "ext:deno_audio/02_offline_audio.js";

installGlobal("AudioBuffer", AudioBuffer);
installGlobal("AudioContext", AudioContext);
installGlobal("AudioDestinationNode", AudioDestinationNode);
installGlobal("AudioNode", AudioNode);
installGlobal("AudioParam", AudioParam);
installGlobal("AudioScheduledSourceNode", AudioScheduledSourceNode);
installGlobal("BaseAudioContext", BaseAudioContext);
installGlobal("GainNode", GainNode);
installGlobal("OfflineAudioCompletionEvent", OfflineAudioCompletionEvent);
installGlobal("OfflineAudioContext", OfflineAudioContext);
installGlobal("OscillatorNode", OscillatorNode);
