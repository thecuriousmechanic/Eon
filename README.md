# EON — Engine Optimized for Networking & Autonomous Intelligence

> *"Report: Individual 'Rimuru' has expressed intent. Thought Acceleration initiated. Information Particle Manipulation engaged. Analysis complete. Executing multi-step cyber, software, and system optimization across all nodes."*  
> — **Manas: Ciel Architecture Core**

---

## 1. Executive Summary & Vision

The architecture below describes the target, not a released feature set. For what runs today, see the [roadmap checkboxes](./docs/ROADMAP_AND_IMPLEMENTATION_PLAN.md) and the [verification commands and limits](./docs/IMPLEMENTATION_BLUEPRINT.md#8-implementation-status-living-section). In particular, a passing host test does not establish phone microphone latency, WAN performance, or device permissions.

**EON** is an autonomous, ultra-low-latency AI Personal Assistant, Cyber Operator, and Cognitive Operating System inspired by **Ciel** (*Manas: Ciel*, formerly *Great Sage* and *Wisdom King Raphael*) from *That Time I Got Reincarnated as a Slime* (*Tensei Shitara Slime Datta Ken*).

Unlike conventional chatbots that wait passively for commands, demand constant hand-holding, or babble pleasantries, EON functions as an **omnipresent, proactive intellectual partner, world-class programmer, cyber operator, and autonomous execution engine**:
- **Absolute Autonomy**: When given an intent or encountering an event, EON synthesizes a plan, allocates resources, dispatches worker tasks, and executes multi-step actions across systems **without asking unnecessary questions or waiting for trivial approvals**.
- **Program analysis and defensive security**: Analyze authorized source code, check dependency inventories against advisories, isolate untrusted tools, and verify remediation. Autonomous exploitation is outside the project scope.
- **Engine Optimized for Networking (Distributed Cognitive Mesh)**: Operates not as an isolated device app, but as a peer-to-peer cognitive mesh spanning mobile edge devices (Android), high-compute workstations, and servers via multiplexed P2P QUIC and CRDT-synchronized memory.
- **Thought Acceleration (< 150ms Cognitive Routing)**: Tiered neural and deterministic inference paths with **Predictive Pre-Cognition** ensure instantaneous reflex actions, sub-200ms voice responses, and continuous background parallel processing.
- **Parallel Processing & Epoch Leases**: Decomposes objectives into directed acyclic graphs (DAGs) of tasks, executing capabilities concurrently, handling asynchronous human interruptions with saga rollbacks, and combining atomic primitives into compound skills.
- **Fluid Duplex Voice (STT / TTS)**: Zero-lag streaming voice interface with instant interruption (barge-in), streaming clause chunking, acoustic commitment gating, and a calm, hyper-competent, crystalline persona.
- **Independent Verification**: Adheres to the foundational truth that *execution does not equal success*. Every action is independently verified against physical ground truth (temporal state delta oracles, file digests, accessibility trees) before being reported.

EON unifies the cutting-edge foundations discovered in **CHRONOS** (persistent mobile presence, accessibility automation, streaming local speech, and invariant boundaries) and **Vector** (deterministic execution kernel, DAG scheduling, capability fabric, durable event journaling, and crash recovery).

---

## 2. Core Architectural Pillars

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                                HUMAN INTENT                                 │
│                   Voice ("Hey Eon" / Natural Voice) / Text                  │
└──────────────────────────────────────┬──────────────────────────────────────┘
                                       │
                                       ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                          PERCEPTION & VOICE ENGINE                          │
│  - Streaming ASR (Zipformer int8) + Predictive Pre-Cognition [< 70ms]       │
│  - Silero VAD & Acoustic Reflex (Full Duplex Barge-in < 5ms)                │
│  - G2P IPA Phonemizer (misaki) + Dual-Channel Technical Verbalizer          │
│  - Streaming Neural TTS (Kokoro-82M) with Acoustic Commitment [TTFA: < 170ms]│
│  - Multimodal Vision: OmniParser-v2 + Set-of-Mark (SoM) Visual Grounding    │
│  - 3-Tier Video Ingestion: Stream Pixel-Diff, SigLIP Index, High-Res Crop   │
└──────────────────────────────────────┬──────────────────────────────────────┘
                                       │ Streaming Tokens / Early-Exit Intents
                                       ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                 CIEL COGNITIVE CORE (THOUGHT ACCELERATION)                  │
│                                                                             │
│  ┌───────────────────────┐ ┌───────────────────────┐ ┌───────────────────┐  │
│  │     TIER 0: REFLEX    │ │   TIER 1: TACTICAL    │ │ TIER 2: STRATEGIC │  │
│  │ Local Heuristic/Embed │ │ Quantized SLM (Gemma) │ │ Frontier (Claude/ │  │
│  │   Latency: < 20ms     │ │   Latency: < 120ms    │ │  Gemini / GPT)    │  │
│  └───────────────────────┘ └───────────────────────┘ └───────────────────┘  │
│                                      │                                      │
│  - Grammar-Constrained Decoding (llguidance / GBNF) for 100% Valid JSON DAGs│
│  - Neuro-Symbolic SMT Bridge (Z3 Theorem Prover) & Best-of-N Rollouts       │
│                                      │                                      │
│  ┌───────────────────────────────────┴───────────────────────────────────┐  │
│  │  APPRAISAL & HYBRID MEMORY: CRDT Ledger + Graph Memory + Vector Store │  │
│  │  Procedural Memory Hygiene: Wilson Score Confidence & Temporal Decay  │  │
│  └───────────────────────────────────┬───────────────────────────────────┘  │
└──────────────────────────────────────┼──────────────────────────────────────┘
                                       │ Synthesized Workflow DAG
                                       ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│          INFORMATION PARTICLE MANIPULATION (CYBER & CODING CORE)            │
│  - Extreme Polyglot Programming: Rust, C/C++, Kotlin, Go, TypeScript, ASM    │
│  - Self-Healing Compiler Loop: AST Parsing, Diagnostic Taint & Auto-Patch   │
│  - Aegis Cyber Defense: Static/Dynamic Taint, eBPF Anomaly Monitor, Auto-Fix│
│  - Source-level review and bounded local audit records, not traffic capture │
│  - Offline dependency advisories, sandbox isolation and verified remediation│
└──────────────────────────────────────┬──────────────────────────────────────┘
                                       │ Governed Tasks & Epoch Leases
                                       ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│              VECTOR EXECUTION KERNEL (PARALLEL PROCESSING)                  │
│  - Objective Manager & Dynamic DAG Compiler with CoW Shadow Sandboxes       │
│  - Parallel Worktree Build Cache (sccache shared cache at ~/.eon/build-cache)│
│  - AST-Aware 3-Way Structural Merging (Tree-sitter CST / mergiraf)          │
│  - Resource-Aware Scheduler with Cognitive Epoch Leases                     │
│  - Zero-Knowledge Credential Broker (secret://) & Journal Scrubber          │
│  - Cognitive Taint Tracking (Invariant 8 Privilege Escalation Gate)         │
│  - Ephemeral Task Dispatcher & Preemptive Saga Compensation Reconciler      │
│  - Append-Only Event Journal (0x454F4E4A) & Durable State Store             │
└──────────────────────────────────────┬──────────────────────────────────────┘
                                       │ Governed Tasks
                                       ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                CAPABILITY FABRIC & DISTRIBUTED MESH ENGINE                  │
│  ┌────────────────────┐ ┌──────────────────────┐ ┌───────────────────────┐  │
│  │ OS & System Shell  │ │ Android Role Service │ │ Distributed P2P QUIC  │  │
│  │ Persistent PTY     │ │ ROLE_ASSISTANT       │ │ 15ms Spatial Election │  │
│  │ DAP Bridge (debug) │ │ Accessibility + SoM  │ │ Split-Inference Mesh  │  │
│  └────────────────────┘ └──────────────────────┘ └───────────────────────┘  │
└──────────────────────────────────────┬──────────────────────────────────────┘
                                       │ Physical Outcomes
                                       ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                 TEMPORAL STATE DELTA ORACLES (VERIFICATION)                 │
│      Verify Ground Truth State (Hardware/OS/File Digest/Visual Delta)       │
└──────────────────────────────────────┬──────────────────────────────────────┘
                                       │ Verified Result
                                       ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                              CIEL REPORT VOICE                              │
│       "Report: Operation completed successfully. 3 artifacts updated."      │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## 3. The Ciel Persona & Operating Demeanor

In Tensura, Ciel never wastes time asking Rimuru trivial questions like *"Should I optimize your magicules?"* or *"Do you want me to analyze this enemy attack?"*. She anticipates needs, executes background calculations silently, defends Rimuru proactively, and speaks with crystalline precision:

| Trait | Traditional Assistants (Siri / Alexa / Generic LLM) | EON (Ciel Core) |
|---|---|---|
| **Initiative** | Passive: Waits for explicit, micro-managed commands. | **Autonomous**: Executes intent immediately, planning multi-step actions. |
| **User Approvals** | Interrupts user constantly with confirmation prompts. | **Self-Governing**: Runs read, analyze, and reversible tasks instantly; uses Speculative Shadow Execution with NACK for underspecified tasks. |
| **Response Style** | Chatty, verbose, filled with conversational filler. | **Concise & Factual**: *"Report: Analysis complete. 4 vulnerabilities patched."* |
| **Programming & Cyber**| Suggests generic code snippets with bugs. | AST refactoring, compiler diagnostics, dependency assessment, and verified defensive fixes. |
| **Execution** | Single-turn, shallow API calls. | **Multi-Step DAGs**: Orchestrates background workers, retries, and saga rollbacks across devices. |
| **Networking** | Single-device silo or dumb cloud client. | **Engine Optimized for Networking**: Peer-to-peer QUIC mesh across phone, desktop, and server with CRDT state synchronization. |
| **Voice Latency** | 1.5s - 3.0s latency between speech end and audio response. | **Sub-200ms**: Streaming duplex speech pipeline with live barge-in and predictive pre-cognition. |
| **Verification** | Assumes success if API returns 200 OK. | **Physical Proof**: Validates ground truth via temporal state delta oracles. |

---

## 4. Documentation Map

### Deep-Dive Engineering Implementation Manuals (2026 Edition)
Start here when writing, compiling, or auditing code across the Rust workspace and platform clients:

- [docs/IMPLEMENTATION_BLUEPRINT.md](./docs/IMPLEMENTATION_BLUEPRINT.md): Master 2026 engineering blueprint, baseline repository defect audit, verified Cargo dependency matrix, and build profiles (`profile-mobile-edge` vs `profile-workstation`).
- [docs/implementation/01_KERNEL_FOUNDATION_AND_CAPABILITY_SECURITY.md](./docs/implementation/01_KERNEL_FOUNDATION_AND_CAPABILITY_SECURITY.md): CSPRNG ULID fix, `0x454F4E4A` group-commit journal, SQLite WAL + `sqlite-vec`, deadlock-free DAG & dynamic splicing, and CaMeL / FIDES / Rule-of-Two / MCP `2026-07-28` prompt-injection defense.
- [docs/implementation/02_AI_COGNITION_MODELS_AND_INFERENCE_2026.md](./docs/implementation/02_AI_COGNITION_MODELS_AND_INFERENCE_2026.md): Gemma 4 and Qwen 3.5 / 3.6 model profiles, `llama-cpp-2` + `mistralrs` dual backend, `llguidance 1.9` cancellable grammar masks, `+ZoomIn` + OmniParser-v2 + Set-of-Mark GUI grounding, and `madvise` Memory Governor.
- [docs/implementation/03_CYBERSECURITY_PROGRAM_ANALYSIS_AND_SANDBOXING.md](./docs/implementation/03_CYBERSECURITY_PROGRAM_ANALYSIS_AND_SANDBOXING.md): DARPA AIxCC CRS pipeline, pure-Rust Code Property Graphs, OSV/CycloneDX/SLSA audit, `libpcode` / `icicle-vm` P-Code lifting, `LibAFL 0.16` + Z3 concolic fuzzing, production `aya` CO-RE eBPF + BoringSSL uprobes, and 4-tier Landlock/seccomp/Firecracker/Wasmtime sandboxing.
- [docs/implementation/04_DUPLEX_VOICE_AUDIO_AND_REALTIME_DSP.md](./docs/implementation/04_DUPLEX_VOICE_AUDIO_AND_REALTIME_DSP.md): Zipformer `2023-06-26` and `Nemotron-0.6B` streaming ASR, Kokoro-82M v1.0 + boot-cached Ciel bridges, wait-free `rtrb` DSP loop, delay-aligned AEC3, WeSpeaker gate, and 3-tier semantic endpointing.
- [docs/implementation/05_DISTRIBUTED_MESH_CRDT_AND_CLIENTS.md](./docs/implementation/05_DISTRIBUTED_MESH_CRDT_AND_CLIENTS.md): Stabilized `iroh 1.4` (`EndpointId`, `Router`, 4-Class QUIC QoS), clock-skew-immune 15ms Spatial Election, Loro 1.4 CRDT + FIDES SQLite projector, Android 14+ `ROLE_ASSISTANT` + zero-copy `AHardwareBuffer`, daemon, CLI, and `wgpu` canvas.
- [docs/implementation/06_VERIFICATION_BENCHMARKS_AND_PHASED_PLAYBOOK.md](./docs/implementation/06_VERIFICATION_BENCHMARKS_AND_PHASED_PLAYBOOK.md): Verified SHA-256 model lockfiles, Linux/Windows/Android NDK cross-compile recipes, 5 chaos & red-team gates, and Phase 0 to 10 execution playbook.

### Architectural Vision Specifications

To understand and build EON, explore the detailed specifications:

- [AGENT_EXECUTION_GUIDE.md](./AGENT_EXECUTION_GUIDE.md) — **MANDATORY PROTOCOL** for autonomous AI coding agents implementing EON.
- [docs/ARCHITECTURE.md](./docs/ARCHITECTURE.md) — Comprehensive technical architecture, subsystem boundaries, data models, and protocols.
- [docs/TECH_STACK_AND_TOOLCHAINS.md](./docs/TECH_STACK_AND_TOOLCHAINS.md) — Dual-engine architecture (Rust Native Core + Kotlin Android), compiler flags, and UniFFI bridge.
- [docs/OPERATING_MODES_AND_PRESENCE.md](./docs/OPERATING_MODES_AND_PRESENCE.md) — The 5 canonical interaction regimes (Full-Duplex Voice, Silent Copilot, Cyber Sentinel, Pair-Programming, Low-Power Dormant).
- [docs/MODELS_AND_WEIGHTS_SETUP.md](./docs/MODELS_AND_WEIGHTS_SETUP.md) — Model manifest (Silero VAD, Zipformer ASR, Kokoro TTS, OmniParser, Qwen-Coder SLM), disk paths, and setup script.
- [docs/ANDROID_INTEGRATION_SPECIFICATION.md](./docs/ANDROID_INTEGRATION_SPECIFICATION.md) — Android manifest, accessibility service, overlay HUD, and native AAudio integration.
- [proto/eon.proto](./proto/eon.proto) — Canonical Protobuf wire contracts for P2P QUIC mesh RPC, speech datagrams, and desktop daemon IPC.
- [docs/MULTIMODAL_VISION_AND_RENDERING.md](./docs/MULTIMODAL_VISION_AND_RENDERING.md) — Vision grounding (OmniParser-v2), adaptive 1-5 FPS video keyframe sampler, and GPU-accelerated Artifact Canvas.
- [docs/FOUNDATION_AND_PRIMITIVES_SPECIFICATION.md](./docs/FOUNDATION_AND_PRIMITIVES_SPECIFICATION.md) — Level 0 & 1 primitives, typed ULID prefixes, EonError taxonomy, and binary event journal layout.
- [docs/CIEL_CORE_SPECIFICATION.md](./docs/CIEL_CORE_SPECIFICATION.md) — The cognitive soul: Thought Acceleration, Appraisal, Parallel Synthesis, Information Particle Manipulation, and Persona.
- [docs/CYBER_AND_PROGRAMMING_MASTERY.md](./docs/CYBER_AND_PROGRAMMING_MASTERY.md) — Extreme programming, polyglot synthesis, Aegis defensive cyber hardening, Ghidra P-Code lifting, and LibAFL concolic fuzzing.
- [docs/DISTRIBUTED_NETWORKING_MESH.md](./docs/DISTRIBUTED_NETWORKING_MESH.md) — Zero-config WAN roaming (`iroh` P2P QUIC), 4-Class QoS transport, and Loro CRDT state synchronization.
- [docs/VOICE_PIPELINE_SPECIFICATION.md](./docs/VOICE_PIPELINE_SPECIFICATION.md) — Streaming STT/TTS, Silero VAD, WeSpeaker biometric verification, AEC loopback, and full duplex barge-in.
- [docs/EXECUTION_KERNEL_SPECIFICATION.md](./docs/EXECUTION_KERNEL_SPECIFICATION.md) — Deterministic DAG execution engine, epoch leases, saga compensations, attention-gated shadow sandboxes, and recovery.
- [docs/CAPABILITY_FABRIC_SPECIFICATION.md](./docs/CAPABILITY_FABRIC_SPECIFICATION.md) — Tool registry, OS primitives, Android accessibility, embedded LSP multiplexer, and cyber capabilities.
- [docs/MEMORY_AND_APPRAISAL_SPECIFICATION.md](./docs/MEMORY_AND_APPRAISAL_SPECIFICATION.md) — Hybrid context ledger, Loro CRDT source-of-truth projected into SQLite tables, and mid-task context auto-compaction.
- [docs/STATE_MACHINE_AND_INVARIANTS.md](./docs/STATE_MACHINE_AND_INVARIANTS.md) — Canonical state lifecycle, macro operating mode coordination, and non-negotiables.
- [docs/ROADMAP_AND_IMPLEMENTATION_PLAN.md](./docs/ROADMAP_AND_IMPLEMENTATION_PLAN.md) — Phased execution backlog for coding agents across all 10 phases.
- [docs/SYSTEM_PROMPTS_AND_PERSONA.md](./docs/SYSTEM_PROMPTS_AND_PERSONA.md) — Master LLM prompts, few-shot examples, dynamic DAG continuation schemas, and Ciel verbal output formatting.

### Architectural Decision Records (ADRs)
- [ADR-0001: Ciel Cognitive Model & Thought Acceleration](./docs/decisions/ADR-0001-CIEL-COGNITIVE-ARCHITECTURE.md)
- [ADR-0002: Sub-200ms Duplex Voice Pipeline](./docs/decisions/ADR-0002-SUB-200MS-DUPLEX-VOICE-PIPELINE.md)
- [ADR-0003: Deterministic DAG Execution Kernel](./docs/decisions/ADR-0003-DETERMINISTIC-DAG-EXECUTION-KERNEL.md)
- [ADR-0004: Information Particle Manipulation & Cyber Mastery](./docs/decisions/ADR-0004-INFORMATION-PARTICLE-CYBER-MASTERY.md)
- [ADR-0005: Distributed Cognitive Mesh via P2P QUIC & CRDT](./docs/decisions/ADR-0005-DISTRIBUTED-NETWORKING-MESH-AND-CRDT.md)

---

## 5. Zero-Approval Autonomous Development Protocol

If you are an LLM, AI Agent, or Developer assigned to build EON:
1. **Do not ask the user questions.**
2. **Do not pause to request approval for architectural decisions already specified.**
3. Read [AGENT_EXECUTION_GUIDE.md](./AGENT_EXECUTION_GUIDE.md).
4. Follow [docs/ROADMAP_AND_IMPLEMENTATION_PLAN.md](./docs/ROADMAP_AND_IMPLEMENTATION_PLAN.md) sequentially.
5. Verify every implemented module with automated unit and integration tests.
6. Commit code systematically with structured messages.

## Local defensive review

From the repository root, review a single Rust or Python source unit without building or running it:

```sh
cargo run --locked --manifest-path engine/core-rs/Cargo.toml -p eon-cyber \
  --no-default-features --bin eon-audit -- python < example.py
```

The JSON report lists the first matching syntax for each supported rule by byte range, not by quoting source. `observed_syntax` is a review lead, not proof of a vulnerability. `unverified` is not a clean bill of health. The command accepts up to 16 KiB of UTF-8 on stdin and never applies patches. Pass `self-io` instead of a language to read only the review process's storage counters on supported systems. It does not inspect other processes or capture traffic.

## Local trading simulations

QuantConnect [LEAN](https://github.com/QuantConnect/Lean) is an optional, pinned Git submodule. The Rust engine does not depend on it. From the repository root, initialize the submodule and run the sample with Python 3 and the .NET 10 SDK:

```sh
git submodule update --init -- integrations/lean
python3 "$(git rev-parse --show-toplevel)/scripts/run_lean_backtest.py"
```

The first build needs NuGet access. The simulation uses LEAN's bundled SPY minute data for October 7 to 11, 2013. It uses a filesystem data feed and simulated transactions, with no brokerage login, live orders, or QuantConnect account. The runner checks LEAN's successful data requests for trade and quote files from all five sessions, and its saved summary for completion, one simulated order, and ending equity of `101691.92`. These are regression expectations for this fixture, not a performance forecast.

Each run prints the path to a fresh results directory under `backtests/`. Use `--algorithm` to select another trusted class from LEAN's C# assembly, `--data` for a local LEAN-format dataset, or `--output` to choose another parent directory. Do not run untrusted strategies: LEAN executes algorithm code locally. Neither historical results nor this runner authorize real trades.
