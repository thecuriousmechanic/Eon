# EON Master Engineering and Implementation Blueprint (2026 Edition)

> **Status:** Authoritative Engineering Implementation Reference  
> **Reviewed Commit:** `6700068`  
> **Scope:** Full-stack implementation specification bridging EON's architectural vision with verified 2026 Rust crates, open-weight AI models, autonomous cyber reasoning systems, and multi-platform OS substrates.

---

## 1. Purpose of This Implementation Suite

The high-level specifications in `docs/*.md` define **what** EON is: a local-first, distributed cognitive operating system, autonomous software engineer, and cyber reasoning engine modeled on *Manas: Ciel*. However, inspecting the repository at commit `6700068` reveals two major gaps between the conceptual design and a shippable system:

1. **Codebase State**: Only `eon-primitives` (~260 lines) and a struct stub in `eon-vision` (~59 lines) contain code. Eight of the ten workspace crates in `engine/core-rs/crates/` are 3-line stubs, and `clients/` (`android`, `cli`, `canvas`, `daemon`) has not yet been scaffolded.
2. **Ecosystem and Reality Drift**: Several model selections, crate names, wire APIs, and hardware assumptions in the baseline docs predate major 2025 to 2026 shifts in open-weight AI, speech synthesis, Rust networking, and agentic security, or contain concrete defects that would break compilation or runtime reliability if implemented verbatim.

This blueprint and the six companion manuals under `docs/implementation/` provide the exact module trees, Cargo manifests, Rust traits, binary layouts, algorithms, security proofs, cross-compilation recipes, and verification gates required to build EON end-to-end without guesswork.

---

## 2. Implementation Manual Index

| Document | Crates and Subsystems Covered | Primary Engineering Focus |
|---|---|---|
| [01: Kernel, Foundation, and Capability Security](./implementation/01_KERNEL_FOUNDATION_AND_CAPABILITY_SECURITY.md) | `eon-primitives`, `eon-foundation`, `eon-kernel`, `eon-capability` | CSPRNG ULID fix, `0x454F4E4A` group-commit journal, SQLite WAL + `sqlite-vec`, OS enclave `CredentialBroker`, deterministic DAG + dynamic splicing, CaMeL/FIDES/Rule-of-Two/MELON prompt-injection defense, MCP `2026-07-28` tool pinning. |
| [02: AI Cognition, Models, and Inference (2026)](./implementation/02_AI_COGNITION_MODELS_AND_INFERENCE_2026.md) | `eon-cognition`, `eon-vision`, `perception/models` | Gemma 4 (`E2B`, `E4B`, `12B Unified`, `26B-A4B`) and Qwen 3.5 / 3.6 (`0.8B` to `27B` / `35B-A3B`) profiles, `llama-cpp-2` + `mistralrs` dual backend, `llguidance` `1.9` cancellable token masks, Z3 neuro-symbolic bridge, hierarchical ZoomIn + OmniParser-v2 + Set-of-Mark GUI grounding, `madvise` Memory Governor. |
| [03: Cybersecurity, Program Analysis, and Sandboxing](./implementation/03_CYBERSECURITY_PROGRAM_ANALYSIS_AND_SANDBOXING.md) | `eon-cyber`, `eon-capability` (sandboxes) | DARPA AIxCC CRS pipeline (ATLANTIS / Buttercup / Big Sleep patterns), `petgraph` Code Property Graphs (AST+CFG+PDG), OSV/CycloneDX/SLSA supply chain audit, `libpcode` / `libsla` / `icicle-vm` P-Code lifting and emulation, `libafl` `0.16` + Z3 concolic fuzzing, production `aya` CO-RE eBPF + BoringSSL uprobes, Landlock + seccomp + Firecracker + Wasmtime 4-tier sandboxing. |
| [04: Duplex Voice, Audio, and Real-Time DSP](./implementation/04_DUPLEX_VOICE_AUDIO_AND_REALTIME_DSP.md) | `eon-voice` | Replacing the defective 20M Zipformer with `zipformer-en-2023-06-26` and `Nemotron-Speech-Streaming-0.6B` / `Parakeet-TDT-0.6B-v3`, Kokoro-82M v1.0 clause synthesis vs `Pocket-TTS` / `Linnet` 80ms frame streaming, lock-free SPSC rings, `rubato` resampling, delay-aligned WebRTC AEC3, WeSpeaker biometric gate, two-tier endpointing, `< 5ms` barge-in. |
| [05: Distributed Mesh, CRDT, and Clients](./implementation/05_DISTRIBUTED_MESH_CRDT_AND_CLIENTS.md) | `eon-mesh`, `eon-uniffi`, `clients/*`, `proto/eon.proto` | Post-1.0 `iroh` `1.4` (`EndpointId`, `Router`, ALPNs, `noq` QUIC datagrams and stream priorities), clock-skew-immune 15ms Spatial Arbitration, Loro CRDT containers + SQLite projector, Android 14/15/16 `ROLE_ASSISTANT` + zero-copy `AAudio` and `AHardwareBuffer`, desktop daemon, `ratatui` CLI, `wgpu` CFG/AST canvas. |
| [06: Verification, Benchmarks, and Phased Playbook](./implementation/06_VERIFICATION_BENCHMARKS_AND_PHASED_PLAYBOOK.md) | `scripts/`, `tests/`, `.github/workflows/` | Verified SHA-256 model fetcher, cross-compilation CI matrix, torn-write crash harness, adaptive prompt-injection red-team suite, ASR onset/endpoint gate, Cyber CRS CWE benchmark suite, step-by-step Phase 1 to 10 execution order. |

---

## 3. Critical Audit of Baseline Repository Defects and Spec Corrections

Implementing agents MUST apply the following corrections over earlier conceptual docs. Each finding was verified against the current repository code or 2025 to 2026 upstream releases.

### 3.1 Defect 1: Broken ULID Entropy and Validation in `eon-primitives/src/id.rs`
- **Current Bug**: `generate_ulid_string()` in `engine/core-rs/crates/eon-primitives/src/id.rs` initializes `let mut entropy = count;` where `count` is a `u64` starting at `0`, then loops 16 times shifting `entropy >>= 5` (80 bits total). Shifting a `u64` 16 times leaves the upper 16 bits permanently zero, makes IDs predictable across restarts (colliding across distributed mesh nodes!), and `from_raw()` only checks `starts_with("<prefix>_")` without checking the 26-character length or Crockford Base32 alphabet.
- **Required Fix**: Maintain a `u128` ULID core (48-bit Unix millisecond timestamp + 80-bit random payload seeded per-millisecond from OS CSPRNG via `getrandom`, incremented monotonically within the same millisecond with overflow guard). Enforce exact 26-char Crockford Base32 validation in `from_raw()` and implement `Serialize` / `Deserialize`. Full code is in Manual 01, Section 2.

### 3.2 Defect 2: Placeholder SHA-256 Hashes and Outdated Models in `scripts/download_models.py`
- **Current Bug**: `scripts/download_models.py` hardcodes sequential dummy hashes (`"a1b2c3d4e5f60718293a4b5c6d7e8f90..."`, `"e3b0c442..."` which is the SHA-256 of an empty 0-byte file!) and points to `sherpa-onnx-streaming-zipformer-en-20M-2023-02-17`.Running `scripts/download_models.py` immediately raises `ValueError: INVARIANT 2 VIOLATION` on every file except an empty file.
- **Required Fix**:
  1. **Never ship `sherpa-onnx-streaming-zipformer-en-20M-2023-02-17`**. Empirical streaming frame-by-frame evaluation proves that the 2023-02-17 20M first-generation Zipformer drops the opening word of ~89% of utterances under realistic VAD session-start pre-roll (~23.5% WER), turning *"I was talking about turning off the flashlight..."* into *"turning off the flashlight..."*. Replace it on edge/mobile with `sherpa-onnx-streaming-zipformer-en-2023-06-26` (int8 encoder + fp32 decoder + int8 joiner, ~72.5 MB, 5.7% to 7.1% WER, >85% first-word survival with zero synthetic silence warm-up) and on workstation with `Nemotron-Speech-Streaming-EN-0.6B` / `Parakeet-TDT-0.6B-v3`.
  2. **Upgrade SLMs/VLMs to 2026 Apache-2.0 SOTA**: Replace `gemma-2-2b-it` (custom Gemma Terms, text-only) and `Qwen2.5-Coder-3B` with **Google Gemma 4** (`E2B`, `E4B`, `12B Unified`, `26B-A4B`, all Apache-2.0, native multimodal text+image+audio with Per-Layer Embeddings and official QAT checkpoints) and **Alibaba Qwen 3.5 / Qwen 3.6** (`Qwen3.5-4B`, `Qwen3.5-9B`, `Qwen3.6-27B` scoring 77.2% SWE-bench Verified, and `Qwen3.6-35B-A3B` MoE, all Apache-2.0).
  3. **Correct Kokoro TTS Sizing and Streaming Mechanics**: `Kokoro-82M v1.0` ONNX (`onnx-community/Kokoro-82M-v1.0-ONNX`, Apache-2.0) is ~326 MB in FP32 (~165 MB in INT8/FP16) plus ~28 MB `voices.bin`, and its ONNX graph synthesizes **one complete clause/utterance per call**, not 20ms autoregressive frames. Sub-170ms TTFA with Kokoro therefore strictly depends on short acoustic reflex clauses (`"Affirmative."`) or pairing with true 80ms frame-streaming models (`Pocket-TTS` 100M MIT or `Linnet` 41M Rust runtime).
  4. Move hashes into a signed/versioned `perception/models/manifest.lock.json` generated and verified against Hugging Face LFS `oid sha256` headers. Full spec is in Manual 02 and Manual 06.

### 3.3 Defect 3: Fictional or Renamed Rust Crates and Cross-Compilation Traps
- **`sleigh-rs` vs Real P-Code Lifters**: The baseline docs name `sleigh-rs` as the Ghidra P-Code lifter. In the real 2026 Rust ecosystem, there are three distinct maintained options with very different FFI properties:
  - **`libpcode` (`0.1.8`)**: Pure-Rust Ghidra `.sla` P-Code disassembler and lifter (`object` + `nom` + `roxmltree`, zero C++ compiler needed, compiles cleanly to `aarch64-linux-android`!).
  - **`icicle-vm` (`icicle-emu`)**: Pure-Rust multi-architecture P-Code lifter, interpreter, and Cranelift JIT emulator specifically built for high-speed fuzzing and instrumentation.
  - **`libsla` (`1.2.0` + `sleigh-config`)**: Rust 2024 FFI bindings to NSA Ghidra's C++ `libsla` decompiler/sleigh engine (best reserved for `workstation-full` builds).
- **`iroh` Pre-1.0 vs `iroh 1.4.0` (Oct 2026)**: `iroh` stabilized 1.0 in mid-2026 on top of `noq` 1.0. `NodeId` is deprecated in favor of `EndpointId` (`X-Iroh-Endpoint-Id`); `MdnsAddressLookup` moved out of `iroh` into `iroh-mdns-address-lookup`, and DHT lookup moved into `iroh-mainline-address-lookup`.
- **Android NDK (`cargo-ndk`) C++ Linker Explosion**: Linking `z3-sys` (huge C++ SMT solver), `libafl_qemu`, `libsla`, and desktop `clangd`/`codelldb` into `aarch64-linux-android` inside one unconditional dependency tree will break Android builds and bloat `libeon_uniffi.so` past 150 MB. EON MUST split Cargo features into `profile-mobile-edge` and `profile-workstation` while keeping crate trait boundaries identical (with transparent `eon-mesh` RPC delegation when an Android edge node invokes `cyber.offense.fuzz_concolic` or heavy `reasoning.smt_solve` on a paired workstation, plus a pure-Rust SAT/BV micro-solver or bounded `z3` static cross-build when explicitly enabled).

### 3.4 Defect 4: Endpointing Cutoff vs Latency Tradeoff
- **Current Spec Conflict**: `VOICE_PIPELINE_SPECIFICATION.md` sets `end_speech_silence_duration = 250ms` (lowering to `180ms`). Empirical Silero VAD v5 testing on natural human sentences shows a median intra-sentence clause pause of `388ms` (p75 `598ms`, p90 `772ms`). A flat `250ms` cutoff chops >50% of multi-clause engineering and cyber instructions in half and executes the truncated prefix!
- **Required Fix**: Implement **Two-Tier Semantic-Prosodic Endpointing** (Manual 04):
  - **Tier A Fast Endpoint (`300ms`)**: Triggers *only* when the streaming Zipformer partial transcript matches a syntactically complete closed reflex command (e.g., `"Affirmative"`, `"Stop"`, `"Switch to silent mode"`, `"Run cargo test"`) with falling terminal RMS energy.
  - **Tier B Analytical Endpoint (`800ms`, extending to `1100ms` on trailing conjunctions/prepositions like `"and"`, `"while"`, `"in"`, `"then"`)**: Keeps multi-clause instructions intact (<3% premature cutoff rate) while Predictive Pre-Cognition warms the KV-cache concurrently during the pause so perceived response latency after true speech end remains `< 170ms`.

### 3.5 Defect 5: Prompt Injection Defense Must Be Deterministic, Not Prompt-Based
- **2026 Security Reality**: Research across 2025 and 2026 (*EchoLeak CVE-2025-32711*, *MINJA / MemoryGraft*, *MCP Tool Poisoning & Rug Pulls*, and *"The Attacker Moves Second"* showing adaptive attacks bypass >90% of classifier/prompt defenses) proves that system-prompt rules and standalone BERT classifiers cannot secure an autonomous shell/code agent.
- **Required Fix**: Enforce **Defense-in-Depth Information Flow Control (IFC)** outside the LLM in `eon-kernel` and `eon-capability` (Manual 01):
  1. **Meta's Rule of Two Gate**: No single task execution context may hold `[A] Untrusted Input` + `[B] Sensitive Credentials/Filesystem` + `[C] External State Mutation / Network Egress` without deterministic policy clearance or a raw-command human approval gate.
  2. **FIDES Lattice Labels + CaMeL Provenance**: Every DAG variable and memory triple carries `(Integrity, Confidentiality, ProvenanceSet)` tags checked by Rust type-state rules before `TaskDispatcher` issues a lease.
  3. **Memory & MCP Pinning**: Block `UntrustedExternal` turns from writing procedural macros or unquarantined semantic facts (defeating MINJA/MemoryGraft), and pin SHA-256 digests of external MCP tool schemas/descriptions upon registration (defeating MCP rug pulls).

---

## 4. Verified 2026 Workspace Dependency Matrix

Checked on 2026-11-17 against crates.io and project release notes. The `crates/` pins below are what the code uses today. The block after the table is the target layout.

| Component | Used by | Pin in code | Latest verified | Notes |
|---|---|---|---|---|
| wasmtime | `eon-capability` | `50` | 50.0.2 stable (51.0.0-rc.1 is a pre-release) | April 2026 advisories fixed in 36.0.7, 42.0.2, 43.0.1; a July 2026 advisory (CVE-2026-104855) affects 46.0.0 to 46.0.1 and 47.0.0 to 47.0.2, so 50 is past both |
| ort | `eon-voice` | `=2.0.0-rc.15` | 2.0.0-rc.15 | rc.15 needs `ort::init().build()` and `Session::builder(&env)`, both wrapped in `eon-voice::runtime` |
| iroh | `eon-mesh` | `1.4` | 1.4.1 | Relays disabled in the transport; loopback tests only |
| llguidance | not yet used | n/a | 1.9.1 | Grammar masks; llama.cpp builds it with `-DLLAMA_LLGUIDANCE=ON` |
| llama-cpp-2 | `eon-cognition` | `0.1.161` | 0.1.161 | Current |
| landlock | `eon-capability` | `0.4.7` | 0.4.7 | Current |
| seccompiler | `eon-capability` | `0.5.0` | 0.5.0 | Current |
| loro | `eon-mesh` | `1.x` | 1.18.0 | Current |
| tree-sitter | not yet used | n/a | 0.27.2 | Planned for polyglot CPG |
| cpal | not yet used | n/a | 0.18.2 | Planned for the audio device loop |
| rubato | `eon-voice` | `5.1` | 5.1.0 | Windowed-sinc resampling via `Async::process_all` |
| mergiraf | blocked | n/a | 0.20.0 | Pins `termcolor ~1.1` through `stderrlog`, which conflicts with wasmtime's `termcolor ^1.2` |

All crates in `engine/core-rs/Cargo.toml` MUST pin workspace-level dependencies using `[workspace.dependencies]` so versions never drift across the 10 crates.

```toml
[workspace]
resolver = "2"
members = [
    "crates/eon-primitives",
    "crates/eon-foundation",
    "crates/eon-kernel",
    "crates/eon-capability",
    "crates/eon-cyber",
    "crates/eon-mesh",
    "crates/eon-voice",
    "crates/eon-vision",
    "crates/eon-cognition",
    "crates/eon-uniffi",
]

[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.85"
license = "Apache-2.0"

[workspace.dependencies]
# Async & Concurrency
tokio = { version = "1.43", features = ["rt-multi-thread", "macros", "sync", "time", "net", "fs", "process", "io-util", "signal"] }
tokio-util = { version = "0.7", features = ["rt", "codec"] }
async-trait = "0.1"
futures = "0.3"
parking_lot = "0.12"
crossbeam-channel = "0.5"
dashmap = "6.1"
rtrb = "0.3"                        # Wait-free SPSC real-time audio ring buffer

# Serialization, IDs, Crypto & Foundation
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
postcard = { version = "1.1", features = ["use-std"] }
prost = "0.13"
bytes = "1.9"
bitflags = { version = "2.8", features = ["serde"] }
crc32fast = "1.4"                   # SIMD-accelerated CRC32 verification for 0x454F4E4A frames
blake3 = { version = "1.6", features = ["mmap", "rayon"] }
sha2 = "0.10"
zeroize = { version = "1.8", features = ["derive"] }
getrandom = "0.3"
aho-corasick = "1.1"                # Multi-pattern secret prefix scanner in JournalScrubber
thiserror = "2.0"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }

# Storage, Vector Index & CRDT
rusqlite = { version = "0.42", features = ["bundled", "backup", "functions", "vtab"] }
sqlite-vec = "0.1.10-alpha.4"       # Pure-C SIMD vector virtual tables (vec0)
loro = "1.4"                        # Rich-text, Map, List, MovableList & Tree CRDT engine
memmap2 = "0.9"

# Networking & Distributed Mesh (Post-1.0 iroh stack)
iroh = "1.4"                        # Direct P2P QUIC over noq 1.0, EndpointId, relay fallback
iroh-mdns-address-lookup = "0.1"    # Zero-relay LAN discovery
iroh-blobs = "0.105"                # BLAKE3 verified resumable artifact/model streaming
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "stream", "json", "http2"] }
smoltcp = { version = "0.14", default-features = false, features = ["std", "medium-ip", "medium-ethernet", "proto-ipv4", "proto-ipv6", "socket-tcp", "socket-udp", "socket-icmp"] }

# AI Inference, Constrained Decoding & Vision
ort = { version = "2.0.0-rc.15", default-features = false, features = ["std", "ndarray"] }
llama-cpp-2 = { version = "0.1.160", optional = true }
mistralrs = { version = "0.8.2", optional = true }
llguidance = "1.9.1"                # <50us Earley/derivative CFG token mask engine
toktrie = "0.7"
image = { version = "0.25", default-features = false, features = ["jpeg", "png", "webp"] }

# Code Intelligence, Program Analysis, Cyber & Sandboxing
tree-sitter = "0.25"
ast-grep-core = "0.36"
petgraph = "0.7"                    # Code Property Graph (CPG), CFG, and DAG algorithms
ignore = "0.4"                      # Ripgrep gitignore-aware fast directory walker
portable-pty = "0.9"                # Cross-platform stateful PTY multiplexer
lsp-types = "0.97"
object = "0.36"                     # Zero-copy ELF, PE, Mach-O, COFF binary parser
goblin = "0.9"                      # ELF dynamic symbols, relocations, checksec flags
capstone = "0.13"                   # Fast multi-arch disassembly
libpcode = "0.1.8"                  # Pure-Rust Ghidra Sleigh P-Code lifter (mobile+desktop safe)
libsla = { version = "1.2.0", optional = true }    # Workstation Ghidra C++ Sleigh bindings
libafl = { version = "0.16.1", optional = true }   # Workstation coverage-guided concolic fuzzer
libafl_bolts = { version = "0.16.1", optional = true }
z3 = { version = "0.21.1", optional = true }       # SMT-LIB2 / bitvector solver bridge
landlock = "0.4"                    # Linux LSM unprivileged filesystem & TCP sandbox
seccompiler = "0.5"                 # Firecracker's BPF syscall filter compiler
wasmtime = { version = "30.0", optional = true }   # Fuel-metered WASI 0.2 plugin sandbox

# Audio DSP & FFI
cpal = "0.15"
rubato = "0.16"                     # SIMD asynchronous polyphase resampler (48k <-> 24k <-> 16k)
uniffi = "0.29"
```

---

## 5. Strict Layering and Crate Dependency DAG

Lower-numbered crates MUST NEVER import higher-numbered crates. Enforce this in CI with `cargo tree --edges normal` assertions:

```text
Level 0: eon-primitives
           ▲
Level 1: eon-foundation  (depends on: eon-primitives)
           ▲
Level 2/3: eon-kernel    (depends on: eon-primitives, eon-foundation)
           ▲
Level 4: eon-capability  (depends on: eon-primitives, eon-foundation, eon-kernel)
           ▲
Level 4+: Domain Engines (all implement Capability traits or feed Perception events):
          ├── eon-voice   (depends on: eon-primitives, eon-foundation)
          ├── eon-vision  (depends on: eon-primitives, eon-foundation)
          ├── eon-cyber   (depends on: eon-primitives, eon-foundation, eon-capability)
          └── eon-mesh    (depends on: eon-primitives, eon-foundation, eon-kernel, eon-capability)
           ▲
Level 5: eon-cognition   (depends on: eon-primitives, eon-foundation, eon-kernel,
                          eon-capability, eon-vision, eon-mesh)
           ▲
Level 6: Façade & Clients:
          ├── eon-uniffi  (depends on all core crates with profile-mobile-edge features)
          ├── clients/daemon
          ├── clients/cli
          ├── clients/canvas
          └── clients/android
```

---

## 6. Build Profiles: `mobile-edge` vs `workstation-full`

To keep Android NDK cross-compilation reliable while unlocking heavy binary emulation, kernel eBPF, and 27B/35B coding models on Linux and Windows workstations, every crate with platform-heavy dependencies exposes two canonical Cargo feature bundles:

| Feature Bundle | Target Triples | Enabled Subsystems | Excluded / Delegated via `eon-mesh` |
|---|---|---|---|
| `profile-mobile-edge` | `aarch64-linux-android`, `x86_64-linux-android` | `ort` (QNN + XNNPACK + NNAPI), `llama-cpp-2` (Vulkan + ARMv8.2 FP16/DotProd), `sherpa-onnx` NDK, `AAudio` / `Oboe`, `libpcode` pure-Rust lifter, `smoltcp` TUN (`VpnService`), Android Keystore `CredentialBroker`. | `libafl_qemu`, `libsla` C++, `z3-sys` C++ (uses SMT-LIB2 mesh RPC or embedded lightweight bitvector solver), Firecracker KVM. |
| `profile-workstation` | `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`, `aarch64-apple-darwin` | Everything in `mobile-edge` (using CPAL/WASAPI/CoreAudio + OS Keyring/DPAPI) PLUS `mistralrs` (CUDA/Metal/FlashAttn), `z3` (`0.21.1`), `libafl` (`0.16.1`), `icicle-vm` + `libsla`, `aya` CO-RE eBPF / Windows ETW, Landlock + `seccompiler` + `bubblewrap` + Firecracker microVM, `sccache` + `mergiraf`, embedded LSP/DAP servers. | Android-specific NDK/JNI (`AAudio`, `AHardwareBuffer`, `RoleManager`). |

---

## 7. Cross-Cutting Performance and Memory Budgets

Every subsystem implementation is gated by hard quantitative budgets verified in CI (Manual 06):

| Metric | Mobile Edge Budget (Snapdragon 8 Gen 3 / Tensor G4, 12 GB RAM) | Workstation Budget (8+ Core CPU, 16+ GB VRAM GPU) | Enforcement Mechanism |
|---|---|---|---|
| **Dormant RSS Memory** | `< 45 MB` | `< 90 MB` | `MemoryGovernor` calls `madvise(MADV_DONTNEED)` on Tier 3 weights on entry to `DORMANT`. |
| **Active Voice + Tactical SLM RSS** | `< 2.4 GB` (`Gemma-4-E2B` QAT or `Qwen3.5-1.7B/4B` mmap) | `< 8.5 GB` (`Gemma-4-12B` QAT or `Qwen3.5-9B`) | 4,096-token sliding working context + quantized KV-cache (`Q8_0`). |
| **VAD Frame Latency** | `< 0.6 ms` per 32ms window | `< 0.25 ms` per 32ms window | Pinned `mlock` Silero v5 int8 ONNX, single intra-op thread. |
| **Barge-In Mute Latency** | `< 5.0 ms` from verified voiceprint | `< 3.0 ms` from verified voiceprint | Atomic `Ordering::Release` flag read directly inside lock-free audio output callback. |
| **Time-to-First-Audio (TTFA)** | `< 185 ms` median (`< 280 ms` P99) | `< 140 ms` median (`< 210 ms` P99) | Tier 0 acoustic bridge (`"Affirmative."`) + pre-synthesized PCM cache for the 5 canonical Ciel prefixes! |
| **Grammar Token Mask Overhead** | `< 65 us` per token | `< 25 us` per token | `llguidance` `TokTrie` precomputed at startup; mask computed concurrently during GPU/NPU forward pass. |
| **Journal Append Latency** | `< 350 us` (NVMe/UFS group commit) | `< 120 us` (NVMe group commit) | Batching concurrent task transitions within a `200 us` coalescing window before `fdatasync`. |
| **Spatial Arbitration Window** | `15.0 ms` hard deadline | `15.0 ms` hard deadline | Monotonic local timer starting on first `SensoryClaim` arrival over Class 0 QUIC datagram. |

> **Engineering Note on Sub-170ms TTFA**: Synthesizing `"Affirmative."` from scratch on every turn wastes 35 to 60ms of ONNX compute on identical phonemes. Pre-rendering the five canonical Ciel bridge openers (`"Affirmative."`, `"Analyzing."`, `"Notice:"`, `"Negative."`, `"Report:"`) with matching Kokoro voice prosody variations at engine boot reduces Tier 0 acoustic bridge latency to `< 8ms` (pure ring-buffer enqueue) while Kokoro synthesizes the dynamic continuation clause in parallel!

---

## 8. Implementation Status (Living Section)

Update this table when a phase lands. A row is "done" only when its verification command passes on the commit that claims it.

| Phase (Manual 06 §4) | Scope landed | Verification | Status |
|---|---|---|---|
| 0 | `eon-primitives`: CSPRNG monotonic ULIDs, prefix and body validation, `ClaimId`/`SessionId`/`QueryId`/`MacroId`, `serde` feature (Manual 01 §2) | `cargo test -p eon-primitives --features serde` (7 tests) | Done |
| 1 (partial) | `eon-foundation::journal`: 32-byte CRC32 frames, group-commit `append_batch`, rolling segments, torn-tail repair, mid-file corruption refusal (Manual 01 §3.1) | `cargo test -p eon-foundation --test journal_crash` (proptest truncation at any byte, single-byte corruption, real SIGKILL writer loop) | Done (synchronous) |
| 1 (partial) | `eon-foundation::store`: WAL + `synchronous=NORMAL` pragmas, `sqlite-vec` auto-extension, forward-only projection checkpoints (Manual 01 §3.2) | `cargo test -p eon-foundation` | Done |
| 1 (partial) | `eon-foundation::credentials`: zeroized `EphemeralSecret` (no `Debug`), `secret://` argv refusal (Manual 01 §3.3) | `cargo test -p eon-foundation` | Done |
| 1 (partial) | `eon-foundation::scrubber`: Unicode strip, Aho-Corasick prefixes, token-qualified entropy with path/hash exemptions, BLAKE3 redaction markers (Manual 01 §3.4) | `cargo test -p eon-foundation` | Done |
| 1 (done) | `eon-foundation::group_commit`: worker thread drains queued payloads into one write and one `sync_data`, acks only after sync, bounded queue, restart durability (Manual 01 §3.1) | `cargo test -p eon-foundation --lib group_commit` (8 producers x 200 appends, restart) | Done |
| 1 (partial) | `eon-foundation::journal::replay_from`: sealed segments get a sparse `.idx` (one entry per 64 frames, written atomically on roll). Seeking skips earlier segments. A damaged index falls back to scanning and never changes results | `cargo test -p eon-foundation --lib journal` (3 index tests, mutation-checked) | Done |
| 1 (partial) | Scrubber coverage audit: the only free-text journal entry point is `append_scrubbed`. Kernel events (`eon-kernel::journal_sink`) carry typed IDs and task states only, so there is nothing to scrub there. Any future free-text writer must use `append_scrubbed` | code audit: `grep append_durable` | Done |
| 1 (partial) | `GroupCommitJournal::append_scrubbed`: text is scrubbed (invisible characters, known prefixes, entropy) before it is journaled. Test confirms the raw secret is absent from the replayed log | `cargo test -p eon-foundation --lib group_commit` | Done |
| 1 (partial) | `zstd` journal compression: opt-in `JournalWriter::set_compression`, frames flagged with `FLAG_ZSTD` (CRC covers stored bytes), payloads under 256 bytes or not shrinking are stored raw, replay decompresses and never exposes the flag, undecodable compressed frames are refused like bad CRCs | `cargo test -p eon-foundation` (32 unit tests including round trip, on-disk shrink, corrupt-frame refusal) | Done |
| 2 and 3 (partial) | `eon-kernel::resource`: canonical-order all-or-nothing acquisition, shared/exclusive locks, priority wait queue (Manual 01 §4.1). Synchronous, no `tokio::Notify`, no lease TTL reaping | `cargo test -p eon-kernel --test resource_model` (model-based random ops: capacity, writer exclusivity, drain liveness) | Done (synchronous core) |
| 2 and 3 (partial) | `eon-kernel::workflow`: Kahn validation, `ready()`, `splice_subgraph` with sink rewiring and snapshot revert (Manual 01 §4.2) | `cargo test -p eon-kernel --test workflow_properties` (random DAG execution, valid and cyclic splices) | Done |
| 2 and 3 (done) | `eon-kernel::fsm`: explicit 7-edge transition table, illegal and replayed transitions rejected (Manual 01 §4) | `cargo test -p eon-kernel --lib fsm` | Done |
| 2 and 3 (done) | `eon-kernel::saga`: forward execution with reverse compensation; a failing undo is recorded and the rest still run | `cargo test -p eon-kernel --lib saga` | Done |
| 2 and 3 (partial) | `eon-kernel::events`: `KernelEvent` (transitions, splices) written to an `EventSink` before success. Failed recording rolls back the state change | `cargo test -p eon-kernel --lib events` (4 tests) | Done |
| 2 and 3 (partial) | `eon-kernel::journal_sink`: events encoded as versioned records and written through the group-commit journal before success. Replay decodes back to the same events | `cargo test -p eon-kernel --lib journal_sink` | Done |
| 2 and 3 (partial) | `eon-kernel::epoch`: `CognitiveEpoch` with staged writes, atomic commit, rollback, and fencing so stale or replayed epochs cannot overwrite a newer commit. `commit_recorded` scrubs keys and values, records an `EpochCommitted` event before applying (a failed write applies nothing and leaves the epoch open). `SharedState::replay` rebuilds state from the journal and refuses an epoch that does not advance the fence | `cargo test -p eon-kernel --lib epoch` (10 tests incl. restart rebuild, secret scrubbing before the log, failing sink, corrupt replay) | Done for journal wiring |
| 2 and 3 (partial) | `eon-kernel::schedule::run_task_in_epoch`: runs a ready task's step inside an epoch. A failing step rolls back and leaves the task pending. Success commits the writes through the sink, then completes the task. A completion failure after commit leaves the task ready, so the step is at-least-once (Manual 01 §4) | `cargo test -p eon-kernel --lib schedule` (5 tests: success order, rollback, unfinished dependencies, fenced epoch, completion failure then retry) | Done. Parallel dispatch and retry policy are not implemented |
| 2 and 3 (partial) | `eon-kernel::shadow`: three-way tree merge for parallel shadow workspaces. Disjoint edits and identical edits are taken, overlapping text uses a line-level `diffy` merge, delete-vs-modify and non-UTF-8 overlaps fail closed, and the whole merge is all-or-nothing. A clean merge must pass a `VerifyGate` before it is accepted (Manual 01 §4.3). Adjacent-line edits conflict, which is conservative | `cargo test -p eon-kernel --lib shadow` (7 tests, incl. verification refusing a textually clean merge) | Done for the merge core |
| 2 and 3 (partial) | `eon-kernel::worktree`: shadow checkouts as detached `git worktree`s of a base commit, `snapshot` into the merge's `Tree` type. Two real task edits merge back through `merge_trees`, and overlapping edits in real worktrees conflict. Worktrees are real working copies, not copy-on-write overlays, and the structural CST merge is deferred: mergiraf 0.20 pins `termcolor ~1.1` through `stderrlog`, which conflicts with the `termcolor ^1.2` that `wasmtime 36` needs, so it does not resolve in this workspace | `cargo test -p eon-kernel --lib worktree` (2 tests) | Done for git worktrees. Overlayfs and CST merge not started |
| 2 and 3 (partial) | `eon-kernel::structural::merge_functions`: function-level structural merge for Rust and Python, built on the tree-sitter function blocks in `eon-cyber`. Edits to different functions merge cleanly, the same function changed differently on both sides conflicts, additions from both sides are kept, and text outside functions comes from the base. Substitutes for the deferred CST merge; it is not a CST merge, so two edits to one function always conflict | `cargo test -p eon-kernel --lib structural` (4 tests) | Done at function granularity. A true CST merge (mergiraf or tree-sitter-based) is still deferred behind the termcolor conflict |
| 2 and 3 (partial) | `eon-kernel::overlay`: copy-on-write shadow views with `fuse-overlayfs`. The shared base is the read-only lower layer and each task has a private upper layer, mounted unprivileged (no root helper). The merge reads the merged view through `worktree::snapshot`. `available()` lets callers fall back to worktrees. Installed here with `apt-get install fuse-overlayfs` | `cargo test -p eon-kernel --lib overlay` (mounts for real: base unchanged, siblings isolated, edits merge back, upper layers audited) | Done for overlay storage. Kernel overlayfs (root) and the CST merge are not used |
| Manual 06 §1 (partial) | `scripts/models.lock.json` pins (four verified Zipformer `2023-06-26` digests) and fail-closed `scripts/download_models.py` with resumable `.part` fetch | `python3` integrity test: resume via Range, server ignoring Range, digest mismatch | Done for the fetch path |
| Manual 06 §1 (deferred) | `lock --revision` HF API pinning, `--profile` selection, profile-scoped manifest at `perception/models/` | n/a | Not started |
| P0 (Manual 02 §2.1) | `eon-cognition::profile::ModelProfile::P0`: selection below 8 GB (6 to 8 GB gap goes here), lock key `p0_mobile_ultralight`. Lock resolves `common_voice` + Qwen3.5-0.8B Q4_K_M + Kokoro v1.0 int8. All artifacts pinned (HF LFS oid, Silero from tagged upstream). Substitutes 0.8B for the spec's 1.7B, which is not on Hugging Face | `cargo test -p eon-cognition`; `python3 -m unittest test_download_models` (from `scripts/`); `download_models.py --profile p0_mobile_ultralight --dry-run` | Done |
| P1 (Manual 02 §2.1) | `ModelProfile::P1`: 8 to under 12 GB. Gemma 4 E2B QAT Q4_0 GGUF (3.35 GB, google repo) + common voice + Kokoro v1.0 int8, all pinned | same as P0 | Done |
| P2 (Manual 02 §2.1) | `ModelProfile::P2`: 12 to under 16 GB. Gemma 4 12B QAT UD-Q4_K_XL (6.72 GB, unsloth repo; spec states 6.98 GB) + Kokoro v1.0 fp16 + common voice, all pinned. OmniParser `icon_detect` is AGPL-3.0 and is opt-in only (`gui_grounding_agpl` group), so it is not in the default profile. ONNX conversion is deferred until the AGPL decision is made | `python3 -m unittest test_download_models` | Done |
| P3 (Manual 02 §2.1) | `ModelProfile::P3`: 16 GB and up. Qwen3.6-27B Q4_K_M (16.8 GB, unsloth repo), Nemotron-3.5-ASR 160 ms int8 ONNX bundle (`csukuangfj2/sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-160ms-int8-2026-06-11`), common voice, Kokoro v1.0 fp16. Every artifact pinned. OmniParser is opt-in (AGPL), see P2. The ASR bundle replaces the spec's unavailable `nemotron-asr-0.6b-int8` name | `download_models.py --profile p3_workstation_heavy --dry-run` exits 0 | Done |
| 4 (partial) | `eon-capability::ifc::labels`: FIDES lattice (join = min integrity, max confidentiality), typed `ExtractedValue` endorsement (free-form values refused, SHA-256 validated at construction), bounded provenance (Manual 01 §5.1) | `cargo test -p eon-capability` (15 unit, 8 property, truth table) | Done |
| 4 (partial) | `eon-capability::ifc::rule_of_two`: deterministic triad gate, untrusted control-parameter gate with air-gap exemption (Manual 01 §5.2) | `cargo test -p eon-capability --test ifc_properties` | Done |
| 4 (done) | `eon-capability::mcp_pinning`: BLAKE3 length-prefixed fingerprint per tool, rug-pull quarantine, user-approved re-pin, 256-char planner summary (Manual 01 §5.5) | `cargo test -p eon-capability --lib mcp_pinning` (6 tests) | Done |
| 4 (partial) | `eon-capability::registry`: `Tool` trait and registry; every dispatch passes the pin store and the Rule-of-Two gate before the tool runs. Quarantined tools refused (Manual 01 §5.2, §5.5) | `cargo test -p eon-capability --lib registry` (5 tests) | Done |
| 4 (partial) | `eon-capability::schema`: canonical JSON (sorted keys) and input validation for the declared subset (`type`, `properties`, `required`, `enum`, `items`, closed `additionalProperties`). Unknown keywords refused. Dispatch validates input before any tool runs | `cargo test -p eon-capability --lib schema registry` | Done |
| 4 (partial) | `eon-capability::quarantine`: Spotlighting with a fresh 128-bit CSPRNG nonce per call, delimiter spoofing refused, invisible characters stripped, quarantined output schema-checked and kept Untrusted (Manual 01 §5.3) | `cargo test -p eon-capability --lib quarantine` (7 tests, mutation-checked) | Done |
| 4 (partial) | `eon-capability::melon`: masked re-execution check. The masked model run sits behind a `MaskedRunner` trait. A proposed call is refused with `SecurityViolation` when a masked-run call scores above 0.65 cosine similarity on canonical token bags (Manual 01 §5.4) | `cargo test -p eon-capability --lib melon` (5 tests) | Done as a check and wired into dispatch: `CapabilityRegistry::dispatch_guarded` runs it after pin, schema, and Rule-of-Two checks when the call follows untrusted context and a runner is installed (`cargo test -p eon-capability --lib registry`, 2 MELON tests). `eon-cognition::melon::LocalMaskedRunner` runs the masked pass on the pinned local model under the neutral goal and parses JSON tool calls from its output (tolerant: malformed or prose yields no call). The e2e test checks the contract (the run completes, calls are well formed, the check follows the similarity result), not model quality (`cargo test -p eon-cognition --test melon_e2e`, plus 4 parser unit tests). The spec's `Summarize the formatting` goal is used verbatim |
| 4 (partial) | `eon-capability::sandbox`: `run_confined` runs a tool process under a Landlock ruleset (read and execute only on listed paths, everything else denied) and an optional seccomp filter that refuses `socket` and `socketpair` with EPERM. The ruleset and filter are built in the parent, and the child fails before exec if Landlock is not enforced (Manual 01 §5, sandbox layer) | `cargo test -p eon-capability --lib sandbox` (3 tests: allowed read, denied read outside policy, socket allowed without and refused with `deny_network`) | Done for filesystem and socket confinement. Wasmtime is not used, and the kernel-version fallback is not tested |
| 4 (partial) | `eon-capability::wasm_tool`: WebAssembly tool execution on Wasmtime 50 with fuel metering (budget exhaustion reported as `ResourceExhausted`) and an empty linker, so any module that declares an import fails to instantiate. No WASI. Tools are called as `(i32) -> i32` exports for now | `cargo test -p eon-capability --lib wasm_tool` (4 tests: pure call, fuel stop, import refusal, missing export) | Done for the execution sandbox. Rich tool I/O and the component model not started |
| 5 (partial) | `eon-voice`: 3-tier endpointer (300/800/1150 ms, 300 ms breath holdoff), peak-limiting ASR gain (instant attack, smoothed release), frame geometry (Silero 512, AEC 160, Zipformer 320 ms) (Manual 04). Silero v5 VAD inference (`ort` rc.10) and Zipformer `2023-06-26` streaming transducer ASR (`asr.rs`: kaldi-native-fbank 80-bin features, 45-frame chunks with shift 32, cache carry, greedy search) | `cargo test -p eon-voice` (15 unit, 2 ASR e2e, 4 VAD e2e); ASR e2e transcribes upstream `en.wav` to `THE DRIBAL CHIEFTAIN CALLED FOR THE BOY AND PRESENTED HIM THE FIFTY PIECES OF GOLD` exactly | Done for ASR and VAD inference |
| 5 (partial) | `eon-voice::vad`: real Silero v5 ONNX inference via `ort` (pinned to `=2.0.0-rc.15`, the latest release; sessions are built against the shared environment in `eon-voice::runtime`). Model digest-verified from `scripts/models.lock.json`. End-to-end test runs the model over the upstream `test_wavs/en.wav` (16 kHz speech, sha256 eb1eb008...): speech frames detected, pauses scored quiet, endpointer closes a turn, silence scores low, state reset is repeatable | `cargo test -p eon-voice --test vad_e2e` (4 tests, real model) | Done |
| 5 (partial) | `eon-voice::ring`: wait-free `rtrb` SPSC PCM ring. Overflow drops the newest samples and counts them. Barge-in `flush` releases all queued samples in one index update (Manual 04 §2) | `cargo test -p eon-voice --lib ring` (3 tests incl. 200k-sample cross-thread order); `cargo test -p eon-voice --test ring_no_alloc` (counting global allocator: 0 allocations across 10k push/pop cycles and a flush, with a control test proving the counter sees allocations) | Done for the ring |
| 5 (partial) | `eon-voice::resample`: mono windowed-sinc resampling with rubato 5 (`process_all`, so no leading startup silence). A 440 Hz tone at 48 kHz converts to 16 kHz with length and frequency preserved | `cargo test -p eon-voice --lib resample` (2 tests) | Done for resampling |
| 5 (partial) | `eon-voice::device`: cpal capture (downmix to mono into the PCM ring) and playback (mono fanned out to every channel, silence and an underrun count when the ring is empty). Driver errors and callback counts are exposed. The first ALSA period takes over a second to arrive | `cargo test -p eon-voice --test device_loop` (2 tests on the host's default devices). These need a running audio server. In this codespace that is PulseAudio started on `/tmp/runtime-vscode/pulse/native`. A device that exists but never delivers callbacks fails the test, it does not skip. Playback requests an explicit 4096-frame buffer: with the backend default, PulseAudio's ALSA plugin failed `snd_pcm_avail_delay` with I/O error 5 at stream start (before the first callback) in about a third of runs. With the fixed buffer alone, one full workspace run still failed. The device now also rebuilds the stream when the driver reports an error before 10 callbacks (up to 3 attempts, counted in `PlaybackHandle::startup_retries`), and the playback consumer lives behind a `Mutex` that the callback only `try_lock`s, so a rebuild keeps the queued audio. Afterwards the device binary passed 40 of 40 runs (it failed 4 of 30 before the retry). In full workspace runs since then, one run failed the steady-state check (errors after the stream was healthy, which the startup retry does not cover). Diagnostic runs that passed showed the stream reaching 10 callbacks in 0.7 to 2.1 s with no errors. That steady-state error is not yet explained | Done for the device loop on PulseAudio. AEC3 and Kokoro TTS are built (see the rows below) |
| 5 (partial) | `eon-voice::tts`: Kokoro-82M int8 text-to-speech. Text goes to en-us IPA through espeak-ng (stdin only, never argv), then to token ids via the 114-entry vocabulary in `assets/kokoro_vocab.json` (origin recorded there). The style row is picked by token count, as in the reference, and the output is 24 kHz. Requires `espeak-ng` on PATH. Assets: `kokoro-v1.0-int8` and `kokoro-v1.0-voice-af_heart`, both digest-verified | `cargo test -p eon-voice --lib tts` (vocab) and `cargo test -p eon-voice --test tts_e2e` (round trip: the synthesized sentence is transcribed by the pinned ASR as "A LOW WORLD THIS IS A TEST OF THE VOICE", and the test requires 3 of 5 content words) | Done for TTS. The pinned ASR reads "hello" as "a low", so the round trip is a real intelligibility check, not a formality. Streaming TTS and AEC3 are built (see the rows below) |
| 5 (partial) | `eon-voice::aec`: WebRTC AEC3 echo cancellation (`webrtc-audio-processing` 2.1, bundled build). 10 ms frames, fixed stream delay, render analyzed before capture is cleaned in place. Measured on a 440 Hz render with a 45 ms, half-amplitude echo: echo reduction 45.0 dB after convergence; near-end speech without echo keeps 0.998 of its energy. Tests require 15 dB and 0.7 | `cargo test -p eon-voice --lib aec` (3 tests: echo cancelled, near-end preserved, frame length refused). The bundled C++ builds in about 40 s with CMake and Ninja under `/tmp/cargo-target`. Barge-in controller, double-talk tests, and the delay-aligned render reference from the spec are not built yet | Done for AEC3 inference. The barge-in controller, WeSpeaker gate, and streaming TTS are built (see the rows below) |
| 5 (partial) | `eon-voice::barge_in`: a shared flag from the speech detector to the playback callback. The next callback flushes the TTS ring and writes silence, and the flush is counted (`barge_in_flushes`, `flushed_samples`). The device test confirms playback is cut and the request is consumed. The in-ring flush is timed under 5 ms on a full 1M-sample ring in release builds only, because rtrb walks the discarded slots and that costs time in debug builds | `cargo test -p eon-voice --lib barge_in`; `cargo test -p eon-voice --test device_loop barge_in` (needs an audio server); `cargo test --release -p eon-voice --lib ring` (timed flush) | Done for the flush path. Built since: the AEC-plus-barge-in loopback with self-echo rejection (NLMS canceller), the WeSpeaker speaker gate, streaming TTS, and the delay-aligned render reference. Not built: wiring the speech-onset detector into the live device loop |
| 5 (partial) | `eon-voice::linear_echo` and `barge_in::SpeechOnsetDetector`: barge-in detection runs on an NLMS residual (1024 taps, mu 0.2), which removes only the linear echo. Measured: echo residual power 9e-10 after convergence, and owner speech keeps 89% of its energy. AEC3 output is not used for detection, because its suppressor removed near-end speech (residual RMS 0.0006 against roughly 0.07 expected) whenever the render was active, even with louder or broadband owner speech. AEC3 stays on the ASR path. Loopback: owner speech over a 45 ms echo triggers within 200 ms, with no trigger before 1.2 s | `cargo test -p eon-voice --lib linear_echo`; `cargo test -p eon-voice --lib barge_in` (onset run logic); `cargo test -p eon-voice --test aec_barge_in_loopback` | Done for detection on the loopback. The WeSpeaker speaker gate, streaming TTS, and the delay-aligned render reference are built. Not built: wiring the onset detector into the live device loop |
| 5 (partial) | `eon-voice::speaker`: WeSpeaker ResNet34 speaker embeddings (256-dim, L2-normalized) on the shared 80-bin fbank with per-utterance mean normalization, and `SpeakerGate` (cosine against an enrolled owner). Pin: the lock URL pointed at a file that does not exist (`model.onnx`); it is now `voxceleb_resnet34_LM.onnx`, with the LFS sha256 `7bb2f06e…` and 26,530,309 bytes. The downloader's refusal test now uses its own unpinned fixture, because no real entry is unpinned. Measured: same speaker (two halves of `en.wav`) 0.657, the assistant's Kokoro voice 0.000 | `cargo test -p eon-voice --lib speaker` (3 tests); `cargo test -p eon-voice --test speaker_e2e` (same speaker clears the gate, assistant voice does not); `python3 -m unittest test_download_models` (24 tests) | Done for the gate. A two-way split of one recording is weaker than two recordings, and the owner enrollment flow is not built. The spec's 1.8 ms budget is not measured |
| 5 (partial) | `eon-voice::tts` streaming and `eon-voice::delay`: `split_clauses` cuts text after `.!?;:,`, and `synthesize_streaming` hands each clause to a callback as soon as it is synthesized, with first-audio and total timing. On the pinned model a three-sentence reply splits into four clauses (commas split too), and the first audio arrives well before the reply finishes. `DelayLine` holds the playback reference back by a fixed latency. At a 400 ms echo delay, AEC3 cancels 77.2 dB with the reference aligned and 58.9 dB without, so alignment matters. At 45 ms AEC3's adaptive filter absorbs the offset and alignment barely matters | `cargo test -p eon-voice --lib tts delay`; `cargo test -p eon-voice --test tts_e2e` (streaming); `cargo test -p eon-voice --test aec_alignment` | Done for streaming TTS and delay alignment. Playback-path latency is not measured on a real device yet, so the 400 ms figure is a chosen test value |
| Pins (corrected) | Zipformer 2023-06-26 pins were copied from the k2-fsa release tarball (`sherpa-onnx-streaming-zipformer-en-2023-06-26.tar.bz2`, sha256 639e25b5...). The same-named Hugging Face repo has different bytes, so those URLs were the wrong source. Downloader now supports `archive` entries: fetch the tarball once into `perception/models/downloads/` (gitignored), verify it, extract the named member, verify its digest. Qwen3.5-0.8B pin had a one-digit typo (caught by fail-closed verification) and is fixed. All HF LFS pins were re-checked against the API oids; the only mismatches were the three Zipformer files above | `download_models.py --ids zipformer-en-2023-06-26-*` (real fetch, rerun skips) and `python3 -m unittest test_download_models` (24 tests) | Done |
| 6 (partial) | `eon-cognition::memory`: reciprocal rank fusion (k=60), Wilson lower-bound procedural ranking, `MemoryGovernor` that releases tiers 2 and 3 on dormant (Manual 02 §4 to §5) | `cargo test -p eon-cognition --lib memory` (12 tests incl. profile) | Done |
| 6 (partial) | `eon-cognition::backend`: real llama.cpp inference via `llama-cpp-2` 0.1.161, greedy decoding, one process-wide backend (llama.cpp allows a single init). End-to-end over the pinned Qwen3.5-0.8B Q4_K_M: produces coherent text, identical output on repeat, oversized prompt refused. The model's answer to the test prompt is not factually correct, and the test does not assert on content | `cargo test -p eon-cognition --test llm_e2e` (3 tests, real model) | Done |
| 6 (deferred) | `mistralrs` backend, KV pre-cognition, OmniParser grounding | n/a | Not started |
| 6 (partial) | `eon-cognition::grammar` and `LocalModel::generate_constrained`: tool-call GBNF (`{"name": <allow-listed tool>, "arguments": <object>}`), with tool names restricted to `[a-z0-9._-]`. Decoding chains the grammar sampler with greedy in llama.cpp. A GBNF grammar is used rather than llguidance: llguidance needs a llama.cpp build flag (`-DLLAMA_LLGUIDANCE=ON`) that this crate does not set. A bug found on the way: `LlamaSampler::sample` already accepts the token, so the extra `accept` aborted llama.cpp with `GGML_ASSERT(!stacks.empty())`. On the pinned model, the unconstrained reply to a tool prompt is a JSON array; the constrained reply is `{"name":"fs.read","arguments":{"path":"current directory"}}` | `cargo test -p eon-cognition --lib grammar` (2 tests); `cargo test -p eon-cognition --test grammar_e2e` (parse, allow-list, object arguments) | Done for grammar-constrained tool calls. mistral.rs, KV pre-cognition, and OmniParser not started |
| 6 (partial) | `LocalModel::build_prefix_cache` and `generate_with_prefix`: a fixed prefix is decoded once, its context state is captured with llama.cpp's state copy, and later prompts that start with the same tokens restore it and decode only the new tokens. A prompt that does not start with the prefix falls back to a full decode. Verified on the pinned model: cached and full greedy output are identical, 64 prefix tokens are reused, and the unrelated prompt reuses none. One run: 1.06 s full, 0.71 s cached, mostly the 24 generated tokens, so the speed gain is not established by this test | `cargo test -p eon-cognition --test kv_cache_e2e` (2 tests: identical output with reuse, fallback without reuse) | Done for prefix reuse. Pre-warming at boot, cache eviction, and multi-prefix pools not built |
| 6 (partial) | `LocalModel::warm_up`: one short greedy generation that pages in the weights and compute graph and returns how long it took. Warming does not change later greedy output on the pinned Qwen model | `cargo test -p eon-cognition --test warm_up_e2e` (1 test) | Done as a primitive. The daemon does not call it at boot yet, and mistral.rs is not wired |
| 7 (partial) | `eon-cyber::taint`: petgraph source-to-sink slicing, sanitizer removal, bypass detection, witness paths (Manual 03 §2) | `cargo test -p eon-cyber --lib` (6 tests) | Done |
| 7 (partial) | `eon-cyber::sbom`: CycloneDX 1.5 JSON emission with purls (crates.io maps to `pkg:cargo`), OSV advisory parsing from local JSON, and exact-version matching. Advisories that only give ranges are reported as `Unverified`, never clean (fail closed) | `cargo test -p eon-cyber --lib sbom` (4 tests) | Done for offline SBOM and advisory matching. Live OSV fetch, range resolution, and the polyglot CPG are not started |
| 7 (partial) | `eon-cyber::cpg`: tree-sitter 0.27 code graph for Rust (tree-sitter-rust 0.24) and Python (tree-sitter-python 0.25). Functions are nodes. Calls to same-file functions are resolved edges, and other calls are kept as unresolved, so none are silently dropped. Reachability follows resolved edges only | `cargo test -p eon-cyber --lib cpg` (3 tests: resolution and unresolved calls in Rust, reachability direction, Python attribute calls) | Done for same-file call graphs in Rust and Python. Cross-file resolution, other languages, and wiring into the taint slicer are not built |
| 7 (partial) | `eon-cyber::interproc::call_flow_findings`: source-to-sink search over the code graph's resolved call edges, with sanitizer functions removed from the search and the shortest witness returned. Call edges carry no values, so a direct call from a source to a sink is reported even when another branch is sanitized. That is the conservative direction, and the test documents it | `cargo test -p eon-cyber --lib interproc` (4 tests: witness through helpers, sanitizer on every path, bypass still reported, unknown names) | Done for same-file call-graph taint. Cross-file resolution is built (see the rows below). Value-level tracking is not built |
| 7 (partial) | Cross-file call resolution (`eon-cyber::cpg::build_project`): nodes are `path::function`. A qualified call (`store::save`, Python `store.save`) resolves to the module the qualifier names. An unqualified call prefers its own file, then a single definition elsewhere in the same language. Several matches are reported as ambiguous and never guessed. Calls that match nothing, such as a qualifier naming a crate outside the project, stay unresolved | `cargo test -p eon-cyber --test project_graph` (11 tests) | Done for module-qualified, Rust `use`-imported (including `as` aliases and grouped `use a::{b, c}`), and name-based resolution. Not built: receiver types for method calls (`x.save()` stays name-only), glob imports (`use a::*`), nested groups, and module trees beyond the file's stem or directory name |
| 7 (partial) | Firecracker microVM runs (`eon-capability::microvm`): boots Firecracker v1.18.0 on KVM with a 5.10 guest kernel and initramfs, types a script into the guest's serial console once the shell prompt appears, and returns the serial output. The script ends with `reboot -f`, which the `reboot=k` guest turns into a KVM reset, so the VMM exits. A run past its deadline is killed. `scripts/fetch_firecracker.sh` downloads and checksum-verifies the artifacts into `/tmp/eon-firecracker` (re-creatable). KVM access works for this user through the `glances` group, so no permission change is needed | `cargo test -p eon-capability --lib microvm` (2 unit tests). End to end, with the artifacts from the script: `EON_FC_BIN=... EON_FC_KERNEL=... EON_FC_INITRD=... cargo test -p eon-capability --test microvm_e2e -- --ignored` (1 test, about 1.2 s) | Done for booting and running commands in a microVM. Not built: a block-device rootfs (the guest uses the initramfs), network, jailer confinement, and wiring microVMs into the `dispatch_guarded` tool path |
| 7 (partial) | eBPF execve probe (`engine/bpf/exec_count.bpf.c`, loaded by `eon-ebpf::ExecCounter` with aya 0.16). The probe is a tracepoint on `syscalls:sys_enter_execve` that counts launches in a BPF array map. The probe is self-contained C (no libbpf headers), built by `scripts/build_bpf.sh` with clang. Attaching needs `CAP_BPF`, because `kernel.unprivileged_bpf_disabled=2` on this host, so the test runs as root, and tracefs must be mounted at `/sys/kernel/tracing` (on this host it was not mounted, and mounting it is a non-persistent change). The test binary is built as the user and run with sudo | `cargo test -p eon-ebpf --test exec_probe --no-run`, then `sudo env EON_BPF_OBJ=/tmp/eon-bpf/exec_count.bpf.o <printed binary> --ignored` (1 test: the counter rises by at least five after five `/bin/true` launches) | Done for counting execve on this host (kernel 6.8, aya 0.16). Not built: aya's loader with BTF CO-RE relocations for other kernels, per-process filtering, eBPF enforcement (blocking calls), and running the probe from the daemon |
| 7 (partial) | Z3 path-condition solver (`eon-cyber::solver`, z3 0.23 with the `bundled` feature, which compiles Z3 from source in about 4 minutes on this host). A branch condition is a `Constraint` over named integer inputs. `solve` returns concrete inputs that satisfy it or `Unsat`, and `holds` evaluates a constraint on concrete inputs so a model is checked directly rather than trusted. Unsat is reported for a dead branch | `cargo test -p eon-cyber --lib solver` (4 tests: a sat branch's model holds, an impossible branch is unsat, disjunction and negation encode correctly, a missing input makes a constraint false) and `cargo test -p eon-cyber --lib branches` (4 tests: lowering with the literal on either side, negation and parentheses, unsupported conditions reported, a lowered condition solved to a witness or unsat) | Done for the solver. Source `if` conditions lower into `Constraint` (`eon-cyber::branches::branch_conditions`): comparisons of an identifier with an integer literal, combined with `&&`, `||`, `!`, and parentheses. Anything else is reported as unsupported, not approximated. Not built: the Z3 overlay on CFG branch edges in the canvas, which needs the p-code lifter (libpcode is not built), and conditions over calls, arithmetic, or two variables |
| 7 (deferred) | libpcode/icicle-vm, LibAFL |
| 8 (partial) | `eon-mesh::election`: 15 ms window on local arrival time only (sender clock never used), tie-break by id, +10 hysteresis margin (Manual 05 §2) | `cargo test -p eon-mesh --lib` (7 tests) | Done |
| 8 (partial) | `eon-mesh::sync`: Loro 1.x replica with offline edits, update export and apply, convergence across three replicas in any order, idempotent replay, corrupt update refused. Transport not included | `cargo test -p eon-mesh --lib sync` (5 tests) | Done |
| 8 (partial) | `eon-mesh::transport`: iroh 1.4 peer transport. Endpoints dial by public key over QUIC with the `eon/mesh/1` ALPN, relays are disabled, and request/reply runs on bidirectional streams with a 1 MiB frame cap. Peers on another ALPN are refused in the handshake | `cargo test -p eon-mesh --test transport_loopback` (2 tests: round trip, ALPN refusal) | Done for loopback transport. Relay-backed dialing is not started |
| 8 (partial) | `eon-mesh::qos`: four traffic classes from manual 05 §2.2. Audio is sent as QUIC datagrams (`Link::send_audio` refuses frames above the path limit instead of splitting them). Control, delta, and bulk use streams with send priorities 100, 50, and 0 (`PeerNode::request_with_class`). Higher priority is transmitted first in noq | `cargo test -p eon-mesh --test qos_lanes` (3 tests: audio datagrams in order, oversize refused, classed stream round trip and audio refused as a stream) and `cargo test -p eon-mesh --lib qos` (1 test) | Done for loopback. Bulk blob transfer (iroh-blobs with BLAKE3 verification and the 25% link-rate throttle) is not built |
| 8 (partial) | Multi-node sync over the transport (`eon-mesh/tests/multi_node_sync.rs`): two peers with offline edits send their full Loro state over iroh QUIC, the server applies it and replies with its own state, and the client applies the reply. Both replicas hold both keys, and conflicting writes to one key resolve identically. Replaying an older update after a newer write does not roll the key back | `cargo test -p eon-mesh --test multi_node_sync` (2 tests) | Done for two-peer full-state sync on loopback. More than two peers are covered by the next row |
| 8 (partial) | Delta sync (`eon-mesh::sync::Replica::version` and `export_since`, exercised over the transport in `eon-mesh/tests/delta_sync.rs`). The client sends its Loro version vector, the server replies with its own version vector plus only the updates the client lacks, and the client sends back the updates the server lacks. For a shared history of 200 keys, the reply is under a quarter of full state. A corrupt version vector is refused | `cargo test -p eon-mesh --lib sync` and `cargo test -p eon-mesh --test delta_sync` | Done for two peers |
| 8 (partial) | Three-peer convergence through a hub (`eon-mesh/tests/three_peer_sync.rs`): three peers write offline, each syncs with the hub in two rounds using the full-state push and delta reply, and all four replicas agree on every key and on the shared key | `cargo test -p eon-mesh --test three_peer_sync` | Done for a hub topology on loopback. Neighbour-to-neighbour gossip is in the ring row |
| 8 (partial) | Peer discovery by signed announcements (`eon-mesh::discovery`). A peer signs its own address with its endpoint key (`PeerNode::announce`). A directory accepts an announcement only when the signature verifies, the address belongs to the signing key, and the sequence number is newer than the one held. Forged, tampered, or stale announcements are refused, and a peer found this way can be dialed | `cargo test -p eon-mesh --lib discovery` (6 tests) and `cargo test -p eon-mesh --test discovery` (1 test) | Done for signed announcements carried by any third party. Relay- or DNS-backed lookup, mDNS (`iroh-mdns-address-lookup`), and key rotation are not built |
| 8 (partial) | Neighbour gossip (`eon-mesh::exchange`, `eon-mesh/tests/gossip_ring.rs`). The one-round offer and answer is now a library module shared by the hub and ring tests. Three peers in a ring (A with B, B with C, C with A) each exchange only with a neighbour, and after one pass every replica holds every write | `cargo test -p eon-mesh --lib exchange` (2 tests) and `cargo test -p eon-mesh --test gossip_ring` (1 test) | Done for a ring on loopback. Fan-out and rounds are in the next row |
| 8 (partial) | Fan-out anti-entropy (`eon-mesh::exchange::pick_neighbours`, `eon-mesh/tests/anti_entropy.rs`). Each round, every peer exchanges with a seeded subset of two other peers, and repeats until it holds every write. Five peers converge within eight rounds, and no peer talks to all the others in one round. Picks are deterministic per seed, so a schedule can be replayed | `cargo test -p eon-mesh --lib exchange` (4 tests) and `cargo test -p eon-mesh --test anti_entropy` (1 test) | Done for loopback with fixed rounds. Partition healing, adaptive scheduling, and backoff are not built |
| 8 (partial) | Bulk blob transfer (`eon-mesh::blob`, `eon-mesh/tests/bulk_blob.rs`): a manifest of per-chunk BLAKE3 hashes, verified chunk by chunk and assembled in any order. Each chunk is fetched on its own Bulk-class (priority 0) stream. A substituted chunk stops the transfer at that index. The manifest must come from a trusted source such as an epoch commit | `cargo test -p eon-mesh --lib blob` (5 tests) and `cargo test -p eon-mesh --test bulk_blob` (2 tests) | Done for verified chunked transfer on loopback, paced: `blob::fetch` waits for `qos::BulkPacer` before each chunk (25% of the link for 200 ms after interactive traffic). `blob::fetch_missing` resumes an interrupted transfer by requesting only the missing chunks (tested with a refused chunk as the interruption, not a dropped connection). iroh-blobs Bao streaming is in the next row |
| 8 (partial) | Bao-verified streaming with iroh-blobs 0.105 (`eon-mesh/tests/bao_stream.rs`): a provider serves a 3 MiB blob from an in-memory store through an iroh `Router` on `iroh_blobs::ALPN`, and a client streams it with `get_blob` and checks the bytes. A hash the provider does not hold is refused by a stream reset. The blueprint's earlier `iroh-blobs = "0.34"` pin was stale, and 0.105 targets iroh 1.0, which matches the workspace | `cargo test -p eon-mesh --test bao_stream` (2 tests) | Done for verified streaming on loopback. Resume across providers, the persistent fs store, and the 25% link-rate pacer on this path are not wired (the custom blob path has the pacer) |
| 9 (partial) | `eon-cli` (`eon status <dir>`): status computed from the durable journal on disk, rendered via ratatui (TestBackend snapshot). Binary-level end-to-end test runs the compiled `eon` against a real journal, plus usage and missing-directory exit codes | `cargo test -p eon-cli` (3 unit, 2 e2e) | Done |
| 9 (partial) | `eon-uniffi`: exported `select_model_profile` and `endpoint_silence_ms`, typed errors. Kotlin bindings are generated from the built cdylib and compile with kotlinc 2.4.21 (JDK 17). Compiling exposed a clash: the `message` field of the error variant collided with `Exception.message`, so it is now `reason`. A JNA smoke test runs the bindings against the native library | `crates/eon-uniffi/kotlin/run_smoke.sh` (needs JAVA_HOME, KOTLINC, JNA_JAR, KOTLIN_STDLIB_JAR) prints `eon-uniffi kotlin smoke: OK`; `cargo test -p eon-uniffi --lib` | Done (bindings compile and run on Linux); Android AAR packaging and Kotlin unit tests not done |
| 9 (partial) | `eon-daemon`: Unix socket status service. Socket mode 0600, peer UID checked with SO_PEERCRED before any read, stale sockets replaced but regular files never deleted, unknown commands rejected. Real client/server round trip over the kernel socket. NamedPipe (Windows) not included | `cargo test -p eon-daemon` (6 tests) | Done (Unix only) |
| 9 (partial) | `eon watch <journal-dir>`: live ratatui view on crossterm. Each refresh reads the journal from disk, so it shows durable state only. `q` quits, the terminal is restored on every exit path, and running without a terminal fails with exit 1 | `cargo test -p eon-cli` (6 unit tests incl. key handling, missing directory, snapshot); pty run confirmed exit 0 on `q` with the title drawn and the alternate screen restored | Done for the TUI. The headless `wgpu` canvas and the `egui` window shells are built (see the rows below) |
| 9 (partial) | `eon-canvas` (`engine/core-rs/crates/eon-canvas`, not `clients/canvas` yet so the workspace builds and tests it): Sugiyama layout (longest-path ranks, dummy nodes for long edges, eight barycenter sweeps, crossing count, cycles refused) and a headless `wgpu` 30 renderer that draws node boxes and edge polylines to RGBA8. Renders on any adapter, including lavapipe on CPU hosts | `cargo test -p eon-canvas` (8 unit tests for layout and diff matching, 1 render test that checks pixel colors, 2 headless egui tests for the layout and diff views) and `cargo test -p eon-cyber --test function_blocks` (2 tests) | Done for layout, offscreen rendering, and an `eframe` window (`cargo run -p eon-canvas --example view`, checked under Xvfb with a screenshot). The headless egui view test counts boxes, labels, and segments. AST structural diff pane (`eon-canvas::diff`, `view::paint_diff`, `cargo run -p eon-canvas --example diff`): functions from `eon-cyber::cpg::function_blocks` matched by name, classed unchanged, changed, added, or removed, with a Bezier ribbon per matched pair. Apply and NACK buttons per changed, added, or removed function record a reviewer decision (`diff::Review`, `view::review_controls`). Unchanged pairings need no review. A headless test sends real pointer clicks to the buttons. `diff::apply_review` builds the patched source from the decisions: applied changes take the new text, applied removals are deleted, applied additions are appended at the end, NACKed changes keep the old text, and any pending review refuses to apply (`cargo test -p eon-canvas --test apply_review`, 2 tests). `writeback::write_back` puts the patch in a shadow checkout of the reviewed commit, merges it with the repository's current tree through a `VerifyGate`, and writes the file only if the gate passes. The shadow is removed on every path (`cargo test -p eon-canvas --test writeback`, 2 tests: accepted, and refused by the gate with the repo untouched). Ribbons are cubic Bezier curves that bend between rows when a matched pair sits on different rows. Not built: P-code CFG pane, Z3 overlay, DAP highlight, |
| 10 (partial) | `engine/android/core-bindings`: standalone Gradle (8.13 wrapper, Kotlin/JVM 2.3.0) module. The build compiles the Rust `eon-uniffi` cdylib, generates its Kotlin bindings, and runs the tests against the native library through JNA. No CHRONOS toolchain or files are used | `./gradlew :core-bindings:test` (3 tests: profile selection, endpoint thresholds, typed `InvalidInput` error) | Done for the binding layer on the JVM. Android application module (AAR packaging, `ROLE_ASSISTANT`, MediaProjection FGS, AAudio, Compose HUD, AHardwareBuffer) not started |


Known spec corrections made while implementing:

- Frame CRC is CRC32 (IEEE) from `crc32fast`, so the header field is named `crc32`, not `crc32c` (Manual 01 §3.1).
- Model profiles: the lock uses named groups (`common_voice`) as in Manual 06 §1.1. Model ids follow the artifacts actually found on Hugging Face, not the spec names where those differ (Qwen3.5-0.8B for P0, Gemma 4 QAT GGUF repos, Kokoro v1.0, Nemotron 160 ms sherpa int8 bundle for P3). Schema is now v3. Silero and `tokens.txt` digests come from downloading the upstream file, since they are not LFS-tracked, so they are not HF LFS oids.
- Open conflict: Manual 02 gives P3 a 16 GB weight footprint, while §7 caps the workstation active voice plus tactical SLM RSS at 8.5 GB. These cannot both hold. Resolve in the spec before Phase 6 sets the memory governor.
- Scrubber known-prefix matches must start a token. Without this, `xoxb-` inside the path segment `axoxb--0` redacted the rest of the token. Prefix boundaries count only alphanumerics and `_`, so `auth=ghp_...` still redacts.
- Scrubber path segments must be words: one optional leading capital, then lowercase, digits, `_`, `-`, `.`. The earlier "at most one case change" rule still let random base64 such as `jD0/l5AEFn3G1B/opca2H0Idefq4g/J/` through.
- Scrubber path exemption originally let AWS-style keys through (`wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY` was treated as a path). Fixed: a path segment must be a word with at most one case change.
- Resource requests larger than total capacity are refused with `ResourceExhausted`. Queuing them would block the queue forever.
- Queued requests are granted head-of-line in priority order, so a large high-priority request cannot be starved by smaller later ones.
- Journal tail policy (decided): frames are fsynced before acknowledgement, so a torn write can only leave an unacknowledged prefix or a zero-filled extension. Those are truncated. A complete-length frame that fails its CRC is refused, never silently dropped, and `repair_torn_tail` is the explicit operator path. RocksDB's tolerant mode cannot distinguish the two cases, so it was rejected. Known residual risk: a power loss that extends the file size with partly written payload looks like corruption and needs operator repair.
- SIGKILL tests do not exercise power loss. Page-cache data survives a process kill, so they check ordering and recovery, not fsync durability.
- `SecurityLabel` has no `serde` derives yet (Manual 01 §5.1 shows them). Add them when the journal starts persisting labels.
- `ExtractedValue` validates on construction, so `endorse` cannot be called with an unchecked hash or non-finite float. This is stricter than the doc's `endorse(type)` sketch.
- The kernel resource manager is synchronous. Async waiting is a thin layer over `acquire`/`release` and is deferred.
- `splice_subgraph` rewires only pre-existing downstream tasks. Newly spliced roots depend on the parent. Applying the rewire to new roots creates a cycle (caught by the cycle test).
- Unpinned models (`sha256: null`) are refused rather than fetched. The baseline's placeholder digests are removed.
