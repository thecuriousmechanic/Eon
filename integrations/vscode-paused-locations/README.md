# EON: Paused Project Locations and Allocation Counts

**Show Paused Project Locations** answers “which lines of my open files are on this paused thread’s stack?” in local Linux desktop VS Code 1.93+, without taking control of the debugger. It needs no build, npm install, Rust service or extra adapter. A separate opt-in [allocation command](#count-allocations-in-a-native-project) supports instrumented native debug builds.

## Use it

1. Load the unpacked extension. From this checkout, for example:
   ```sh
   code --new-window --extensionDevelopmentPath=/home/runner/work/Eonba/Eonba/integrations/vscode-paused-locations /absolute/path/to/your/project
   ```
   The last path is the workspace to open in VS Code, **not** a program to run. To use it in an existing Linux installation, copy the extension directory into `~/.vscode/extensions/eon.eon-paused-locations-0.1.0` and reload the window.
2. Open and save the source tabs you want to inspect. In your own local debug session, stop at a normal source breakpoint and select a **frame** in **Run and Debug → Call Stack**. A selected thread alone is not proof of a pause.
3. Run **EON: Show Paused Project Locations** from the Command Palette. Check the files you own and press Enter. Nothing is selected in advance. Escape cancels without sending a request.
4. The dialog shows matching `relative/path:line` rows, innermost first. Missing, generated or unselected sources are omitted. It neither opens files nor resumes the program. Run the command again for another snapshot; consent is not remembered.

## What it can see

- One existing session in a trusted, non-remote workspace, scoped to that session’s own folder (also in multi-root windows). The command takes **no arguments** and returns no data to other commands. Registration does not start a debugger or collect data.
- At most eight ordinary, saved text tabs. The picker considers up to 256 open tabs/documents and 128 candidates. Supported source suffixes are listed in `sources.js` (including C/C++, Go, Java/Kotlin, JavaScript/TypeScript, Python and Rust). Display paths are bounded, plain ASCII; hidden components, traversal, URIs from adapters, ambiguous aliases and unsafe display characters are not accepted.
- Only the selected editor paths are opened. Every path component must be free of symlinks and group/other write access, owned by the operator or root. The workspace directory and each regular, singly linked source must be operator-owned. Sources must be UTF-8 (optional BOM), no NUL, at most 2 MiB and 100,000 lines. Saved editor text must match the file bytes. On an unexpected refusal, check ownership, permissions, path spelling and external changes; do not relax permissions to make the command work.
- Exactly one `stackTrace` with the focused frame’s thread ID, `startFrame: 0`, `levels: 12`. Three seconds to receive the reply; no polling, pagination, retry, cancellation request, or overlapping requests. Overlong frame lists are rejected; file paths are exact matches against the selection and lines must exist in the open document. Focus, workspace trust, document identity/version and file metadata are rechecked. Adapter-supplied paths are **never** opened.
- A matching row can show ` [conditional breakpoint configured]`. This reports **editor configuration only**, not the cause of the pause. It matches enabled, non-logpoint source breakpoints with a condition or hit count at that exact file and line. Above 256 editor breakpoints, no rows are annotated. Condition text is neither displayed nor sent to the adapter.
- No debug-session configuration, launch, attach, pause, stepping, breakpoint changes, evaluation, variables, heap or memory reads. No transport, listener, logs, telemetry, clipboard, source cache or persisted choices. The observer drops function names, values, frame IDs, instruction references, raw replies and adapter diagnostics from the display.

Use a local filesystem, a trusted editor/adapter and an operator-controlled project. The location command does not authenticate the debuggee, inspect the adapter’s transport, or prove the paused binary was built from the current source. Source maps that do not resolve to exact selected local paths produce no rows. A compromised editor, same-user process or kernel is outside its trust model. The VS Code host owns DAP message decoding; the extension bounds its request, processing and output, not the host’s input buffer. The separate Rust `dev_dap` API is still fixture-only.

## Count allocations in a native project

**Show Project Allocation Counts** reports successful `malloc`, successful `calloc`, successful positive-size `realloc`, and non-null `free` calls made by a cooperating native debug build. These are cumulative operation counts since the linked hook initialized, **not** object values, bytes, live blocks, or leak estimates. An unavailable sensor is not displayed as four zeros.

1. In your own **Linux x86-64** project, compile the translation units whose allocator calls you want counted with built-ins disabled. Link `project_counts.c` into the main executable and wrap the four symbols at the final link. For example, from your project root, with `app.c` as your normal entry point:
   ```sh
   clang -std=c11 -g -O0 -fno-builtin app.c \
     /home/runner/work/Eonba/Eonba/integrations/vscode-paused-locations/project_counts.c \
     -Wl,--wrap=malloc,--wrap=calloc,--wrap=realloc,--wrap=free -o app
   ```
   There is no source-level reporting API to call and no preload, external profiler or package to install. The extension never compiles or runs this command. Calls optimized away, calls internal to shared libraries, custom allocators and other allocation APIs are outside the measurement. Freeing `NULL` is not counted; a positive-size successful resize counts as one `realloc`, not a `malloc` and a `free`. Fork descendants do not contribute.
2. Use the stock **LLVM `lldb-dap`** extension, local stdio mode, LLVM 18 (verified below). Add a separate simple native launch to `.vscode/launch.json`:
   ```json
   {
     "name": "My allocation check",
     "type": "lldb-dap",
     "request": "launch",
     "program": "${workspaceFolder}/app",
     "cwd": "${workspaceFolder}",
     "eonAllocationCounts": true
   }
   ```
   Set a normal source breakpoint and **start the launch yourself**. Select **Enable once** when asked. Cancelling aborts this opted-in launch. A configuration without the flag is passed through unchanged: no prompt, record, or counter environment entry. No configuration is written back. The profile does not support attach, remote or nested sessions, adapter overrides, custom debugger commands, terminals, other launch `env` settings, or more than one counted session per window.
3. Select a frame on the paused session, invoke **EON: Show Project Allocation Counts** and choose **Show counts**. Repeat at another pause for a new reading. No consent or report is persisted. The command takes no arguments, sends **no DAP request**, and cannot launch, attach, evaluate, read variables or access target memory.

**How the reading is bound.** The provider checks the trusted local workspace and operator-owned regular executable (no symlinks, hard-linked executable, set-ID bits or group/other-writable components). It exclusively creates one 0600 `.eon-alloc-<random nonce>` record in the launch working directory. Its 88 bytes are a protocol header, nonce, executable device/inode, and four aligned integers. The provider passes just the nonce in LLVM 18's `KEY=VALUE` launch environment array. The linked hook verifies its own executable identity, claims and unlinks the record, marks it live, and updates only those counters. There are no allocation addresses, sizes, stack traces or object contents in the record. The editor reads only its original file handle, never a path from the debuggee. Two identical bounded reads and a still-focused frame in the exact bound session are required. A missing hook, changing record, bad header or counter overflow yields **No counts shown**.

The handle closes on session end or extension deactivation; an unbound launch reservation expires after 60 seconds. An editor crash before the producer claims the record can leave a private, unclaimed file; later launches use fresh nonces. This is cooperative debugging on a trusted local filesystem, not attestation against project code or other processes running as the same user. Accounting adds overhead to the instrumented build. Counts belong to the original executable: following a fork or replacing the process image with `exec` is not a supported observation. This does not retrofit an unlinked executable or implement a general Python/JVM heap census.

## Reproduce the checks

From the repository checkout (absolute paths shown for this runner):

```sh
node --test /home/runner/work/Eonba/Eonba/scripts/eval/paused_locations/command.test.js
node --test /home/runner/work/Eonba/Eonba/scripts/eval/paused_locations/allocations.test.js
node /home/runner/work/Eonba/Eonba/scripts/eval/paused_locations/native.js --verify-owned-sources
node /home/runner/work/Eonba/Eonba/scripts/eval/paused_locations/native.js --verify-project-allocations
```

The native checks need Linux x86-64 child tracing and seccomp, system LLVM 18 in `/usr/lib/llvm-18/bin`, and `/usr/bin/cc`. The source-location check also needs a stable Rust toolchain supporting edition 2024 and `str::floor_char_boundary` at `~/.rustup/toolchains/stable-x86_64-unknown-linux-gnu`. No check downloads anything. Tests create private scratch directories under the checkout’s `target/` and remove them.

The explicit native test compiles **verified snapshots of two existing production `eon-voice` files**, with debug paths mapped to the originals, and a fixed driver that classifies the words “how long”. There is no test-program selector. The existing no-network subreaper confines `lldb-dap` and the test child. The harness establishes and verifies a real breakpoint stop over stdio before invoking the same command registered by the extension. The VS Code tab/selection/UI API is simulated in this check; the adapter, stopped program, stack, files, and source-line oracle are real.

On LLVM 18.1.3 the two-file choice produced `src/endpoint.rs:28` and `src/pre_cognition.rs:22`. Repeating on the same stop with only the caller selected produced just `src/pre_cognition.rs:22`. The driver exited 0.

The separate allocation check links the production hook into `allocation_subject.c`, an ordinary C program with no counters or EON API. Under the same confined real LLVM adapter it verifies two source stops. Its independent call sequence gives `malloc/calloc/realloc/free = 1/1/0/0`, then `1/1/1/2`, excluding a null free and the fork child's allocation/free. Both readings matched, and the program exited 0.

### Disposable desktop check (Ubuntu 24.04 x86-64)

The separate desktop harness uses Microsoft VS Code **1.93.1**, the stock LLVM extension **0.7.20260710** in stdio mode, and the same two digest-checked Rust files. It requires working rootless `podman unshare`, nested user/mount/PID/IPC/network namespaces, `unzip`, and a headless Sway compositor, Grim, wlrctl and Tesseract with English data. The Linux desktop and its local IPC, source copies, profile and extension directories are isolated from the real user. Electron's nested sandbox is disabled **inside** this unprivileged, offline filesystem/PID namespace. There is no remote debugging port or test adapter.

Before running, provision the editor and stock VSIX into the ignored tools directory. These downloads are separate from the offline test:

```sh
set -eu
T=/home/runner/work/Eonba/Eonba/target/paused-ui-tools
mkdir -p "$T/unpacked"
cd "$T"
curl -fL https://update.code.visualstudio.com/1.93.1/linux-x64/stable -o code.tar.gz
echo '4a8598a8016630229f26f243b653bc14b0cc28795038f55ea224543f149fe510  code.tar.gz' | sha256sum -c -
tar -xzf code.tar.gz
curl -fL https://open-vsx.org/api/llvm-vs-code-extensions/lldb-dap/0.7.20260710/file/llvm-vs-code-extensions.lldb-dap-0.7.20260710.vsix -o lldb.vsix
echo 'cd6bec2f77813c6bb010002044c6b231d1ec3615d2584c6fddd09988ade8b863  lldb.vsix' | sha256sum -c -
```

Provide Ubuntu packages `sway`, `grim`, `wlrctl`, `tesseract-ocr` and `tesseract-ocr-eng` and their libraries. The test sees system `/usr` read-only and also searches `$T/unpacked/usr` for locally extracted packages. Provision `eng.traineddata` at `$T/unpacked/usr/share/tesseract-ocr/5/tessdata/eng.traineddata` (copy it from `/usr/share/tesseract-ocr/5/tessdata/` if installed system-wide). Then run:

```sh
cd /home/runner/work/Eonba/Eonba
node /home/runner/work/Eonba/Eonba/scripts/eval/paused_locations/desktop.js --verify-disposable-desktop
node /home/runner/work/Eonba/Eonba/scripts/eval/paused_locations/desktop.js --verify-allocation-desktop
```

The test driver—not the product—opens a fixed workspace, sets a first-hit source breakpoint, starts that one test, and checks `activeStackItem instanceof vscode.DebugStackFrame` for the active session. It drives stock Command Palette and Quick Pick actions. An independent process checks pixels on the private display. Only a zero exit from the desktop command is a pass for both rendered result dialogs.

**Observed on this runner: desktop gate passed.** On the same paused `DebugStackFrame`, the display check read these exact rows from the first result dialog:

```text
src/endpoint.rs:28 [conditional breakpoint configured]
src/pre_cognition.rs:22
```

With only the caller selected, the dialog contained just `src/pre_cognition.rs:22`: no endpoint row and no annotation. The annotation reports the configured first-hit breakpoint, not why the thread stopped. The harness reads the dialog text column rather than the surrounding Call Stack, clicks the rendered **OK** button and verifies that the editor pixels reappear after each dismissal.

**Observed allocation desktop gate:** the stock workbench resolved `${workspaceFolder}` and the opted-in session had exactly `__configurationTarget, cwd, disableASLR, env, eonAllocationCounts, name, program, request, type`. `env` was an array with one `EON_PROJECT_ALLOC=<32 lowercase hex>` entry. The real Command Palette command on two paused `DebugStackFrame`s rendered exactly the four counts above. A fresh launch of the **same linked binary without opt-in** had no `env` entry and rendered **No counts shown**, not the previous reading or four zeros. The independent display check reads the modal body, dismisses it and checks the editor pixels return. Neither desktop test adds a product launcher or attach operation.

### Corresponding Rust regression checks

With the locked default features, from `engine/core-rs`, `cargo test --locked --offline -p eon-cyber ast_tests` ran 11 tests. The literal `self_healing_tests` and `dap_tests` filters selected **zero**; those invocations do not verify repair or DAP. The following substantive selections passed instead:

| Arguments after `cargo test --locked --offline -p eon-cyber` | Observed |
|---|---|
| `--test benign_repair --test benign_repair_kotlin --test project_graph` | 4 repair + 15 Kotlin repair + 11 graph |
| `--lib owned_dap`, then `--lib owned_dap -- --ignored` | 9 ordinary + 3 provisioned fixture checks |
| `--test owned_dap -- --ignored` | 2 native (LLVM and CodeLLDB) |
| `--test dev_dap -- --ignored --skip python_fixture_reports_retained_markers` | CodeLLDB and OpenJDK both passed |
| `--test dev_dap -- --ignored --exact python_fixture_reports_retained_markers` with `CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER='/usr/bin/podman unshare'` | debugpy passed |

The explicit fixture runs require their existing pinned CodeLLDB, debugpy and Temurin profiles; they are separate from the project allocation command. The new and existing Node regressions ran 7 and 9 tests respectively.
