mod dag_validation_tests {
    use eon_kernel::model::{
        Objective, ObjectiveSpec, Priority, Task, TaskSpec, TaskType, Workflow,
    };
    use eon_kernel::resource::{CanonicalResourceKey, Demand, Requirement, ResourceRank};
    use eon_kernel::workflow as runtime;
    use eon_primitives::{IdentityId, ObjectiveId, TaskId, WorkflowId};
    use serde_json::json;

    fn task(wf: &WorkflowId, id: &TaskId, deps: &[&TaskId]) -> Task {
        TaskSpec {
            id: id.clone(),
            workflow_id: wf.clone(),
            task_type: TaskType::AtomicExecution,
            capability: "workspace.read@v1".into(),
            operation: "read_range".into(),
            parameters: json!({"path": "example.txt"}),
            dependencies: deps.iter().copied().cloned().collect(),
            resource_requirements: vec![Requirement {
                key: CanonicalResourceKey {
                    rank: ResourceRank::CpuCores,
                    id: String::new(),
                },
                demand: Demand::Pool(1),
            }],
            timeout_ms: 500,
            is_idempotent: true,
        }
        .try_into()
        .unwrap()
    }

    #[test]
    fn topological_order_and_runtime_projection_respect_every_edge() {
        // IDs sort sink-first, and the input is in neither ID nor topological order.
        let [sink, right, left, root] = std::array::from_fn(|_| TaskId::new());
        let wf = WorkflowId::new();
        let objective = ObjectiveId::new();
        let tasks = [
            task(&wf, &left, &[&root]),
            task(&wf, &sink, &[&left, &right]),
            task(&wf, &root, &[]),
            task(&wf, &right, &[&root]),
        ];
        let plan = Workflow::new(wf.clone(), objective.clone(), 12, 3, tasks.clone()).unwrap();
        assert_eq!(
            plan.validate_topological_order().unwrap(),
            [root.clone(), right.clone(), left.clone(), sink.clone()]
        );
        let wire = serde_json::to_string(&plan).unwrap();
        let reversed = Workflow::new(
            wf.clone(),
            objective.clone(),
            12,
            3,
            tasks.into_iter().rev(),
        )
        .unwrap();
        assert_eq!(serde_json::to_string(&reversed).unwrap(), wire);
        let restored: Workflow = serde_json::from_str(&wire).unwrap();
        assert_eq!(restored, plan);
        assert_eq!(restored.id(), &wf);
        assert_eq!(restored.objective_id(), &objective);
        assert_eq!((restored.created_at_ns(), restored.epoch()), (12, 3));

        let mut run = runtime::Workflow::from(&restored);
        assert_eq!(run.ready(), std::slice::from_ref(&root));
        run.complete(&root).unwrap();
        assert_eq!(run.ready(), [right.clone(), left.clone()]);
        run.complete(&left).unwrap();
        assert!(!run.ready().contains(&sink));
        run.complete(&right).unwrap();
        assert_eq!(run.ready(), [sink]);
        assert_eq!(serde_json::to_string(&restored).unwrap(), wire);
    }

    fn rejected_graph(wf: &WorkflowId, tasks: Vec<Task>, reason: &str) {
        let objective = ObjectiveId::new();
        let wire = json!({"id": wf, "objective_id": objective, "created_at_ns": 12, "epoch": 3, "tasks": tasks});
        let from_parts = Workflow::new(wf.clone(), objective, 12, 3, tasks).unwrap_err();
        assert!(from_parts.to_string().contains(reason), "{from_parts}");
        let from_wire = serde_json::from_value::<Workflow>(wire).unwrap_err();
        assert!(from_wire.to_string().contains(reason), "{from_wire}");
    }

    #[test]
    fn rejects_cycle_even_when_a_disjoint_root_can_be_emitted() {
        let wf = WorkflowId::new();
        let [root, a, b] = std::array::from_fn(|_| TaskId::new());
        let cycle = vec![task(&wf, &a, &[&b]), task(&wf, &b, &[&a])];
        rejected_graph(&wf, cycle.clone(), "cycle");
        let mut with_root = cycle;
        with_root.push(task(&wf, &root, &[]));
        rejected_graph(&wf, with_root, "cycle");
    }

    #[test]
    fn rejects_missing_dependency_duplicate_task_and_foreign_workflow() {
        let wf = WorkflowId::new();
        let node = TaskId::new();
        rejected_graph(
            &wf,
            vec![task(&wf, &node, &[&TaskId::new()])],
            "unknown task",
        );
        let root = task(&wf, &node, &[]);
        rejected_graph(&wf, vec![root.clone(), root], "duplicate task");
        rejected_graph(
            &wf,
            vec![task(&WorkflowId::new(), &node, &[])],
            "belongs to workflow",
        );
    }

    #[test]
    fn objective_round_trip_uses_validated_ids_description_and_deadline() {
        let spec = ObjectiveSpec {
            id: ObjectiveId::new(),
            owner_id: IdentityId::new(),
            description: "Read the workspace manifest".into(),
            priority: Priority::High,
            created_at_ns: 7,
            deadline_ns: Some(12),
            epoch: 5,
        };
        let objective = Objective::try_from(spec.clone()).unwrap();
        let wire = serde_json::to_value(&objective).unwrap();
        assert_eq!(wire, serde_json::to_value(&spec).unwrap());
        assert_eq!(
            serde_json::from_value::<Objective>(wire.clone()).unwrap(),
            objective
        );
        for (field, invalid) in [
            ("owner_id", json!(spec.id)),
            ("description", json!("  ")),
            ("deadline_ns", json!(6)),
        ] {
            let mut bad = wire.clone();
            bad[field] = invalid;
            assert!(serde_json::from_value::<Objective>(bad).is_err(), "{field}");
        }
    }

    #[test]
    fn task_contract_round_trip_and_nested_validation() {
        let wf = WorkflowId::new();
        let id = TaskId::new();
        let node = task(&wf, &id, &[]);
        let wire = serde_json::to_value(&node).unwrap();
        assert_eq!(serde_json::from_value::<Task>(wire.clone()).unwrap(), node);
        let plan = Workflow::new(wf, ObjectiveId::new(), 0, 0, [node]).unwrap();
        let plan_wire = serde_json::to_value(plan).unwrap();
        let mut bad_resource = wire["resource_requirements"].clone();
        bad_resource[0]["demand"] = json!("ExclusiveWrite");
        for (field, invalid) in [
            ("id", json!("task_bad")),
            ("workflow_id", json!(id)),
            ("capability", json!("")),
            ("operation", json!(" ")),
            ("dependencies", json!([id])),
            ("timeout_ms", json!(0)),
            ("resource_requirements", bad_resource),
            (
                "task_type",
                json!({"SubAgentSpawn": {"sub_agent_role": "", "prompt": "go"}}),
            ),
        ] {
            let mut bad = wire.clone();
            bad[field] = invalid;
            assert!(
                serde_json::from_value::<Task>(bad.clone()).is_err(),
                "{field}"
            );
            let mut nested = plan_wire.clone();
            nested["tasks"][0] = bad;
            assert!(
                serde_json::from_value::<Workflow>(nested).is_err(),
                "{field}"
            );
        }
    }
}
