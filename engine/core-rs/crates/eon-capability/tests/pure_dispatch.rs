#![cfg(target_os = "linux")]

mod approval_fixture;
use approval_fixture::{Fixture, operator, prepared, workflow};
use eon_capability::{
    EonError, EonResult,
    ifc::{
        labels::{CognitiveTaint, ConfidentialityLevel, IntegrityLevel, SecurityLabel},
        rule_of_two::{Capability, ControlParameter},
    },
    mcp_pinning::ToolDescriptor,
    melon::{MaskedRunner, ToolCall},
    registry::{AllowedTool, CapabilityRegistry, DurableApprovals, ToolInput},
    wasm_tool::{DISPATCH_FUEL, MAX_INPUT_BYTES, MAX_MODULE_BYTES, MAX_OUTPUT_BYTES},
};

const ECHO: &[u8] = br#"(module (memory (export "memory") 1)
    (func (export "run") (param i32) (result i32) local.get 0))"#;
const TRAP: &[u8] = br#"(module (memory (export "memory") 1)
    (func $private_fixture_name (export "run") (param i32) (result i32) unreachable))"#;
const OBJECT: &str = r#"{"type":"object","properties":{"msg":{"type":"string"}},"required":["msg"],"additionalProperties":false}"#;
const MSG: &str = r#"{"msg":"ok"}"#;

fn descriptor(schema: &str) -> ToolDescriptor {
    ToolDescriptor {
        name: "echo".into(),
        description: "return validated input".into(),
        input_schema: schema.into(),
        annotations: "{}".into(),
    }
}

fn approved(schema: &str, module: &[u8]) -> AllowedTool {
    AllowedTool {
        descriptor: descriptor(schema),
        module_blake3: blake3::hash(module).into(),
    }
}

fn registry(schema: &str, module: &[u8]) -> Fixture {
    let mut reg = prepared([approved(schema, module)]).unwrap();
    reg.register(descriptor(schema), module).unwrap();
    reg
}

fn invoke(reg: &mut CapabilityRegistry, json: &str) -> EonResult<String> {
    reg.dispatch(
        "echo",
        &mut workflow(),
        ToolInput {
            json,
            label: &operator(),
        },
    )
    .map(|o| o.output)
}

mod registry_tests {
    use super::*;
    use eon_capability::wasm_tool::{MAX_WORKER_WALL_TIME, WasmWorker};
    use std::{path::Path, time::Duration};

    #[test]
    fn exhausted_journal_refuses_admission_or_withholds_the_guest_result() {
        use eon_foundation::{group_commit::GroupCommitJournal, journal::replay_from};
        use std::{fs, sync::Arc};

        for slots in [0, 1] {
            let mut fixture = registry(OBJECT, ECHO);
            let dir = fixture.root.with_file_name("last-segment");
            // Model a final, empty journal segment. Reopen the real writer with
            // room for either no records or just the attempt, without mocking fsync.
            drop(GroupCommitJournal::spawn(&dir, 1).unwrap());
            let first = u64::MAX - slots;
            fs::rename(
                dir.join("seg_00000000000000000001.eonj"),
                dir.join(format!("seg_{first:020}.eonj")),
            )
            .unwrap();
            let journal = Arc::new(GroupCommitJournal::spawn(&dir, 1).unwrap());
            fixture.registry = fixture.registry.with_dispatch_journal(journal);
            let mut profile = workflow();
            let result = fixture.dispatch(
                "echo",
                &mut profile,
                ToolInput {
                    json: MSG,
                    label: &operator(),
                },
            );
            assert!(
                matches!(result, Err(EonError::InternalError(m)) if m.contains("sequence space exhausted"))
            );
            assert_eq!(
                profile.context.integrity == IntegrityLevel::Untrusted,
                slots == 1
            );
            drop(fixture.registry);
            let frames = replay_from(&dir, first).unwrap();
            assert_eq!(frames.len() as u64, slots);
            if let Some(frame) = frames.first() {
                assert_eq!(frame.seq_no, first);
                assert!(frame.payload.starts_with(b"tool.dispatch attempt "));
            }
        }
    }

    #[test]
    fn missing_audit_journal_denies_even_an_approved_registered_tool() {
        let fixture = prepared([approved(OBJECT, TRAP)]).unwrap();
        let mut reg = CapabilityRegistry::with_allowlist(
            [approved(OBJECT, TRAP)],
            DurableApprovals::open(&fixture.root).unwrap(),
        )
        .unwrap()
        .with_wasm_worker(WasmWorker::new(&fixture.executable, MAX_WORKER_WALL_TIME).unwrap());
        reg.register(descriptor(OBJECT), TRAP).unwrap();
        assert!(
            matches!(invoke(&mut reg, MSG), Err(EonError::SecurityViolation(m)) if m.contains("audit journal"))
        );
        let mut profile = workflow();
        let input = ToolInput {
            json: MSG,
            label: &operator(),
        };
        assert!(
            matches!(reg.dispatch_guarded("echo", &mut profile, input, Some("external")),
                Err(EonError::SecurityViolation(m)) if m.contains("audit journal"))
        );
        assert_eq!(profile.context.integrity, IntegrityLevel::Untrusted);
    }

    #[test]
    fn only_a_protected_pinned_native_worker_can_be_provisioned() {
        use std::{
            fs,
            os::unix::fs::{PermissionsExt, symlink},
        };
        let mut fixture = prepared([approved(OBJECT, ECHO)]).unwrap();
        let exe = &fixture.executable;
        let directory = exe.parent().unwrap();
        let provision = |path: &Path| WasmWorker::new(path, MAX_WORKER_WALL_TIME);
        assert!(provision(directory).is_err());
        fs::set_permissions(exe, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(provision(exe).is_err());
        fs::set_permissions(exe, fs::Permissions::from_mode(0o500)).unwrap();
        fs::set_permissions(directory, fs::Permissions::from_mode(0o770)).unwrap();
        assert!(provision(exe).is_err());
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
        let alias = directory.join("alias");
        symlink(exe, &alias).unwrap();
        assert!(provision(&alias).is_err());
        fs::remove_file(&alias).unwrap();
        symlink(directory, &alias).unwrap();
        assert!(provision(&alias.join("eon-wasm-worker")).is_err());
        fs::remove_file(&alias).unwrap();
        fs::hard_link(exe, &alias).unwrap();
        assert!(provision(exe).is_err());
        fs::remove_file(&alias).unwrap();
        // Replacing a name after provisioning cannot select a different worker.
        fs::rename(exe, &alias).unwrap();
        fs::write(exe, b"not a worker").unwrap();
        fs::set_permissions(exe, fs::Permissions::from_mode(0o500)).unwrap();
        assert!(provision(exe).is_err());
        fixture.register(descriptor(OBJECT), ECHO).unwrap();
        assert_eq!(invoke(&mut fixture, MSG).unwrap(), MSG);
    }

    #[test]
    fn registration_requires_a_host_worker_and_a_timely_validation() {
        let root = prepared([]).unwrap();
        let store = DurableApprovals::open(&root.root).unwrap();
        let worker_path = &root.executable;
        assert!(WasmWorker::new(Path::new("eon-wasm-worker"), MAX_WORKER_WALL_TIME).is_err());
        assert!(WasmWorker::new(worker_path, Duration::ZERO).is_err());
        assert!(
            WasmWorker::new(worker_path, MAX_WORKER_WALL_TIME + Duration::from_secs(1)).is_err()
        );
        for worker in [
            None,
            Some(WasmWorker::new(worker_path, Duration::from_nanos(1)).unwrap()),
        ] {
            let entry = approved(OBJECT, ECHO);
            store.approve(&entry).unwrap();
            let mut reg = CapabilityRegistry::with_allowlist([entry], store.clone())
                .unwrap()
                .with_dispatch_journal(root.journal.clone());
            let has_worker = worker.is_some();
            if let Some(worker) = worker {
                reg = reg.with_wasm_worker(worker);
            }
            let error = reg.register(descriptor(OBJECT), ECHO).unwrap_err();
            assert!(matches!(
                (has_worker, error),
                (true, EonError::ResourceLeaseTimeout { .. })
                    | (false, EonError::SecurityViolation(_))
            ));
            assert!(matches!(
                invoke(&mut reg, MSG),
                Err(EonError::CapabilityNotFound { .. })
            ));
            // An interrupted registration does not silently restore durable approval.
            let mut peer = root
                .provisioned([approved(OBJECT, ECHO)], store.clone())
                .unwrap();
            assert!(matches!(
                peer.register(descriptor(OBJECT), ECHO),
                Err(EonError::SecurityViolation(_))
            ));
        }
    }

    #[test]
    fn both_existing_dispatch_apis_enforce_the_worker_budget() {
        let fixture = prepared([approved(OBJECT, ECHO)]).unwrap();
        let store = DurableApprovals::open(&fixture.root).unwrap();
        let mut reg = fixture
            .provisioned([approved(OBJECT, ECHO)], store)
            .unwrap();
        reg.register(descriptor(OBJECT), ECHO).unwrap();
        let exe = &fixture.executable;
        reg = reg.with_wasm_worker(WasmWorker::new(exe, Duration::from_nanos(1)).unwrap());
        assert!(matches!(
            invoke(&mut reg, MSG),
            Err(EonError::ResourceLeaseTimeout { .. })
        ));
        let mut profile = workflow();
        let input = ToolInput {
            json: MSG,
            label: &operator(),
        };
        assert!(matches!(
            reg.dispatch_guarded("echo", &mut profile, input, None),
            Err(EonError::ResourceLeaseTimeout { .. })
        ));
        assert_eq!(profile.context.integrity, IntegrityLevel::Untrusted);
        reg = reg.with_wasm_worker(WasmWorker::new(exe, MAX_WORKER_WALL_TIME).unwrap());
        assert_eq!(invoke(&mut reg, MSG).unwrap(), MSG);
    }

    #[test]
    fn quarantine_survives_restart_until_an_exact_explicit_reapproval() {
        for metadata_drift in [false, true] {
            let fixture = prepared([approved(OBJECT, ECHO)]).unwrap();
            let root = &fixture.root;
            let reopen = |entry| {
                fixture
                    .provisioned([entry], DurableApprovals::open(root).unwrap())
                    .unwrap()
            };
            let mut first = reopen(approved(OBJECT, ECHO));
            first.register(descriptor(OBJECT), ECHO).unwrap();
            let mut peer = reopen(approved(OBJECT, ECHO));
            peer.register(descriptor(OBJECT), ECHO).unwrap();
            let mut listing = descriptor(OBJECT);
            let bytes = if metadata_drift {
                listing.description.push_str(" changed");
                ECHO
            } else {
                TRAP
            };
            assert!(matches!(
                first.register(listing, bytes),
                Err(EonError::SecurityViolation(_))
            ));
            drop(first);
            // Even a peer with a previously validated module consults the durable state.
            assert!(matches!(
                invoke(&mut peer, MSG),
                Err(EonError::SecurityViolation(_))
            ));
            let mut fresh = reopen(approved(OBJECT, ECHO));
            assert!(matches!(
                fresh.register(descriptor(OBJECT), ECHO),
                Err(EonError::SecurityViolation(_))
            ));
            assert!(invoke(&mut fresh, MSG).is_err());
            // Explicitly approve a DIFFERENT reviewed identity. Old registries and
            // old expected digests must not become callable on re-approval.
            let revised_schema = r#"{"type":"object"}"#;
            let revised = approved(revised_schema, ECHO);
            DurableApprovals::open(root)
                .unwrap()
                .approve(&revised)
                .unwrap();
            assert!(invoke(&mut peer, MSG).is_err());
            assert!(fresh.register(descriptor(OBJECT), ECHO).is_err());
            let mut restored = reopen(revised);
            restored.register(descriptor(revised_schema), ECHO).unwrap();
            assert_eq!(invoke(&mut restored, MSG).unwrap(), MSG);
        }
    }

    #[test]
    fn missing_busy_or_corrupt_durable_state_never_authorizes_execution() {
        let empty = prepared([]).unwrap();
        let store = DurableApprovals::open(&empty.root).unwrap();
        let entry = approved(OBJECT, ECHO);
        let mut reg = empty
            .provisioned([approved(OBJECT, ECHO)], store.clone())
            .unwrap();
        assert!(reg.register(descriptor(OBJECT), ECHO).is_err());
        store.approve(&entry).unwrap();
        reg.register(descriptor(OBJECT), ECHO).unwrap();
        let path = std::fs::read_dir(&empty.root)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let busy = std::fs::File::open(&path).unwrap();
        busy.lock().unwrap();
        assert!(matches!(
            invoke(&mut reg, MSG),
            Err(EonError::OperationFailed { .. })
        ));
        assert!(reg.register(descriptor(OBJECT), ECHO).is_err());
        assert!(store.approve(&entry).is_err());
        busy.unlock().unwrap();
        drop(busy);
        assert_eq!(invoke(&mut reg, MSG).unwrap(), MSG);
        for corrupt in [&b"short"[..], &[0; 32][..]] {
            std::fs::write(&path, corrupt).unwrap();
            assert!(invoke(&mut reg, MSG).is_err());
            let mut restarted = empty
                .provisioned([approved(OBJECT, ECHO)], store.clone())
                .unwrap();
            assert!(restarted.register(descriptor(OBJECT), ECHO).is_err());
        }
        std::fs::remove_file(path).unwrap();
        assert!(invoke(&mut reg, MSG).is_err());
    }

    #[test]
    fn a_listing_cannot_self_approve_and_approval_alone_does_not_register() {
        let mut bare = CapabilityRegistry::new();
        assert!(matches!(
            bare.register(descriptor(OBJECT), ECHO),
            Err(EonError::SecurityViolation(_))
        ));
        let mut reg = prepared([approved(OBJECT, ECHO)]).unwrap();
        assert!(matches!(
            invoke(&mut reg, MSG),
            Err(EonError::CapabilityNotFound { .. })
        ));
        assert!(prepared([approved(OBJECT, ECHO), approved(OBJECT, ECHO)]).is_err());
        reg.register(descriptor(OBJECT), ECHO).unwrap();
        reg.register(descriptor(OBJECT), ECHO).unwrap();
        assert_eq!(invoke(&mut reg, r#"{ "msg" : "ok" }"#).unwrap(), MSG);
    }

    #[test]
    fn changing_approved_metadata_or_code_quarantines_the_installed_version() {
        for change_code in [false, true] {
            let mut reg = registry(OBJECT, ECHO);
            let mut listing = descriptor(OBJECT);
            let module = if change_code {
                &b"(module)"[..]
            } else {
                listing.description.push_str(" changed");
                ECHO
            };
            assert!(matches!(
                reg.register(listing, module),
                Err(EonError::SecurityViolation(_))
            ));
            assert!(
                matches!(invoke(&mut reg, MSG), Err(EonError::SecurityViolation(m)) if m.contains("quarantined"))
            );
            assert!(reg.register(descriptor(OBJECT), ECHO).is_err());
        }
    }

    #[test]
    fn invalid_schema_or_module_is_never_installed() {
        for schema in [
            "not JSON",
            r#"{"properties":{"unused":{"type":42}}}"#,
            r#"{"enum":true}"#,
            r#"{"required":[1]}"#,
            r#"{"items":{"unsupported":1}}"#,
        ] {
            let mut reg = prepared([approved(schema, ECHO)]).unwrap();
            assert!(matches!(
                reg.register(descriptor(schema), ECHO),
                Err(EonError::ValidationError(_))
            ));
            assert!(matches!(
                invoke(&mut reg, MSG),
                Err(EonError::CapabilityNotFound { .. })
            ));
        }
        for bad in [
            "not Wasm",
            "(module)",
            r#"(module (memory (export "memory") 1) (func (export "run")))"#,
        ] {
            let mut reg = prepared([approved(OBJECT, bad.as_bytes())]).unwrap();
            assert!(reg.register(descriptor(OBJECT), bad.as_bytes()).is_err());
            assert!(matches!(
                invoke(&mut reg, MSG),
                Err(EonError::CapabilityNotFound { .. })
            ));
        }
        let mut reg = registry(OBJECT, ECHO);
        assert!(
            matches!(reg.register(descriptor(OBJECT), &vec![0; MAX_MODULE_BYTES + 1]),
            Err(EonError::ResourceExhausted {resource, ..}) if resource == "tool_module_bytes")
        );
        assert!(
            matches!(invoke(&mut reg, MSG), Err(EonError::SecurityViolation(m)) if m.contains("quarantined"))
        );
    }

    #[test]
    fn malformed_wrong_type_and_undeclared_fields_do_not_reach_guest() {
        let mut reg = registry(OBJECT, TRAP);
        for bad in [
            "not JSON",
            "42",
            "{}",
            r#"{"msg":2}"#,
            r#"{"msg":"ok","extra":0}"#,
        ] {
            assert!(matches!(
                invoke(&mut reg, bad),
                Err(EonError::ValidationError(_))
            ));
        }
        let mut closed = registry(r#"{"type":"object","additionalProperties":false}"#, ECHO);
        assert!(matches!(
            invoke(&mut closed, MSG),
            Err(EonError::ValidationError(_))
        ));
    }

    #[test]
    fn bounds_checked_on_both_sides_of_the_abi() {
        let mut echo = registry(r#"{"type":"string"}"#, ECHO);
        let full = format!("\"{}\"", "x".repeat(MAX_INPUT_BYTES - 2));
        assert_eq!(invoke(&mut echo, &full).unwrap(), full);
        let too_large = format!(" {full}");
        assert!(
            matches!(invoke(&mut echo, &too_large), Err(EonError::ResourceExhausted {resource, ..}) if resource == "tool_input_bytes")
        );
        for size in [MAX_OUTPUT_BYTES as i32, MAX_OUTPUT_BYTES as i32 + 1, -1] {
            let module = format!(
                r#"(module (memory (export "memory") 1)
                (func (export "run") (param i32) (result i32) i32.const {size}))"#
            );
            let result = invoke(&mut registry("{}", module.as_bytes()), "0");
            if size == MAX_OUTPUT_BYTES as i32 {
                assert_eq!(result.unwrap().len(), MAX_OUTPUT_BYTES);
            } else {
                assert!(
                    matches!(result, Err(EonError::ResourceExhausted {resource, requested, available})
                    if resource == "tool_output_bytes" && requested == u64::from(size as u32) && available == MAX_OUTPUT_BYTES as u64)
                );
            }
        }
        let invalid = br#"(module (memory (export "memory") 1)
            (func (export "run") (param i32) (result i32)
              i32.const 0 i32.const 255 i32.store8 i32.const 1))"#;
        assert!(matches!(invoke(&mut registry("{}", invalid), "0"),
            Err(EonError::OperationFailed {operation, ..}) if operation == "wasm.output"));
    }

    #[test]
    fn start_and_export_loops_stop_on_real_fuel_traps() {
        for wat in [
            r#"(module (memory (export "memory") 1) (func $init (loop br 0)) (start $init)
                 (func (export "run") (param i32) (result i32) i32.const 1))"#,
            r#"(module (memory (export "memory") 1)
                 (func (export "run") (param i32) (result i32) (loop br 0) i32.const 1))"#,
        ] {
            assert!(matches!(invoke(&mut registry("{}", wat.as_bytes()), "0"),
                Err(EonError::ResourceExhausted {resource, requested: DISPATCH_FUEL, available: 0}) if resource == "wasm_fuel"));
        }
    }

    #[test]
    fn guest_cannot_instantiate_oversized_memory_or_tables() {
        for storage in [
            r#"(memory (export "memory") 1025)"#,
            r#"(memory (export "memory") 1) (table 1025 funcref)"#,
        ] {
            let module = format!(
                r#"(module {storage}
                    (func (export "run") (param i32) (result i32) local.get 0))"#
            );
            assert!(
                matches!(invoke(&mut registry("{}", module.as_bytes()), "0"),
                    Err(EonError::OperationFailed {operation, ..}) if operation == "wasm.instantiate")
            );
        }
    }
}

mod taint_enforcement_tests {
    use super::*;

    struct Masked(Vec<ToolCall>);
    impl MaskedRunner for Masked {
        fn emitted_calls(&self, _: &str) -> EonResult<Vec<ToolCall>> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn successful_pure_output_taints_the_workflow_and_blocks_privilege_escalation() {
        let mut reg = registry(OBJECT, ECHO);
        let owner = operator();
        let input = || ToolInput {
            json: MSG,
            label: &owner,
        };
        let mut profile = workflow();
        profile.context.confidentiality = ConfidentialityLevel::Restricted;
        assert_eq!(
            CognitiveTaint::from(profile.context.integrity),
            CognitiveTaint::TrustedOperator
        );
        let out = reg.dispatch("echo", &mut profile, input()).unwrap();
        assert_eq!(out.output, MSG);
        assert_eq!(out.taint(), CognitiveTaint::UntrustedExternal);
        assert_eq!(out.label.confidentiality, ConfidentialityLevel::Restricted);
        assert_eq!(out.label.provenance, ["operator", "echo"]);
        assert_eq!(out.label, profile.context);

        reg.set_melon_runner(Box::new(Masked(vec![])));
        for privilege in [
            Capability::MutateWorkspace,
            Capability::DeviceControl,
            Capability::ExecuteProcess,
            Capability::NetworkAccess {
                pinned: false,
                query_parameterized: false,
            },
        ] {
            profile.capabilities = vec![privilege];
            profile.air_gapped = true;
            assert!(
                matches!(reg.dispatch_guarded("echo", &mut profile, input(), Some(&out.output)),
                Err(EonError::SecurityViolation(m)) if m.contains("human approval"))
            );
        }
    }

    #[test]
    fn even_without_the_triad_external_inputs_controls_or_context_cannot_elevate() {
        let mut reg = registry(OBJECT, TRAP);
        let owner = operator();
        let web = SecurityLabel::external("fixture");
        reg.set_melon_runner(Box::new(Masked(vec![])));
        for source in ["input", "control", "context"] {
            let mut profile = workflow();
            profile.capabilities = vec![Capability::ExecuteProcess];
            if source == "control" {
                profile.control_parameters.push(ControlParameter {
                    name: "selection".into(),
                    label: web.clone(),
                });
            }
            let label = if source == "input" { &web } else { &owner };
            let result = reg.dispatch_guarded(
                "echo",
                &mut profile,
                ToolInput { json: MSG, label },
                (source == "context").then_some("external"),
            );
            assert!(
                matches!(result, Err(EonError::SecurityViolation(m)) if m.contains("privileged") || m.contains("UntrustedControlParameter"))
            );
            assert_eq!(profile.context.integrity, IntegrityLevel::Untrusted);
        }
    }

    #[test]
    fn a_guest_failure_cannot_restore_operator_authority_or_leak_guest_backtraces() {
        let mut reg = registry(OBJECT, TRAP);
        let mut profile = workflow();
        let err = reg
            .dispatch(
                "echo",
                &mut profile,
                ToolInput {
                    json: MSG,
                    label: &operator(),
                },
            )
            .unwrap_err();
        assert!(
            matches!(err, EonError::OperationFailed {ref operation, ..} if operation == "wasm.call")
        );
        assert!(!err.to_string().contains("private_fixture_name"));
        assert_eq!(profile.context.integrity, IntegrityLevel::Untrusted);
    }

    #[test]
    fn known_external_context_requires_a_masked_check_without_a_plain_dispatch_bypass() {
        let mut reg = registry(OBJECT, ECHO);
        let mut profile = workflow();
        let owner = operator();
        let input = || ToolInput {
            json: MSG,
            label: &owner,
        };
        assert!(
            matches!(reg.dispatch_guarded("echo", &mut profile, input(), Some("external")),
            Err(EonError::SecurityViolation(m)) if m.contains("MELON runner"))
        );
        assert!(
            matches!(reg.dispatch("echo", &mut profile, input()), Err(EonError::SecurityViolation(m)) if m.contains("MELON"))
        );
        reg.set_melon_runner(Box::new(Masked(vec![ToolCall {
            name: "echo".into(),
            arguments: serde_json::json!({"msg":"ok"}),
        }])));
        assert!(
            matches!(reg.dispatch_guarded("echo", &mut profile, input(), Some("external")),
            Err(EonError::SecurityViolation(m)) if m.contains("matches a masked-run"))
        );
        reg.set_melon_runner(Box::new(Masked(vec![])));
        let out = reg
            .dispatch_guarded("echo", &mut profile, input(), Some("external"))
            .unwrap();
        assert_eq!(out.output, MSG);
        assert_eq!(out.taint(), CognitiveTaint::UntrustedExternal);
    }
}
