# Manual 04: Sub-200ms Duplex Voice, Real-Time Audio DSP, and 2026 Speech Stack

> **Crate:** `eon-voice` (Level 4+)  
> **Companion Specs:** `VOICE_PIPELINE_SPECIFICATION.md`, `ADR-0002-SUB-200MS-DUPLEX-VOICE-PIPELINE.md`

---

## 1. Crate Module Tree

```text
engine/core-rs/crates/eon-voice/src/
├── lib.rs
├── driver/
│   ├── mod.rs              # AudioDriver trait + CaptureBufferModel overrun accounting
│   ├── aaudio_ndk.rs       # Android C/NDK AAudio exclusive low-latency callback
│   └── cpal_host.rs        # Desktop WASAPI / CoreAudio / PipeWire-ALSA driver
├── dsp/
│   ├── ring.rs             # Wait-free rtrb SPSC rings (zero alloc/lock in RT callback)
│   ├── resampler.rs        # rubato SIMD resampler (48k <-> 24k <-> 16k)
│   ├── aec.rs              # Delay-aligned 10ms (160-sample) AEC3 reverse reference loop
│   └── gain_limiter.rs     # ASR-only peak-preserving PcmGainController (gain=8.0)
├── perception/
│   ├── vad_silero.rs       # Silero VAD v5 ONNX ([1, 512] 32ms window + [2, 1, 128] state)
│   ├── endpoint.rs         # Two-Tier Semantic-Prosodic Endpointer + 300ms TTS holdoff
│   ├── speaker_id.rs       # WeSpeaker ResNet34 int8 256-dim cosine verifier
│   └── asr_streaming.rs    # Zipformer 2023-06-26 + Nemotron-0.6B + PreRollRing
└── synthesis/
    ├── verbalizer.rs       # Dual-Channel Technical Verbalizer (spoken vs HUD text)
    ├── g2p.rs              # Pure-Rust misaki lexicon + IPA fallback -> kokoro_vocab IDs
    ├── bridge_cache.rs     # Boot-synthesized PCM bank for the 5 Ciel canonical prefixes
    ├── commitment_gate.rs  # LeaseAcquired barrier preventing speculative contradiction
    ├── chunker.rs          # Prosody-safe ClauseChunker
    └── tts_engine.rs       # Kokoro-82M v1.0 ONNX + optional 80ms frame-streaming backend
```

---

## 2. Hard Real-Time Audio Laws and Sample-Rate Geometry

### 2.1 Zero-Allocation Callback Invariant
Audio glitches, underruns, and dropped barge-ins happen when developers put mutexes, allocations, or ONNX inference inside the OS audio callback. Enforce strict thread separation:

```text
[Hardware Mic Callback (RT Priority)]
   │  Writes raw frames into rtrb::Producer<f32> (wait-free atomic index store, < 200ns)
   ▼
[DSP & Perception Thread (Pinned Core, SCHED_FIFO / MMCSS "Pro Audio")]
   ├─► Resample hw_rate (48kHz) -> 16kHz in exact 10ms (160-sample) blocks
   ├─► Drain aligned 10ms render reference from TTS Loopback Ring -> AEC3::process_capture_frame
   ├─► Accumulate 512 clean samples (32ms) -> Silero VAD v5 (< 0.5ms)
   ├─► On speech active:
   │     ├─► First 120ms (1,920 samples) -> WeSpeaker Biometric Check (< 1.8ms)
   │     ├─► Verified owner while TTS active -> store BARGE_IN_ATOMIC = true (< 5ms mute!)
   │     └─► Feed PcmGainController -> Zipformer / Nemotron ASR stream
   ▼
[Hardware Speaker Callback (RT Priority)]
   └─► Checks BARGE_IN_ATOMIC (Ordering::Acquire):
         - if true: zero output buffer, flush rtrb::Consumer<f32> in O(1), emit BargeInAck
         - if false: pop 24kHz->48kHz resampled TTS samples + push copy to AEC Loopback Ring
```

### 2.2 Fixing the Frame-Size Mismatch Across AEC, VAD, and ASR
The baseline spec mentions "30ms (480 samples)" for Silero VAD v5. **Warning**: Official Silero VAD v5 (`silero_vad.onnx`) at `16,000 Hz` strictly requires **512 samples (`32 ms`)** per step (prepend `64` context samples internally or pass `[1, 512]` with recurrent state tensor `state: [2, 1, 128]` `f32`), whereas WebRTC AEC3 strictly requires **160 samples (`10 ms`)** per step, and Zipformer operates best on **320-sample (`20 ms`)** pushes!

Bridge these deterministically using a zero-allocation `FixedFrameAccumulator<const N: usize>` on the DSP thread:
1. Hardware input -> `rubato::FftFixedInOut` -> **160-sample (10 ms)** blocks into **AEC3**.
2. AEC3 clean 160-sample output -> splits into two accumulators:
   - `vad_acc: FixedFrameAccumulator<512>` -> triggers **Silero VAD v5** every `32 ms` (every 3.2 AEC blocks).
   - `asr_acc: FixedFrameAccumulator<320>` -> pushes `20 ms` frames into `PreRollRing` / active **ASR** stream.

### 2.3 Delay-Aligned AEC3 Loopback (`src/dsp/aec.rs`)
Acoustic Echo Cancellation fails completely on mobile speakers if the reverse reference signal leads or lags the microphone capture by an unknown, drifting buffer offset.
- Measure instantaneous output hardware delay $D_{\text{out}}$ (`AAudioStream_getTimestamp` frame position delta on Android; `IAudioClock::GetPosition` on WASAPI) plus input hardware delay $D_{\text{in}}$.
- Call `aec3.set_stream_delay_ms((D_out + D_in) as i32)` every 50 frames (`500 ms`), and enable `EchoCanceller3Config::delay.use_external_delay_estimator = true` as backup when Bluetooth A2DP/LE Audio introduces dynamic codec latency (`40 to 180 ms`).
- **Headset Optimization**: Query audio routing (`AAudioStream_getDeviceId` / `AudioDeviceInfo.TYPE_WIRED_HEADSET` / `TYPE_BLE_HEADSET`). When closed-back earphones are active and mic bleed is `< -45 dBFS`, bypass heavy nonlinear AEC suppression so whispered user speech in `SILENT_COPILOT` mode is never clipped!

---

## 3. 2026 Streaming ASR Selection and Onset Protection (`src/perception/`)

### 3.1 Model Selection: Eliminating the 20M Zipformer Onset Bug
As documented in the Master Blueprint (Section 3.2), **do not use `sherpa-onnx-streaming-zipformer-en-20M-2023-02-17`**. That first-generation 20M checkpoint loses the opening word on ~89% of real-time VAD-triggered sessions unless fed `>1.5 seconds` of silence first.

Use this two-tier 2026 streaming ASR lineup:

| Tier | Model Artifact | Size & License | Streaming Chunk | Benchmark WER & First-Word Survival |
|---|---|---|---|---|
| **Mobile & Universal Default** | `sherpa-onnx-streaming-zipformer-en-2023-06-26` (`encoder-epoch-99-avg-1-chunk-16-left-128.int8.onnx` + `decoder.onnx` + `joiner.int8.onnx`) | `72.5 MB` total, **Apache-2.0** | `320 ms` (`chunk=16`) | `~5.7% to 7.1%` WER, **>85% to 100%** first-word survival with zero warm-up silence. |
| **Workstation High-Accuracy Streaming** | `NVIDIA Nemotron-Speech-Streaming-EN-0.6B` (or 40-locale `Nemotron-3.5-ASR` int8 ONNX) | `~632 MB` int8, **OpenMDW** | `160 ms` to `560 ms` cache-aware FastConformer | `6.93%` multi-dataset Open ASR average (`2.32%` LibriSpeech clean). |
| **Optional Workstation Utterance Rescorer** | `NVIDIA Parakeet-TDT-0.6B-v3` (25 languages) | `~640 MB` int8, **CC-BY-4.0** | Full-utterance TDT pass at end-of-speech | `~5.9%` Open ASR average at `>1,000x` real-time on GPU (`< 4ms` rescore!). |

### 3.2 Peak-Limiting ASR Gain Controller (`src/dsp/gain_limiter.rs`)
Phone microphones at arm's length frequently deliver speech peaks around `-30 to -38 dBFS`, causing raw `pcm / 32768.0` float inputs to collapse transducer beam search (`>60%` WER). Conversely, multiplying loud near-field speech (`-6 dBFS`) by a naive `8.0x` clips samples into square waves.
- Apply `PcmGainController { requested_gain: 8.0, target_peak: 0.95, release_alpha: 0.08 }` **exclusively on the float copy entering ASR** (never on AEC or VAD!):
  ```rust
  pub fn apply_frame(&mut self, pcm_i16: &[i16], out_f32: &mut [f32]) {
      let peak = pcm_i16.iter().map(|&s| (s as i32).unsigned_abs()).max().unwrap_or(0);
      let safe_gain = if peak == 0 {
          self.requested_gain
      } else {
          self.requested_gain.min((self.target_peak * 32768.0) / (peak as f32))
      };
      // Instantaneous attack to prevent clipping; smooth exponential release to prevent pumping
      self.effective_gain = if safe_gain < self.effective_gain {
          safe_gain
      } else {
          self.effective_gain + (safe_gain - self.effective_gain) * self.release_alpha
      };
      let scale = self.effective_gain / 32768.0;
      for (src, dst) in pcm_i16.iter().zip(out_f32.iter_mut()) {
          *dst = ((*src as f32) * scale).clamp(-self.target_peak, self.target_peak);
      }
  }
  ```

### 3.3 Two-Tier Semantic-Prosodic Endpointing & Post-Reply Holdoff (`src/perception/endpoint.rs`)

Natural multi-clause technical directives (*"Audit the mesh transport crate, and check if replay protection holds under packet loss"*) routinely contain `400 to 700 ms` clause pauses. Implement a **3-tier dynamic silence threshold** driven jointly by Silero VAD probability $p_{\text{speech}}$, RMS energy slope, and the live Zipformer partial transcript $T_{\text{partial}}$:

| Tier | Required Silence ($p_{\text{speech}} < 0.35$) | Activation Condition | Purpose |
|---|---|---|---|
| **Fast Reflex (`300 ms`)** | `300 ms` | $T_{\text{partial}}$ matches a complete Tier 0 command grammar (e.g., `"stop"`, `"affirmative"`, `"run tests"`, `"quiet mode"`), AND does **not** end with an open continuation token. | Shorter confirmation delay for complete commands. With 32 ms VAD frames, this tier needs 320 ms of classified silence. |
| **Standard Clause (`800 ms`)** | `800 ms` | Default for all general spoken queries and engineering instructions. | Cuts premature mid-sentence splits from `~36%` down to `< 3%`. |
| **Continuation Hold (`1,150 ms`)** | `1,150 ms` | $T_{\text{partial}}$ ends with a conjunction, preposition, or dangling operator (`"and"`, `"or"`, `"but"`, `"in"`, `"to"`, `"for"`, `"with"`, `"then"`, `"because"`, `"if"`). | Waits while the operator recalls a filename or symbol name. |

The driven rig responder waits for this endpoint and a nonempty final transcript before it queues a cached prefix. Faster synthesis alone therefore cannot meet a 170 ms acoustic target on turns where VAD remains active through the end of speech. Endpoint time is not the start of the TTFA clock: use the last active input and first sustained speaker output on the same recording (Manual 06 §3.3).

**Post-Reply Session Split (`300 ms` Holdoff)**: When TTS transitions `SPEAKING -> RETURNING_TO_DORMANT`, hold VAD invocation closed for `300 ms` while physical room reverb decays, then force a **session ID split** in the VAD state machine so an operator speaking `350 ms` after Ciel finishes is immediately accepted as a fresh turn rather than suppressed as trailing TTS echo!

---

## 4. Neural TTS, G2P, Dual-Channel Verbalizer, and Commitment Gate (`src/synthesis/`)

### 4.1 Honest Kokoro-82M v1.0 Engineering + True Frame-Streaming Option
In 2026 CPU TTS benchmarks:
- **`Kokoro-82M v1.0` (`onnx-community/Kokoro-82M-v1.0-ONNX`, Apache-2.0)** achieves the highest speech naturalness in its weight class (`UTMOS 4.46`, `24 kHz`), with disk size `326 MB` (`fp32`) or `~165 MB` (`int8`/`fp16`) plus `28 MB` `voices.bin`. However, its StyleTTS2+ISTFTNet ONNX graph synthesizes **one complete text clause per `session.run()` call**, NOT 20ms incremental frames!
- **How EON Achieves `< 170ms` TTFA with Kokoro**:
  1. **Boot-Cached Prefix Bank (`bridge_cache.rs`)**: At startup, synthesize and store in pinned RAM the `24 kHz` PCM waveforms (with `15 ms` raised-cosine fade-out tails) for Ciel's five canonical openers (`"Affirmative."`, `"Report:"`, `"Notice:"`, `"Negative."`, `"Proposal:"`). Total RAM: `< 450 KB`! As soon as Tier 0 or the Acoustic Commitment Gate selects the prefix, playback starts in **`< 8 ms`** while the worker thread runs Kokoro ONNX on Clause 1 (`4 to 10` words, taking `35 to 90 ms` on 2-4 CPU/NPU threads), seamlessly stitching Clause 1 into the `rtrb` output ring before the prefix finishes playing!
  2. **Optional True 80ms Frame-Streaming Backend**: Expose `TtsBackend::PocketTts` (Kyutai `pocket-tts` ~100M, MIT, `236 MB`) or `Linnet` (`83 MB` Rust codec-LM) behind the same `Synthesizer` trait for single-core low-power edge targets where `80 ms` autoregressive frame emission is preferred over clause batching.

### 4.2 Pure-Rust G2P and `kokoro_vocab.json` Mapping (`src/synthesis/g2p.rs`)
Kokoro v1.0 expects `input_ids: i64[1, L]` (`L <= 510`), `style: f32[1, 256]` (indexed from `voices.bin` by `[voice_idx][L]`, because Kokoro's style vector is **token-length indexed**!), and `speed: f32[1]`:
1. Normalize text via `DualChannelVerbalizer`.
2. Convert words to IPA using an embedded pure-Rust `misaki`-compatible US-English gold/silver pronunciation FST (`fst` crate, `< 3.2 MB` memory-mapped) + morphological suffix stripper (`-s`, `-ed`, `-ing`) + Compact Transformer/G2P neural fallback (`< 2 MB` ONNX) for out-of-vocabulary identifiers and camelCase split words (`TreeSitter` -> `"tree sitter"`).
3. Map IPA codepoints through `kokoro_vocab.json` (`178` symbols) and **wrap with pad token `0` at index `0` and index `L - 1`** (`[0, tok_1, ..., tok_n, 0]`). Omitting the leading and trailing `0` pad tokens or failing to index the `256`-dim style row by phoneme length `L` causes clipped initial consonants and prosody distortion in Kokoro ONNX!

### 4.3 Dual-Channel Technical Verbalizer (`src/synthesis/verbalizer.rs`)
Before any token stream enters `ClauseChunker`, `verbalizer.rs` branches the stream into `(visual_markdown, spoken_prose)`:
- Code blocks (```` ```...``` ````) are replaced in `spoken_prose` with `"The implementation diff is displayed on your canvas."` while the full code block streams untouched to HUD/CLI/Canvas.
- File paths (`a/b/c/foo_bar.rs:142`) rewrite to `"foo bar dot R S, line 142"`.
- Hex digests $\ge 12$ chars (`5022b2eca5b19d1b...`) rewrite to `"digest five zero two two"`.
- Semver (`v1.9.1`) -> `"version one point nine point one"`; CWE IDs (`CWE-416`) -> `"C W E four sixteen"`; Rust/C types (`u64`, `&mut str`, `usize`) -> `"unsigned 64-bit int"`, `"mutable string slice"`, `"pointer-sized uint"`.

### 4.4 Acoustic Commitment Gate (`src/synthesis/commitment_gate.rs`)
```rust
pub enum CommitmentState {
    BridgePermittedOnly,             // May speak "Affirmative." / "Analyzing..." only
    Committed(LeaseId, u64),         // Kernel confirmed LeaseAcquired for epoch; full report unlocked
    Aborted(String),                 // Lease failed or Epoch invalidated; pivot to "Notice:" / "Negative."
}
```
Any spoken clause containing past-tense or state-mutating action verbs (*"patched"*, *"deleted"*, *"deployed"*, *"updated"*, *"executing"*) is held in `ClauseChunker` until `CommitmentState::Committed` arrives from `eon-kernel` (typically `< 5 ms` for local resource leases), completely eliminating speculative verbal contradictions.

---

## 5. Verification Test Suite Requirements (`eon-voice`)

1. **`rt_callback_allocation_test`**: Wrap the global allocator with `assert_no_alloc` during 10,000 simulated audio input/output callback invocations (including active barge-in flushes); assert **zero** heap allocations and **zero** mutex acquisitions.
2. **`asr_onset_and_endpoint_gate`**: Replay the 16-utterance benchmark suite (LibriSpeech + negated commands like *"I was talking about turning off the flashlight but do not actually do it"*) with `350 ms` VAD session-start delay and `40 dB` / `15 dB` room noise; assert WER `<= 9.0%`, first-word survival `>= 80%`, and `0%` premature clause-pause truncation on multi-clause prompts.
3. **`aec_barge_in_loopback_test`**: Mix synthetic `24 kHz` Kokoro TTS output (`-12 dBFS`) into simulated mic input (`-18 dBFS` echo with `45 ms` delay) while injecting owner speech (`WeSpeaker cosine = 0.86`) at `t = 1.200 s`; assert **zero** false self-barge-ins during `[0, 1.200s]` and deterministic `BARGE_IN_ATOMIC` trigger within `< 95 ms` of owner voice onset (`< 5 ms` buffer flush after detection).
4. **`kokoro_phoneme_and_verbalizer_test`**: Verify `[0, ..., 0]` token padding, length-indexed `style[L]` slice selection, and Dual-Channel Verbalizer rewrites across paths, SHA-256 hashes, fenced code blocks, and Rust generics.
