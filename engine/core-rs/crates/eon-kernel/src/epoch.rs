//! Cognitive epochs with staged commit and fencing (docs/implementation/01, section 4).
//!
//! An epoch stages writes against the shared state. A commit applies them atomically and closes the epoch. A
//! rollback discards them. A newer epoch fences older ones, so a slow, retried, or replayed cognition step cannot
//! overwrite state that a later epoch already committed.

use std::collections::BTreeMap;

use eon_foundation::scrubber::scrub;
use eon_primitives::{EonError, EonResult};

use crate::events::{EventSink, KernelEvent};

#[derive(Debug, Default)]
pub struct SharedState {
    values: BTreeMap<String, String>,
    /// Highest epoch that has committed. Any epoch at or below this is fenced.
    fence: u64,
}

impl SharedState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    pub fn fence(&self) -> u64 {
        self.fence
    }

    /// Opens a staging area for epoch `id`. Opening an epoch at or below the fence is refused.
    pub fn begin(&self, id: u64) -> EonResult<CognitiveEpoch> {
        if id <= self.fence {
            return Err(EonError::ValidationError(format!(
                "epoch {id} is fenced by committed epoch {}",
                self.fence
            )));
        }
        Ok(CognitiveEpoch {
            id,
            staged: BTreeMap::new(),
            closed: false,
        })
    }

    /// Applies an epoch's staged writes atomically. Refused if a newer epoch has already committed.
    pub fn commit(&mut self, epoch: &mut CognitiveEpoch) -> EonResult<()> {
        self.ensure_committable(epoch)?;
        let writes = std::mem::take(&mut epoch.staged);
        self.apply(epoch, writes);
        Ok(())
    }

    /// Commits scrubbed values through `sink` before applying them. Keys are identifiers: a key
    /// changed by scrubbing could alias another epoch's key and is rejected, not renamed.
    /// Validation or recording failure leaves the epoch open and applies nothing.
    pub fn commit_recorded(&mut self, epoch: &mut CognitiveEpoch, sink: &mut dyn EventSink) -> EonResult<()> {
        self.ensure_committable(epoch)?;
        let mut writes = BTreeMap::new();
        for (key, value) in &epoch.staged {
            if scrub(key) != *key {
                return Err(EonError::ValidationError("epoch key must not require scrubbing".into()));
            }
            writes.insert(key.clone(), scrub(value));
        }
        sink.record(KernelEvent::EpochCommitted {
            id: epoch.id,
            writes: writes.clone(),
        })?;
        self.apply(epoch, writes);
        Ok(())
    }

    /// Rebuilds state from recorded events in journal order. Other event kinds are ignored. An epoch that does not
    /// advance the fence means the log is corrupt, so replay refuses it.
    pub fn replay(events: &[KernelEvent]) -> EonResult<Self> {
        let mut state = Self::new();
        for event in events {
            if let KernelEvent::EpochCommitted { id, writes } = event {
                if *id <= state.fence {
                    return Err(EonError::InternalError(format!(
                        "epoch {id} does not advance past fence {}; the log is corrupt",
                        state.fence
                    )));
                }
                state.values.extend(writes.iter().map(|(k, v)| (k.clone(), v.clone())));
                state.fence = *id;
            }
        }
        Ok(state)
    }

    fn ensure_committable(&self, epoch: &mut CognitiveEpoch) -> EonResult<()> {
        if epoch.closed {
            return Err(EonError::ValidationError(format!("epoch {} is already closed", epoch.id)));
        }
        if epoch.id <= self.fence {
            epoch.closed = true;
            return Err(EonError::ValidationError(format!(
                "epoch {} was fenced by committed epoch {} before commit",
                epoch.id, self.fence
            )));
        }
        Ok(())
    }

    fn apply(&mut self, epoch: &mut CognitiveEpoch, writes: BTreeMap<String, String>) {
        self.values.extend(writes);
        epoch.staged.clear();
        self.fence = epoch.id;
        epoch.closed = true;
    }
}

#[derive(Debug)]
pub struct CognitiveEpoch {
    id: u64,
    staged: BTreeMap<String, String>,
    closed: bool,
}

impl CognitiveEpoch {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Stages a write. Nothing is visible to other readers until commit.
    pub fn stage(&mut self, key: impl Into<String>, value: impl Into<String>) -> EonResult<()> {
        if self.closed {
            return Err(EonError::ValidationError(format!("epoch {} is closed", self.id)));
        }
        self.staged.insert(key.into(), value.into());
        Ok(())
    }

    /// Discards every staged write and closes the epoch.
    pub fn rollback(&mut self) {
        self.staged.clear();
        self.closed = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_writes_are_invisible_until_commit() {
        let mut state = SharedState::new();
        let mut epoch = state.begin(1).unwrap();
        epoch.stage("plan", "draft").unwrap();
        assert_eq!(state.get("plan"), None);
        state.commit(&mut epoch).unwrap();
        assert_eq!(state.get("plan"), Some("draft"));
    }

    #[test]
    fn rollback_discards_staged_writes() {
        let mut state = SharedState::new();
        let mut epoch = state.begin(1).unwrap();
        epoch.stage("plan", "bad").unwrap();
        epoch.rollback();
        assert_eq!(state.get("plan"), None);
        assert!(state.commit(&mut epoch).is_err(), "a rolled-back epoch cannot commit");
    }

    #[test]
    fn a_later_commit_fences_an_earlier_epoch() {
        let mut state = SharedState::new();
        let mut slow = state.begin(1).unwrap();
        let mut fast = state.begin(2).unwrap();
        slow.stage("answer", "stale").unwrap();
        fast.stage("answer", "fresh").unwrap();
        state.commit(&mut fast).unwrap();
        assert!(state.commit(&mut slow).is_err());
        assert_eq!(state.get("answer"), Some("fresh"), "the stale epoch must not overwrite");
    }

    #[test]
    fn a_replayed_epoch_id_cannot_be_reopened_after_commit() {
        let mut state = SharedState::new();
        let mut epoch = state.begin(5).unwrap();
        state.commit(&mut epoch).unwrap();
        assert!(state.begin(5).is_err());
        assert!(state.begin(4).is_err());
        assert!(state.begin(6).is_ok());
    }

    #[test]
    fn a_closed_epoch_rejects_further_writes_and_double_commit() {
        let mut state = SharedState::new();
        let mut epoch = state.begin(1).unwrap();
        state.commit(&mut epoch).unwrap();
        assert!(epoch.stage("x", "y").is_err());
        assert!(state.commit(&mut epoch).is_err());
    }

    fn recorded_epochs(dir: &std::path::Path) -> Vec<KernelEvent> {
        eon_foundation::journal::replay(dir)
            .unwrap()
            .iter()
            .map(|frame| crate::journal_sink::decode(&frame.payload).unwrap())
            .collect()
    }

    #[test]
    fn recorded_commits_survive_restart_and_rebuild_the_same_state() {
        use eon_foundation::group_commit::GroupCommitJournal;

        let dir = tempfile::tempdir().unwrap();
        let mut state = SharedState::new();
        {
            let journal = GroupCommitJournal::spawn(dir.path(), 16).unwrap();
            let mut sink = crate::journal_sink::JournalSink::new(&journal);
            let mut first = state.begin(1).unwrap();
            first.stage("plan", "draft\twith\ttabs\nand a newline").unwrap();
            state.commit_recorded(&mut first, &mut sink).unwrap();
            let mut second = state.begin(2).unwrap();
            second.stage("answer", "42").unwrap();
            state.commit_recorded(&mut second, &mut sink).unwrap();
        }

        let rebuilt = SharedState::replay(&recorded_epochs(dir.path())).unwrap();
        assert_eq!(rebuilt.fence(), 2);
        assert_eq!(rebuilt.get("plan"), state.get("plan"));
        assert_eq!(rebuilt.get("answer"), Some("42"));
        assert_eq!(rebuilt.get("plan"), Some("draft\twith\ttabs\nand a newline"));
    }

    #[test]
    fn secrets_are_scrubbed_before_reaching_the_log_or_state() {
        use eon_foundation::group_commit::GroupCommitJournal;

        let secret = "ghp_FIXTURE-not-a-real-token";
        let dir = tempfile::tempdir().unwrap();
        let mut state = SharedState::new();
        {
            let journal = GroupCommitJournal::spawn(dir.path(), 16).unwrap();
            let mut sink = crate::journal_sink::JournalSink::new(&journal);
            let mut epoch = state.begin(1).unwrap();
            epoch.stage("note", format!("deploy with {secret} now")).unwrap();
            state.commit_recorded(&mut epoch, &mut sink).unwrap();
        }

        assert!(!state.get("note").unwrap().contains(secret));
        assert!(state.get("note").unwrap().contains("REDACTED"));
        for frame in eon_foundation::journal::replay(dir.path()).unwrap() {
            assert!(!String::from_utf8_lossy(&frame.payload).contains(secret), "raw secret reached the log");
        }
    }

    struct FailingSink;
    impl EventSink for FailingSink {
        fn record(&mut self, _: KernelEvent) -> EonResult<()> {
            Err(EonError::OperationFailed {
                operation: "test.sink".into(),
                reason: "disk full".into(),
            })
        }
    }

    #[test]
    fn failed_recording_applies_nothing_and_leaves_the_epoch_open() {
        let mut state = SharedState::new();
        let mut epoch = state.begin(1).unwrap();
        epoch.stage("plan", "draft").unwrap();
        assert!(state.commit_recorded(&mut epoch, &mut FailingSink).is_err());
        assert_eq!(state.get("plan"), None);
        assert_eq!(state.fence(), 0);
        state.commit(&mut epoch).unwrap();
        assert_eq!(state.get("plan"), Some("draft"));
    }

    #[test]
    fn scrubbing_must_not_silently_overwrite_another_staged_key() {
        let mut state = SharedState::new();
        let mut epoch = state.begin(1).unwrap();
        let mut sink = crate::events::VecSink::default();
        epoch.stage("label", "one").unwrap();
        epoch.stage("la\u{200b}bel", "two").unwrap();
        assert!(matches!(
            state.commit_recorded(&mut epoch, &mut sink),
            Err(EonError::ValidationError(_))
        ));
        assert!(sink.events.is_empty());
        assert_eq!(state.fence(), 0);
        assert_eq!(state.get("label"), None);
        assert!(epoch.stage("still open", "yes").is_ok());
    }

    #[test]
    fn a_later_epoch_can_update_a_key_but_cannot_alias_it() {
        let mut state = SharedState::new();
        let mut sink = crate::events::VecSink::default();
        let mut first = state.begin(1).unwrap();
        first.stage("label", "one").unwrap();
        state.commit_recorded(&mut first, &mut sink).unwrap();

        let mut alias = state.begin(2).unwrap();
        alias.stage("la\u{200b}bel", "two").unwrap();
        assert!(matches!(state.commit_recorded(&mut alias, &mut sink), Err(EonError::ValidationError(_))));
        assert_eq!(sink.events.len(), 1);
        assert_eq!(state.fence(), 1);
        assert_eq!(state.get("label"), Some("one"));

        let mut update = state.begin(2).unwrap();
        update.stage("label", "three").unwrap();
        state.commit_recorded(&mut update, &mut sink).unwrap();
        assert_eq!(SharedState::replay(&sink.events).unwrap().get("label"), Some("three"));
    }

    #[test]
    fn replay_refuses_an_epoch_that_does_not_advance_the_fence() {
        let writes = BTreeMap::from([("k".to_owned(), "v".to_owned())]);
        let events = [
            KernelEvent::EpochCommitted { id: 3, writes: writes.clone() },
            KernelEvent::EpochCommitted { id: 3, writes },
        ];
        assert!(SharedState::replay(&events).is_err());
    }
}
