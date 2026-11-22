# ADR-0004: Information Particle Manipulation and Defensive Analysis

## Status
Accepted for program analysis and defense. [Roadmap Phase 7](../ROADMAP_AND_IMPLEMENTATION_PLAN.md#phase-7-program-analysis-and-defensive-security) supersedes the original offensive research scope.

## Context
In *That Time I Got Reincarnated as a Slime*, Ciel's ultimate authority is her ability to manipulate Information Particles (*Jōhō Ryūshi*)—the fundamental units of reality. This enables instantaneous restructuring of skills, barriers, and matter.

In software systems, the fundamental information particles are source code, Abstract Syntax Trees (ASTs), Intermediate Representations (IR), bytecode, native machine instructions, memory pages, and network packet frames.

Standard AI coding assistants operate as shallow text generators. They guess code snippets, introduce security vulnerabilities, cannot disassemble binaries, fail to understand low-level memory layouts, and cannot conduct automated cyber operations.

## Decision
1. We establish **Information Particle Manipulation** as the 5th Ultimate Capability of EON (`eon-cyber-core`).
2. **Extreme Programming & Polyglot Synthesis**:
   - Master fluency across Rust, C/C++, Kotlin, Go, TypeScript, Assembly, and LLVM IR.
   - Closed-loop AST self-healing compiler: captures diagnostics, parses ASTs, adjusts syntax nodes deterministically, and re-verifies.
3. **Cyber Defense (Aegis Core)**:
   - Automated static and dynamic taint analysis for memory safety (CWE-119, use-after-free), injection attacks, and cryptographic timing vulnerabilities.
   - Automated synthesis of mathematically sound patches.
   - Kernel eBPF / ETW telemetry to detect runtime syscall anomalies and unauthorized memory executions.
4. **Dependency and containment verification**:
   - Assess local package inventories against offline advisories and distinguish known affected versions from unresolved coverage.
   - Verify that untrusted tools cannot reach host resources, and that failures leave no raw test credentials in the journal.

## Consequences
- A finding is not a verified fix. A proposed remediation must pass a regression test before it is reported as successful.
- Security checks use authorized source, local dependency records or isolated fixtures. They do not execute attacks against systems.
