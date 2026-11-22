//! Durable kernel events: encodes `KernelEvent` into journal payloads and writes them through the group-commit
//! journal (docs/implementation/01, sections 3.1 and 4).
//!
//! Tagged tab-separated payloads: task transition (`T`), splice (`S`), epoch writes (`E`),
//! workflow submission (`W`), and epoch invalidation (`I`).

use std::collections::BTreeMap;

use eon_foundation::group_commit::GroupCommitJournal;
use eon_primitives::{EonError, EonResult, TaskId};

use crate::events::{EventSink, KernelEvent};
use crate::fsm::TaskState;

fn state_name(state: TaskState) -> &'static str {
    match state {
        TaskState::Pending => "pending",
        TaskState::Ready => "ready",
        TaskState::Running => "running",
        TaskState::Completed => "completed",
        TaskState::Failed => "failed",
        TaskState::Compensating => "compensating",
        TaskState::Compensated => "compensated",
        TaskState::UncertainFailure => "uncertain_failure",
    }
}

fn parse_state(name: &str) -> EonResult<TaskState> {
    Ok(match name {
        "pending" => TaskState::Pending,
        "ready" => TaskState::Ready,
        "running" => TaskState::Running,
        "completed" => TaskState::Completed,
        "failed" => TaskState::Failed,
        "compensating" => TaskState::Compensating,
        "compensated" => TaskState::Compensated,
        "uncertain_failure" => TaskState::UncertainFailure,
        other => return Err(corrupt(format!("unknown state '{other}'"))),
    })
}

fn corrupt(reason: String) -> EonError {
    EonError::InternalError(format!("corrupt kernel event: {reason}"))
}

/// Escapes tab, newline, and backslash so free-text keys and values cannot break the field layout.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

fn unescape(text: &str) -> EonResult<String> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            other => return Err(corrupt(format!("bad escape {other:?}"))),
        }
    }
    Ok(out)
}

pub fn encode(event: &KernelEvent) -> Vec<u8> {
    let line = match event {
        KernelEvent::WorkflowSubmitted { workflow, epoch, digest, tasks } => {
            let mut fields = vec!["W".to_owned(), workflow.to_string(), epoch.to_string(),
                blake3::Hash::from_bytes(*digest).to_hex().to_string()];
            for (id, idempotent) in tasks {
                fields.push(id.to_string());
                fields.push(u8::from(*idempotent).to_string());
            }
            fields.join("\t")
        }
        KernelEvent::EpochInvalidated { workflow, epoch } => format!("I\t{workflow}\t{epoch}"),
        KernelEvent::TaskTransition { task, from, to } => {
            format!("T\t{task}\t{}\t{}", state_name(*from), state_name(*to))
        }
        KernelEvent::SubgraphSpliced { parent, added } => {
            let children: Vec<String> = added.iter().map(|id| id.to_string()).collect();
            format!("S\t{parent}\t{}", children.join(","))
        }
        KernelEvent::EpochCommitted { id, writes } => {
            let mut fields = vec!["E".to_owned(), id.to_string()];
            for (key, value) in writes {
                fields.push(escape(key));
                fields.push(escape(value));
            }
            fields.join("\t")
        }
    };
    line.into_bytes()
}

pub fn decode(bytes: &[u8]) -> EonResult<KernelEvent> {
    let text = std::str::from_utf8(bytes).map_err(|e| corrupt(e.to_string()))?;
    let fields: Vec<&str> = text.split('\t').collect();
    match fields.as_slice() {
        ["W", workflow, epoch, digest, rest @ ..] => {
            if digest.bytes().any(|b| b.is_ascii_uppercase()) {
                return Err(corrupt("non-canonical workflow digest".into()));
            }
            let digest = blake3::Hash::from_hex(digest).map_err(|e| corrupt(e.to_string()))?;
            let (pairs, leftover) = rest.as_chunks::<2>();
            if !leftover.is_empty() {
                return Err(corrupt("submission has an unpaired task".into()));
            }
            let mut tasks = BTreeMap::new();
            for pair in pairs {
                let id = pair[0].parse::<TaskId>().map_err(|e| corrupt(e.to_string()))?;
                let idempotent = match pair[1] {
                    "1" => true,
                    "0" => false,
                    _ => return Err(corrupt("invalid idempotency flag".into())),
                };
                if tasks.insert(id, idempotent).is_some() {
                    return Err(corrupt("duplicate task in submission".into()));
                }
            }
            Ok(KernelEvent::WorkflowSubmitted {
                workflow: workflow.parse().map_err(|e: EonError| corrupt(e.to_string()))?,
                epoch: epoch.parse::<u64>().map_err(|e| corrupt(e.to_string()))?,
                digest: *digest.as_bytes(),
                tasks,
            })
        }
        ["I", workflow, epoch] => Ok(KernelEvent::EpochInvalidated {
            workflow: workflow.parse().map_err(|e: EonError| corrupt(e.to_string()))?,
            epoch: epoch.parse::<u64>().map_err(|e| corrupt(e.to_string()))?,
        }),
        ["T", task, from, to] => Ok(KernelEvent::TaskTransition {
            task: task.parse::<TaskId>().map_err(|e| corrupt(e.to_string()))?,
            from: parse_state(from)?,
            to: parse_state(to)?,
        }),
        ["S", parent, children] => {
            let added = if children.is_empty() {
                Vec::new()
            } else {
                children
                    .split(',')
                    .map(|id| id.parse::<TaskId>().map_err(|e| corrupt(e.to_string())))
                    .collect::<EonResult<Vec<_>>>()?
            };
            Ok(KernelEvent::SubgraphSpliced {
                parent: parent.parse::<TaskId>().map_err(|e| corrupt(e.to_string()))?,
                added,
            })
        }
        ["E", id, rest @ ..] => {
            if rest.len() % 2 != 0 {
                return Err(corrupt("epoch record has an unpaired key".into()));
            }
            let mut writes = BTreeMap::new();
            for pair in rest.chunks(2) {
                writes.insert(unescape(pair[0])?, unescape(pair[1])?);
            }
            Ok(KernelEvent::EpochCommitted {
                id: id.parse::<u64>().map_err(|e| corrupt(e.to_string()))?,
                writes,
            })
        }
        _ => Err(corrupt(format!("unrecognized record with {} fields", fields.len()))),
    }
}

/// Writes each event to the group-commit journal and returns only after the event is fsynced.
pub struct JournalSink<'a> {
    journal: &'a GroupCommitJournal,
}

impl<'a> JournalSink<'a> {
    pub fn new(journal: &'a GroupCommitJournal) -> Self {
        Self { journal }
    }
}

impl EventSink for JournalSink<'_> {
    fn record(&mut self, event: KernelEvent) -> EonResult<()> {
        // Callers can construct an event without going through SharedState::commit_recorded.
        if let KernelEvent::EpochCommitted { writes, .. } = &event {
            for field in writes.keys().chain(writes.values()) {
                if eon_foundation::scrubber::scrub(field) != *field {
                    return Err(EonError::SecurityViolation("unscrubbed epoch field".into()));
                }
            }
        }
        self.journal.append_durable(encode(&event)).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use eon_foundation::journal::replay;

    use super::*;
    use crate::events::{complete_recorded, splice_recorded, transition_recorded};
    use crate::fsm::TaskEvent;
    use crate::workflow::{Task, Workflow};

    #[test]
    fn direct_epoch_records_cannot_bypass_field_scrubbing() {
        let dir = tempfile::tempdir().unwrap();
        let journal = GroupCommitJournal::spawn(dir.path(), 8).unwrap();
        let mut sink = JournalSink::new(&journal);
        let fixture = ["ghp_", "fixture"].concat();
        for (key, value) in [(&fixture[..], "value"), ("key", &fixture[..])] {
            let event = KernelEvent::EpochCommitted {
                id: 1,
                writes: BTreeMap::from([(key.to_owned(), value.to_owned())]),
            };
            assert!(matches!(sink.record(event), Err(EonError::SecurityViolation(_))));
        }
        drop(journal);
        assert!(replay(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn every_event_kind_round_trips_through_its_encoding() {
        let a = TaskId::new();
        let b = TaskId::new();
        let events = [
            KernelEvent::TaskTransition { task: a.clone(), from: TaskState::Ready, to: TaskState::Running },
            KernelEvent::SubgraphSpliced { parent: a.clone(), added: vec![b.clone()] },
            KernelEvent::SubgraphSpliced { parent: a.clone(), added: vec![] },
            KernelEvent::EpochCommitted {
                id: 7,
                writes: BTreeMap::from([
                    ("back\\slash".to_owned(), "tab\there\nnewline".to_owned()),
                    (String::new(), String::new()),
                ]),
            },
        ];
        for event in events {
            assert_eq!(decode(&encode(&event)).unwrap(), event);
        }
    }

    #[test]
    fn transition_state_names_and_discriminants_are_stable() {
        use TaskState::*;
        for (state, name, discriminant) in [
            (Pending, "pending", 0),
            (Ready, "ready", 1),
            (Running, "running", 2),
            (Completed, "completed", 3),
            (Failed, "failed", 4),
            (Compensating, "compensating", 5),
            (Compensated, "compensated", 6),
            (UncertainFailure, "uncertain_failure", 7),
        ] {
            assert_eq!(state as usize, discriminant);
            assert_eq!(state_name(state), name);
            assert_eq!(parse_state(name).unwrap(), state);
        }

        // Existing workflow journals use this shortcut rather than the FSM's intermediate states.
        let task = TaskId::new();
        let legacy = format!("T\t{task}\tpending\tcompleted");
        let event = KernelEvent::TaskTransition { task, from: Pending, to: Completed };
        assert_eq!(decode(legacy.as_bytes()).unwrap(), event);
        assert_eq!(encode(&event), legacy.as_bytes());
    }

    #[test]
    fn durable_replay_stops_at_the_unverified_outcome() {
        use TaskState::*;
        let task = TaskId::new();
        let dir = tempfile::tempdir().unwrap();
        {
            let journal = GroupCommitJournal::spawn(dir.path(), 8).unwrap();
            let mut sink = JournalSink::new(&journal);
            let mut state = Pending;
            for event in [TaskEvent::DependenciesMet, TaskEvent::Start, TaskEvent::OutcomeUncertain] {
                transition_recorded(&mut state, &task, event, &mut sink).unwrap();
            }
            assert!(transition_recorded(&mut state, &task, TaskEvent::Start, &mut sink).is_err());
        }

        let frames = replay(dir.path()).unwrap();
        let expected = [(Pending, Ready), (Ready, Running), (Running, UncertainFailure)];
        assert_eq!(frames.len(), expected.len());
        for (frame, (from, to)) in frames.iter().zip(expected) {
            assert_eq!(
                decode(&frame.payload).unwrap(),
                KernelEvent::TaskTransition { task: task.clone(), from, to }
            );
        }
    }

    #[test]
    fn corrupt_records_are_refused() {
        assert!(decode(b"X\tnope").is_err());
        assert!(decode(b"T\ttask_bad\tpending\tcompleted").is_err());
        assert!(decode(b"T\t01HZ\tbogus\tcompleted").is_err());
        assert!(decode(&[0xff, 0xfe]).is_err());
    }

    #[test]
    fn completion_and_splice_are_durable_and_replayable() {
        let dir = tempfile::tempdir().unwrap();
        let journal = GroupCommitJournal::spawn(dir.path(), 16).unwrap();
        let mut wf = Workflow::new();
        let parent = TaskId::new();
        let child = TaskId::new();
        wf.insert(Task::new(parent.clone(), [])).unwrap();

        complete_recorded(&mut wf, &parent, &mut JournalSink::new(&journal)).unwrap();
        splice_recorded(
            &mut wf,
            &parent,
            vec![Task::new(child.clone(), [])],
            &mut JournalSink::new(&journal),
        )
        .unwrap();
        drop(journal);

        let decoded: Vec<KernelEvent> = replay(dir.path())
            .unwrap()
            .into_iter()
            .map(|frame| decode(&frame.payload).unwrap())
            .collect();
        assert_eq!(
            decoded,
            vec![
                KernelEvent::TaskTransition {
                    task: parent.clone(),
                    from: TaskState::Pending,
                    to: TaskState::Completed,
                },
                KernelEvent::SubgraphSpliced { parent, added: vec![child] },
            ]
        );
    }
}
