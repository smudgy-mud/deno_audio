// Copyright 2026 the deno_audio authors. MIT license.

import { installGlobal } from "ext:deno_audio/00_webidl.js";
import { AudioBuffer } from "ext:deno_audio/01_audio_buffer.js";

installGlobal("AudioBuffer", AudioBuffer);
