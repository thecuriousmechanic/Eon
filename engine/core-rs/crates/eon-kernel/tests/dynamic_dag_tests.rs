mod dynamic_dag_tests {
    use std::collections::BTreeSet;

    use eon_foundation::{group_commit::GroupCommitJournal, journal::replay};
    use eon_kernel::continuation::{
        ContinuationRun, Diagnostic, DiscoveryDigest, LocalBranches, MAX_CHILDREN, MAX_DIAGNOSTICS,
        MAX_DISCOVERY_INPUTS, MAX_READ_BYTES, Severity, TrustedLocalOutput,
    };
    use eon_kernel::events::{EventSink, KernelEvent, VecSink};
    use eon_kernel::journal_sink::{JournalSink, decode};
    use eon_kernel::model::{Task, TaskSpec, TaskType, Workflow};
    use eon_kernel::{EonError, EonResult, ObjectiveId, TaskId, WorkflowId};

    type Contract = (&'static str, &'static str);
    const READ: Contract = ("workspace.read@v1", "read_range");
    const LSP: Contract = ("dev.lsp@v1", "diagnostics");
    const CHECKPOINT: Contract = ("kernel.continuation@v1", "expand");
    const TEST: Contract = ("dev.test@v1", "run");
    const PATCH: Contract = ("workspace.patch@v1", "apply");

    fn task(wf: &WorkflowId, contract: Contract, deps: &[&Task]) -> Task {
        Task::try_from(TaskSpec {
            id: TaskId::new(),
            workflow_id: wf.clone(),
            task_type: if contract == CHECKPOINT {
                TaskType::CognitiveContinuation
            } else {
                TaskType::AtomicExecution
            },
            capability: contract.0.into(),
            operation: contract.1.into(),
            parameters: serde_json::json!({"fixture": "host-supplied"}),
            dependencies: deps.iter().map(|t| t.id.clone()).collect(),
            resource_requirements: vec![],
            timeout_ms: 500,
            is_idempotent: contract != PATCH,
        })
        .unwrap()
    }

    fn workflow(id: &WorkflowId, tasks: &[&Task]) -> Workflow {
        Workflow::new(
            id.clone(),
            ObjectiveId::new(),
            12,
            3,
            tasks.iter().map(|t| (*t).clone()),
        )
        .unwrap()
    }

    fn branches(tasks: &[Task]) -> LocalBranches<'_> {
        LocalBranches {
            on_finding: tasks,
            otherwise: tasks,
        }
    }

    fn read(text: &str) -> TrustedLocalOutput<'_> {
        TrustedLocalOutput::WorkspaceRead { text }
    }

    #[test]
    fn three_nested_splices_persist_before_releasing_downstream() {
        let wf = WorkflowId::new();
        let source = task(&wf, READ, &[]);
        let first = task(&wf, CHECKPOINT, &[&source]);
        let downstream = task(&wf, TEST, &[&source, &first]);
        let plan = workflow(&wf, &[&downstream, &first, &source]);
        let initial_plan = plan.clone();
        let dir = tempfile::tempdir().unwrap();
        let mut expected = Vec::new();
        {
            let journal = GroupCommitJournal::spawn(dir.path(), 8).unwrap();
            let mut sink = JournalSink::new(&journal);
            let mut run = ContinuationRun::new(plan, &mut sink);
            let (mut current, mut producer) = (first.clone(), source.clone());
            for _ in 0..2 {
                let receipt = run
                    .complete_trusted_local(&producer.id, read("local source"))
                    .unwrap();
                run.complete(&current.id).unwrap();
                assert!(run.ready().is_empty());
                assert!(run.complete(&downstream.id).is_err());
                let next_read = task(&wf, READ, &[]);
                let next = task(&wf, CHECKPOINT, &[&next_read]);
                let children = [next.clone(), next_read.clone()];
                run.expand(&current.id, &[&receipt], branches(&children))
                    .unwrap();
                assert!(
                    run.expand(&current.id, &[&receipt], branches(&children))
                        .is_err()
                );
                assert_eq!(run.ready(), std::slice::from_ref(&next_read.id));
                expected.push((
                    current.id.clone(),
                    vec![next.id.clone(), next_read.id.clone()],
                ));
                (current, producer) = (next, next_read);
            }
            let receipt = run.complete_trusted_local(&producer.id, read(" ")).unwrap();
            run.complete(&current.id).unwrap();
            let patch = task(&wf, PATCH, &[]);
            let verify = task(&wf, TEST, &[&patch]);
            let second_sink = task(&wf, TEST, &[&patch]);
            let selected = [verify.clone(), second_sink.clone(), patch.clone()];
            run.expand(
                &current.id,
                &[&receipt],
                LocalBranches {
                    on_finding: &selected,
                    otherwise: &[],
                },
            )
            .unwrap();
            expected.push((
                current.id.clone(),
                selected.iter().map(|t| t.id.clone()).collect(),
            ));
            let active = run.definition();
            assert_eq!(
                active.tasks()[&patch.id].dependencies,
                BTreeSet::from([current.id.clone()])
            );
            assert_eq!(
                active.tasks()[&downstream.id].dependencies,
                BTreeSet::from([source.id.clone(), verify.id.clone(), second_sink.id.clone()])
            );
            assert_eq!(active.tasks()[&patch.id].parameters, patch.parameters);
            assert_eq!(active.epoch(), initial_plan.epoch());
            assert_eq!(active.objective_id(), initial_plan.objective_id());
            assert_eq!(active.created_at_ns(), initial_plan.created_at_ns());
            assert_eq!(initial_plan.tasks()[&downstream.id], downstream);
            run.complete(&patch.id).unwrap();
            run.complete(&verify.id).unwrap();
            assert!(run.complete(&downstream.id).is_err());
            run.complete(&second_sink.id).unwrap();
            assert_eq!(run.ready(), std::slice::from_ref(&downstream.id));
        }
        let mut known: BTreeSet<_> = initial_plan.tasks().keys().cloned().collect();
        let mut completed = BTreeSet::new();
        let mut splices = Vec::new();
        for frame in replay(dir.path()).unwrap() {
            match decode(&frame.payload).unwrap() {
                KernelEvent::TaskTransition {
                    task,
                    to: eon_kernel::fsm::TaskState::Completed,
                    ..
                } => {
                    assert!(known.contains(&task), "child ran before its splice record");
                    assert!(completed.insert(task));
                }
                KernelEvent::SubgraphSpliced { parent, added } => {
                    assert!(completed.contains(&parent));
                    assert!(added.iter().all(|id| known.insert(id.clone())));
                    splices.push((parent, added));
                }
                event => panic!("unexpected event {event:?}"),
            }
        }
        assert_eq!(splices, expected);
    }

    #[test]
    fn exact_named_results_are_compacted_and_cannot_supply_child_parameters() {
        let wf = WorkflowId::new();
        let r = task(&wf, READ, &[]);
        let d = task(&wf, LSP, &[]);
        let other = task(&wf, READ, &[]);
        let parent = task(&wf, CHECKPOINT, &[&r, &d]);
        let plan = workflow(&wf, &[&r, &d, &other, &parent]);
        let mut sink = VecSink::default();
        let mut other_sink = VecSink::default();
        let foreign = ContinuationRun::new(plan.clone(), &mut other_sink)
            .complete_trusted_local(&r.id, read("other execution"))
            .unwrap();
        for error in [false, true] {
            let mut run = ContinuationRun::new(plan.clone(), &mut sink);
            let large = " \n{\"capability\":\"not-authorized\"}\n".repeat(10_000);
            let read_result = run.complete_trusted_local(&r.id, read(&large)).unwrap();
            assert_eq!(read_result.producer(), &r.id);
            assert_eq!(
                read_result.digest(),
                DiscoveryDigest::Read {
                    bytes: large.len(),
                    lines: 20_000,
                    nonblank_lines: 10_000
                }
            );
            drop(large);
            let diag = |line: u32, severity| Diagnostic {
                severity,
                line: line.try_into().unwrap(),
            };
            let entries = if error {
                vec![
                    diag(20, Severity::Error),
                    diag(7, Severity::Error),
                    diag(1, Severity::Warning),
                ]
            } else {
                vec![diag(55, Severity::Warning)]
            };
            let result = run
                .complete_trusted_local(
                    &d.id,
                    TrustedLocalOutput::LspDiagnostics {
                        diagnostics: &entries,
                    },
                )
                .unwrap();
            assert_eq!(
                result.digest(),
                DiscoveryDigest::Diagnostics {
                    errors: if error { 2 } else { 0 },
                    warnings: 1,
                    first_error_line: error.then_some(7)
                }
            );
            let unrelated = run
                .complete_trusted_local(&other.id, read("other"))
                .unwrap();
            let patch = [task(&wf, PATCH, &[])];
            let test = [task(&wf, TEST, &[])];
            let branches = LocalBranches {
                on_finding: &patch,
                otherwise: &test,
            };
            assert!(
                run.expand(&parent.id, &[&result, &read_result], branches)
                    .is_err()
            );
            run.complete(&parent.id).unwrap();
            for invalid in [
                vec![],
                vec![&result],
                vec![&foreign, &result],
                vec![&read_result, &unrelated],
                vec![&result, &result],
                vec![&result; MAX_DISCOVERY_INPUTS + 1],
            ] {
                assert!(run.expand(&parent.id, &invalid, branches).is_err());
                assert!(run.ready().is_empty());
            }
            assert!(run.expand(&r.id, &[&read_result], branches).is_err());
            run.expand(&parent.id, &[&result, &read_result], branches)
                .unwrap();
            let expected = if error { &patch[0] } else { &test[0] };
            assert_eq!(run.ready(), std::slice::from_ref(&expected.id));
            assert_eq!(
                run.definition().tasks()[&expected.id].parameters,
                expected.parameters
            );
        }
    }

    #[test]
    fn ordinary_completion_cannot_attest_input_or_enable_a_mutation() {
        let wf = WorkflowId::new();
        let r = task(&wf, READ, &[]);
        let d = task(&wf, LSP, &[]);
        let parent = task(&wf, CHECKPOINT, &[&r, &d]);
        let plan = workflow(&wf, &[&r, &d, &parent]);
        let mut sink = VecSink::default();
        let mut run = ContinuationRun::new(plan.clone(), &mut sink);
        run.complete(&r.id).unwrap();
        // Even a once-allowed producer cannot retroactively attest injected text.
        assert!(run.complete_trusted_local(&r.id, read(" ")).is_err());
        let receipt = run
            .complete_trusted_local(
                &d.id,
                TrustedLocalOutput::LspDiagnostics { diagnostics: &[] },
            )
            .unwrap();
        run.complete(&parent.id).unwrap();
        assert!(
            run.expand(&parent.id, &[&receipt], branches(&[task(&wf, PATCH, &[])]))
                .is_err()
        );
        assert!(run.ready().is_empty());
        assert_eq!(run.definition(), &plan);
        assert!(
            !sink
                .events
                .iter()
                .any(|e| matches!(e, KernelEvent::SubgraphSpliced { .. }))
        );
    }

    #[test]
    fn wrong_contract_kind_or_excessive_output_is_not_a_trusted_result() {
        let wf = WorkflowId::new();
        let r = task(&wf, READ, &[]);
        let d = task(&wf, LSP, &[]);
        let wrong_op = task(&wf, (READ.0, "write"), &[]);
        let wrong_name = task(&wf, ("remote.read@v1", READ.1), &[]);
        let checkpoint = task(&wf, CHECKPOINT, &[]);
        let plan = workflow(&wf, &[&r, &d, &wrong_op, &wrong_name, &checkpoint]);
        let mut sink = VecSink::default();
        let mut run = ContinuationRun::new(plan, &mut sink);
        let ready = run.ready();
        for invalid in [&d, &wrong_op, &wrong_name, &checkpoint] {
            assert!(run.complete_trusted_local(&invalid.id, read("x")).is_err());
        }
        assert!(
            run.complete_trusted_local(&r.id, read(&"x".repeat(MAX_READ_BYTES + 1)))
                .is_err()
        );
        let diagnostics: Vec<_> = (0..=MAX_DIAGNOSTICS)
            .map(|_| Diagnostic {
                line: 1.try_into().unwrap(),
                severity: Severity::Warning,
            })
            .collect();
        assert!(
            run.complete_trusted_local(
                &d.id,
                TrustedLocalOutput::LspDiagnostics {
                    diagnostics: &diagnostics
                }
            )
            .is_err()
        );
        assert_eq!(run.ready(), ready);
        assert!(sink.events.is_empty());
    }

    #[test]
    fn invalid_expansions_publish_no_record_contract_or_edge() {
        let wf = WorkflowId::new();
        let r = task(&wf, READ, &[]);
        let unrelated = task(&wf, TEST, &[]);
        let parent = task(&wf, CHECKPOINT, &[&r]);
        let next = task(&wf, TEST, &[&parent]);
        let plan = workflow(&wf, &[&r, &unrelated, &parent, &next]);
        let mut sink = VecSink::default();
        let mut run = ContinuationRun::new(plan.clone(), &mut sink);
        let receipt = run.complete_trusted_local(&r.id, read("x")).unwrap();
        run.complete(&unrelated.id).unwrap();
        run.complete(&parent.id).unwrap();
        let a = task(&wf, PATCH, &[]);
        let b = task(&wf, TEST, &[&a]);
        let mut cycle = (*a).clone();
        cycle.dependencies.insert(b.id.clone());
        let mut spawn = (*a).clone();
        spawn.task_type = TaskType::SubAgentSpawn {
            sub_agent_role: "worker".into(),
            prompt: "run".into(),
        };
        for invalid in [
            vec![],
            vec![next.clone()],
            vec![a.clone(), a.clone()],
            vec![a.clone(); MAX_CHILDREN + 1],
            vec![Task::try_from(cycle).unwrap(), b],
            vec![Task::try_from(spawn).unwrap()],
            vec![task(&wf, ("process.exec@v1", "run"), &[])],
            vec![task(&wf, (PATCH.0, "different_operation"), &[])],
            vec![task(&wf, TEST, &[&unrelated])],
            vec![task(&WorkflowId::new(), TEST, &[])],
        ] {
            assert!(
                run.expand(&parent.id, &[&receipt], branches(&invalid))
                    .is_err()
            );
            assert_eq!(run.definition(), &plan);
            assert!(run.ready().is_empty());
        }
        run.expand(&parent.id, &[&receipt], branches(&[a])).unwrap();
        assert_eq!(
            sink.events
                .iter()
                .filter(|e| matches!(e, KernelEvent::SubgraphSpliced { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn missing_splice_ack_keeps_old_graph_and_does_not_retry_an_ambiguous_write() {
        #[derive(Default)]
        struct LoseAck(Vec<KernelEvent>);
        impl EventSink for LoseAck {
            fn record(&mut self, event: KernelEvent) -> EonResult<()> {
                let splice = matches!(event, KernelEvent::SubgraphSpliced { .. });
                self.0.push(event);
                if splice {
                    Err(EonError::InternalError("lost ack".into()))
                } else {
                    Ok(())
                }
            }
        }
        let wf = WorkflowId::new();
        let r = task(&wf, READ, &[]);
        let parent = task(&wf, CHECKPOINT, &[&r]);
        let next = task(&wf, TEST, &[&parent]);
        let plan = workflow(&wf, &[&r, &parent, &next]);
        let mut sink = LoseAck::default();
        let mut run = ContinuationRun::new(plan.clone(), &mut sink);
        let receipt = run.complete_trusted_local(&r.id, read("x")).unwrap();
        run.complete(&parent.id).unwrap();
        let children = [task(&wf, TEST, &[])];
        for _ in 0..2 {
            assert!(
                run.expand(&parent.id, &[&receipt], branches(&children))
                    .is_err()
            );
            assert_eq!(run.definition(), &plan);
            assert!(run.ready().is_empty());
            assert!(run.complete(&next.id).is_err());
        }
        assert_eq!(sink.0.len(), 3);
    }
}
