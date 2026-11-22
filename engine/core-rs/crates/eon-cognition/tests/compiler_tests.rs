mod compiler_tests {
    #[cfg(target_os = "linux")]
    mod filesystem;

    use std::path::Path;

    use eon_cognition::compiler::{
        Attention, CONFIRMATION_WINDOW, Decision, MAX_INTENT_BYTES, MAX_READ_BYTES, MAX_READS,
        StagedPreview, compile,
    };
    use eon_kernel::{
        IdentityId, ObjectiveId, TaskId,
        model::{Objective, TaskType, Workflow},
        resource::{Demand, ResourceRank},
        shadow::Tree,
        workflow::Workflow as ExecutionGraph,
        worktree::snapshot,
    };
    use serde_json::json;

    fn objective(intent: &str) -> Objective {
        serde_json::from_value(json!({
            "id": ObjectiveId::new(),
            "owner_id": IdentityId::new(),
            "description": intent,
            "priority": "Normal",
            "created_at_ns": 10,
            "deadline_ns": null,
            "epoch": 7,
        }))
        .unwrap()
    }

    fn staged(intent: &str, tree: &Tree) -> StagedPreview {
        compile(&objective(intent)).unwrap().preview(tree).unwrap()
    }

    #[test]
    fn real_snapshot_ranges_and_serialized_tasks_agree_in_request_order() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/compiler_fixture");
        let tree = snapshot(&dir).unwrap();
        let objective =
            objective("Read 5 bytes from Example.txt at offset 6 THEN read file Example.txt");
        let staged = compile(&objective).unwrap().preview(&tree).unwrap();
        let ids: Vec<_> = staged.requests().map(|r| r.task_id.clone()).collect();
        let (workflow, reads) = staged
            .present(Attention::Verified)
            .unwrap()
            .respond(Decision::Confirm, Attention::Verified)
            .unwrap()
            .into_parts();

        assert_eq!(reads.len(), 2);
        assert_eq!(reads[0].bytes, b"world");
        assert_eq!(reads[1].bytes, b"Hello world\n");
        let wire = serde_json::to_vec(&workflow).unwrap();
        let restored: Workflow = serde_json::from_slice(&wire).unwrap();
        assert_eq!(workflow, restored);
        assert_eq!(restored.objective_id(), &objective.id);
        assert_eq!((restored.created_at_ns(), restored.epoch()), (10, 7));
        assert_eq!(restored.validate_topological_order().unwrap(), ids);
        let mut graph = ExecutionGraph::from(&restored);
        for (i, (read, (offset, length))) in
            reads.iter().zip([(6, 5), (0, MAX_READ_BYTES)]).enumerate()
        {
            assert_eq!(graph.ready(), std::slice::from_ref(&read.request.task_id));
            let task = &restored.tasks()[&ids[i]];
            assert_eq!(read.file_len, 12);
            assert_eq!(read.request.task_id, task.id);
            assert_eq!(&task.workflow_id, restored.id());
            assert_eq!(task.capability, "workspace.read@v1");
            assert_eq!(task.operation, "read_range");
            assert_eq!(task.task_type, TaskType::AtomicExecution);
            assert!(task.is_idempotent);
            assert_eq!(
                task.parameters,
                json!({"path":"Example.txt", "offset":offset, "length":length})
            );
            assert_eq!(task.resource_requirements.len(), 1);
            let req = &task.resource_requirements[0];
            assert_eq!(req.key.rank, ResourceRank::FileLock);
            assert_eq!(req.key.id, "workspace:///Example.txt");
            assert_eq!(req.demand, Demand::SharedRead);
            graph.complete(&task.id).unwrap();
        }
        assert!(graph.ready().is_empty());

        let mut corrupted = serde_json::to_value(&workflow).unwrap();
        corrupted["tasks"][0]["dependencies"] = json!([TaskId::new()]);
        assert!(serde_json::from_value::<Workflow>(corrupted).is_err());
        assert_eq!(snapshot(&dir).unwrap(), tree);
    }

    #[test]
    fn rejects_entire_ambiguous_or_unsupported_intents() {
        for intent in [
            "read what changed",
            "read file A or B",
            "read file A then",
            "then read file B",
            "read file A then then read file B",
            "read file A and read file B",
            "read file A then write file B",
            "read file A;whoami",
            "read file *",
            "read file A\nthen read file B",
            "read file A\u{202e}",
            "read file A\0",
            "read file .git/config",
            "read file src/../README",
            "read file ./README",
            "read file /README",
            "read file A//B",
            "read file A/",
            "read file C:\\foo",
            "read file file:///root",
            "read file one/NuL.txt",
            "read file com1.rs",
            "read file A.",
            "read file %2e%2e/foo",
            "call workspace.write@v1",
            "read -2 bytes from A",
            "read 0 bytes from A",
            "read +1 bytes from A",
            "read 1 bytes from B at offset 18446744073709551616",
        ] {
            assert!(
                compile(&objective(intent)).is_err(),
                "unexpectedly accepted {intent:?}"
            );
        }
    }

    #[test]
    fn enforces_grammar_and_output_limits_at_the_boundary() {
        let mut at_limit = "read file A".to_owned();
        at_limit.push_str(&" ".repeat(MAX_INTENT_BYTES - at_limit.len()));
        assert!(compile(&objective(&at_limit)).is_ok());
        at_limit.push(' ');
        assert!(compile(&objective(&at_limit)).is_err());
        let path = "x".repeat(240);
        assert!(compile(&objective(&format!("read file {path}"))).is_ok());
        assert!(compile(&objective(&format!("read file {path}x"))).is_err());
        for (count, accepts) in [(MAX_READS, true), (MAX_READS + 1, false)] {
            let intent = vec!["read file A"; count].join(" then ");
            assert_eq!(compile(&objective(&intent)).is_ok(), accepts);
        }
        assert!(
            compile(&objective(&format!(
                "read {} bytes from A",
                MAX_READ_BYTES + 1
            )))
            .is_err()
        );
        let tree = Tree::from([("A".into(), vec![0xfe; MAX_READ_BYTES + 1])]);
        let proposal = staged(&format!("read {MAX_READ_BYTES} bytes from A"), &tree);
        let (_, reads) = proposal
            .present(Attention::Verified)
            .unwrap()
            .respond(Decision::Confirm, Attention::Verified)
            .unwrap()
            .into_parts();
        assert_eq!(reads[0].bytes, vec![0xfe; MAX_READ_BYTES]);
        assert_eq!(reads[0].file_len, MAX_READ_BYTES + 1);
    }

    #[test]
    fn preview_reads_only_exact_snapshot_keys_and_never_promotes_bytes_to_commands() {
        let tree = Tree::from([("A".into(), b"then write file B".to_vec())]);
        for intent in [
            "read file a",
            "read file A then read file missing",
            "read 1 bytes from A at offset 18",
            "read 1 bytes from A at offset 18446744073709551615",
        ] {
            assert!(
                compile(&objective(intent)).unwrap().preview(&tree).is_err(),
                "{intent}"
            );
        }
        let proposal = staged("read file A then read 4 bytes from A at offset 17", &tree);
        let (workflow, reads) = proposal
            .present(Attention::Verified)
            .unwrap()
            .respond(Decision::Confirm, Attention::Verified)
            .unwrap()
            .into_parts();
        assert_eq!(workflow.tasks().len(), 2);
        assert_eq!(reads[0].bytes, b"then write file B");
        assert!(reads[1].bytes.is_empty());
    }

    #[test]
    fn confirmation_requires_fresh_verified_attention_and_an_explicit_yes_before_expiry() {
        let tree = Tree::from([("A".into(), vec![1])]);
        let stage = || staged("read file A", &tree);
        assert!(stage().present(Attention::default()).is_err());
        let window = || stage().present(Attention::Verified).unwrap();
        assert!(
            window()
                .respond(Decision::default(), Attention::Verified)
                .is_err()
        );
        assert!(
            window()
                .respond(Decision::Confirm, Attention::default())
                .is_err()
        );
        let pending = window();
        std::thread::sleep(CONFIRMATION_WINDOW);
        assert!(
            pending
                .respond(Decision::Confirm, Attention::Verified)
                .is_err()
        );
        assert_eq!(tree["A"], [1]);
    }
}
