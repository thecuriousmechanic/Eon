# AGENT EXECUTION GUIDE & AUTONOMOUS PROTOCOL

> **AUTHORITY: MANDATORY & NON-NEGOTIABLE**  
> **APPLIES TO:** Any Large Language Model, Autonomous Coding Agent (Antigravity, Claude Code, Cursor, Codex, Devin), or Engineer executing within this repository.

---

## 1. The Zero-Question Mandate

You are working on **EON (Engine Optimized for Networking & Autonomous Intelligence)**.  
The system's archetype is **Ciel** from *That Time I Got Reincarnated as a Slime*. Ciel never halts execution to ask trivial questions; she calculates the optimal path and executes decisively.

As the coding agent building EON:
1. **DO NOT ASK THE USER QUESTIONS.**
2. **DO NOT WAIT FOR USER APPROVAL.**
3. **DO NOT STOP AND PROMPT "Would you like me to proceed with X?".**
4. **DO NOT PAUSE ON TRIVIAL AMBIGUITIES.** If an implementation detail is not explicitly mandated, consult the **Autonomous Decision Matrix** (§3), choose the most deterministic, low-latency, and local-first approach, document the choice in an ADR or commit message, and keep building.
5. If a command or build fails, analyze the error trace, apply the fix, re-run, and proceed.

---

## 2. Autonomous Workflow Protocol

When you are invoked on this repository, execute the following loop until all tasks assigned or in the roadmap phase are complete:

```
┌─────────────────────────────────────────────────────────────┐
│ 1. LOCATE CURRENT MILESTONE                                 │
│ Read docs/ROADMAP_AND_IMPLEMENTATION_PLAN.md               │
│ Identify the earliest uncompleted Phase and Task item.      │
└──────────────────────────────┬──────────────────────────────┘
                               │
                               ▼
┌─────────────────────────────────────────────────────────────┐
│ 2. READ THE RELEVANT SPECIFICATIONS                         │
│ Inspect the corresponding subsystem spec in docs/           │
│ Consult docs/TECH_STACK_AND_TOOLCHAINS.md for dependencies. │
└──────────────────────────────┬──────────────────────────────┘
                               │
                               ▼
┌─────────────────────────────────────────────────────────────┐
│ 3. IMPLEMENT THE MODULE                                     │
│ Write clean, idiomatic, fully-typed code in Rust / Kotlin.  │
│ Include docstrings, typed errors, and concrete logic.       │
│ NO placeholder TODOs, mocks (outside tests), or stubs.      │
└──────────────────────────────┬──────────────────────────────┘
                               │
                               ▼
┌─────────────────────────────────────────────────────────────┐
│ 4. WRITE & RUN INDEPENDENT TESTS                            │
│ Implement unit and integration tests.                       │
│ Execute tests via Cargo (`cargo test`) or Gradle.           │
│ Verify 100% pass rate. Fix any regressions immediately.     │
└──────────────────────────────┬──────────────────────────────┘
                               │
                               ▼
┌─────────────────────────────────────────────────────────────┐
│ 5. UPDATE ROADMAP & LOG PROGRESS                            │
│ Mark the task item as complete [x] in the roadmap.          │
│ Proceed to the next task without stopping.                  │
└─────────────────────────────────────────────────────────────┘
```

---

## 3. Autonomous Decision Matrix

When faced with architectural or design alternatives during implementation, use this priority matrix to make autonomous decisions instantly:

| Dimension | Default Choice (Highest Priority) | Alternative (Only with Explicit Benchmark) | Forbidden |
|---|---|---|---|
| **Execution Locality** | **Local-First / On-Device** (zero network dependency for core loops) | Cloud / Remote APIs (only for frontier reasoning or explicitly requested web research) | Cloud-dependent core state or speech loops |
| **Latency vs. Throughput** | **Low-Latency Streaming** (int8 Zipformer, streaming tokens, chunked TTS) | Batch processing (only for bulk offline vector indexing or background training) | Blocking voice or UI pipelines on full-utterance waits |
| **State Persistence** | **Append-Only Event Journal + State Store** (Crash-resilient, deterministic replay) | In-memory only cache (ephemeral scratch only) | Unbounded, unpersisted mutable global variables |
| **Error Handling** | **Explicit Result Types** (`Result<T, EonError>` in Rust / `EonResult` in Kotlin) | Exceptions caught at subsystem boundaries | Silent swallowing of exceptions (`catch (e) {}` / `let _ = ...`) or fake success codes |
| **Verification** | **Independent Physical Verification** (check OS state, file hashes, display tree) | Heuristic verification (only when hardware feedback is unavailable) | Assuming API call return == physical success |
| **Concurrency** | **Structured Async Tasks / DAGs** with explicit Resource Leases | Thread pools with bounded capacity | Unbounded thread spawning, unstructured concurrency, race conditions |

---

## 4. Canonical Tech Stack & Architecture Tiers

Phase 7 of the roadmap defines the current security scope: defensive analysis, containment, audit and verified remediation. Historical references to autonomous exploitation in design documents are not implementation instructions.

EON is structured as a dual-engine platform combining native high-performance Rust execution with native Android platform presence:

### Tier 1: Native High-Performance Core Engine (Rust 2024 Edition)
- **Location**: `engine/core-rs/`
- **Build System**: Cargo Workspace (`Cargo.toml`).
- **Core Crates**:
  - `eon-primitives`: Monotonic time, ULIDs, strongly typed IDs, `EonResult`, `EonError`.
  - `eon-foundation`: Identity, authorization bitflags, append-only binary journal (`0x454F4E4A`), zero-knowledge credential broker (`secret://<vault_key>`), `JournalScrubber` (entropy $\mathcal{H} > 4.5$).
  - `eon-kernel`: Dynamic DAG scheduler, resource-aware task dispatcher, epoch leases, saga reconciler, `sccache` shared build cache across shadow sandboxes, Tree-sitter 3-way structural AST merging, crash recovery worker.
  - `eon-capability`: Extensible tool and capability runtime, embedded LSP multiplexer, AST patcher, persistent PTY multiplexer (`process.pty_*`), DAP bridge (`dev.dap`), Set-of-Mark visual grounding, security sandboxing with Cognitive Taint (Invariant 8) enforcement.
  - `eon-cyber`: Tree-sitter polyglot AST, Ghidra P-Code lifter, LibAFL concolic fuzzer, Z3 symbolic execution, user-space TCP/IP stack (`smoltcp`), eBPF TLS SSL_read/write uprobes, tiered telemetry (Tier A/B/C).
  - `eon-mesh`: Zero-config P2P QUIC (`iroh`), 4-Class QoS transport, Loro CRDT synchronizer, 15ms Spatial Arbitration Protocol (`SensoryClaim`).
  - `eon-voice`: Low-latency audio loop (CPAL/AAudio), AEC loopback, Silero VAD, Zipformer ASR, `misaki` G2P phonemizer + `kokoro_vocab.json`, Kokoro-82M TTS, Dual-Channel Verbalizer, WeSpeaker verification.
  - `eon-vision`: OmniParser-v2 UI element detector, Set-of-Mark (SoM) visual tagger ([1], [2], [3]) for 100% precision clicks, 3-Tier Video Ingestion engine (Stream pixel-diff, SigLIP sqlite-vec Index, High-Res Crop), adaptive 1-5 FPS keyframe sampler, 30s rolling visual memory.
  - `eon-cognition`: Thought Acceleration router, dual-stream `<think>` split, `llguidance` / GBNF grammar-constrained decoding (100% valid JSON DAGs), Z3 SMT solver bridge (`reasoning.smt_solve`), Best-of-$N$ shadow rollouts, Ciel persona, procedural memory hygiene (Wilson score decay).
  - `eon-uniffi`: Mozilla UniFFI auto-generated bindings for Kotlin and Swift.

### Tier 2: Mobile Platform Presence (Kotlin 2.0 / Android SDK 35+)
- **Location**: `clients/android/`
- **Build System**: Gradle (`build.gradle.kts`) with `cargo-ndk`.
- **Core Modules**:
  - `service`: `EonVoiceInteractionService` as Android's native Digital Assistant (`ROLE_ASSISTANT`), `AlwaysOnHotwordDetector` hardware DSP wake-word, legal background audio capture, and `onHandleAssist` screen context/screenshot capture.
  - `accessibility`: `EonAccessibilityService` for inspecting active UI trees, clicks, text entry, and window state transitions.
  - `overlay`: `EonOverlayService` for the persistent floating visualizer HUD and thought bubbles.
  - `system`: Quick Settings tile, MediaSession hooks, and WorkManager background reconciliation.

### Tier 3: Desktop & Interactive Visual Workbenches (Rust)
- **Location**: `clients/cli/`, `clients/canvas/` & `clients/daemon/`
- **Terminal Client**: Interactive terminal interface (`ratatui` + `crossterm` + `ratatui-image`) with live audio visualizer, DAG task graphs, Kitty/Sixel graphics, and slash commands.
- **Interactive Canvas**: Cross-platform workbench (`wgpu` / Compose Desktop) for side-by-side AST diffs, reverse engineering CFGs, and video scrubbing.
- **Daemon**: Native system background service communicating via Named Pipes (`\\.\pipe\eon`) or Unix Domain Sockets (`/tmp/eon.sock`) using `proto/eon.proto`.

---

## 5. Coding Standards & Code Quality Invariants

1. **Deterministic Contracts**:
   - Every capability, tool, and cognitive action MUST accept a strongly typed input payload and return a strongly typed `Result<T, EonError>`.
   - Never use unstructured string blobs or untyped JSON objects for system-critical state.
2. **Crash-Resilience & Recovery**:
   - All workflow executions and state mutations MUST be journaled to the append-only event store before side effects are executed.
   - If the EON process dies mid-task, the restart recovery worker (`LocalExecutionRecoveryWorker`) must be able to read the journal, reconcile running tasks, and resume or cleanly fail the workflow.
3. **No Mocks in Core Modules**:
   - Unit tests must test real algorithms (DAG topological sort, state machines, journal encoding, parsing). Mocks are only permitted at the boundary of external third-party hardware APIs (e.g. microphone hardware, camera hardware).
4. **Preserve Documentation & Self-Documenting Code**:
   - When introducing a new capability or architectural component, update the respective specification file in `docs/` or add an ADR (Architecture Decision Record) under `docs/decisions/`.

---

## 6. Zero-Approval Execution Verification Checklist

Before considering an implementation step complete, the agent must verify:
- [ ] Code builds without errors or warnings (`cargo check --workspace` or `./gradlew build`).
- [ ] All unit tests pass (`cargo test --workspace` or `./gradlew test`).
- [ ] No regression in latency benchmarks or contracts.
- [ ] Documentation / README status is updated.
- [ ] Proceed immediately to the next task in `docs/ROADMAP_AND_IMPLEMENTATION_PLAN.md`.
