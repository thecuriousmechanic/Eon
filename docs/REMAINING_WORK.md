# Remaining work

This lists what is not built yet, by phase, checked against the status rows in `docs/IMPLEMENTATION_BLUEPRINT.md` §8. Each row there says what is built and how it is tested. This file only lists what is left.

## Phase 2 and 3: kernel and shadows

- Structural (CST) merge. The function-level merge in `eon-kernel::structural` is built, but two edits to one function always conflict. mergiraf is blocked by a `termcolor` version conflict with wasmtime.

## Phase 4: sandboxing

- Rich tool I/O and the WebAssembly component model.

## Phase 5: voice

- Wire the speech-onset detector into the live device loop. It is tested on the loopback only.
- Measure playback-path latency on a real device.
- WeSpeaker owner enrollment flow, and the 1.8 ms speaker-gate budget (not measured).
- Speaker separation is measured on a two-way split of one recording, which is weaker than two recordings.

## Phase 6: cognition

- mistral.rs backend.
- KV pre-cognition.
- OmniParser (AGPL, opt-in only).
- Pre-warm the model at daemon boot. `LocalModel::warm_up` exists, but the daemon never builds a `LocalModel`.
- Prefix cache eviction and multi-prefix pools.

## Phase 7: cyber and sandboxing

- LibAFL.
- The p-code lifter (libpcode or icicle-vm). Needed for the Z3 overlay on CFG edges and the P-code CFG pane.
- Live OSV fetch and version-range resolution for SBOM matching. Offline matching is built.
- Code graph for languages other than Rust and Python.
- Receiver types for method calls (`x.save()` resolves by name only). Glob imports (`use a::*`), nested use groups, and module trees beyond the file stem.
- Value-level taint tracking. Call edges carry no values today.
- Firecracker: a block-device rootfs, networking, jailer confinement, and wiring microVMs into the `dispatch_guarded` tool path.
- eBPF: BTF CO-RE loading for other kernels, per-process filtering, enforcement (blocking calls), and running the probe from the daemon.

## Phase 8: mesh

- Relay-backed dialing. Relays are disabled on purpose today, so this needs a decision on which relays to trust.
- Relay or DNS address lookup, mDNS (`iroh-mdns-address-lookup`), and key rotation.
- Partition healing, adaptive scheduling, and backoff for anti-entropy rounds.
- The 25% link-rate pacer is on the custom chunked transfer path only. The iroh-blobs path is not paced.

## Phase 9: canvas and CLI

- The p-code CFG pane and the Z3 overlay on CFG edges (both need the p-code lifter).
- DAP instruction-pointer highlight (needs a DAP server).
- Move `engine/core-rs/crates/eon-canvas` to `clients/canvas` as the blueprint layout specifies.

## Phase 10: Android

- The application module: AAR packaging, `ROLE_ASSISTANT`, MediaProjection foreground service, AAudio, Compose HUD, and AHardwareBuffer.
- Needs a JDK, the Android SDK, and a device or emulator. The Gradle bindings module (`engine/android/core-bindings`) builds and tests on the JVM.

## Cross-cutting

- Lean integration. Deferred for a separate discussion.
- Playback flake in `eon-voice` `device_loop`: a steady-state ALSA error still appears in roughly one workspace run in six or seven. The startup retry covers only startup failures.
