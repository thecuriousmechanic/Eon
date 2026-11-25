//! Group-commit journal worker (docs/implementation/01, section 3.1).
//!
//! Callers enqueue payloads from any thread. One background thread drains everything queued at that
//! moment and writes it with a single `write` and a single `sync_data`. Each caller is acknowledged only
//! after that sync returns, so an acknowledged sequence number is durable.

use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread::{self, JoinHandle};

use eon_primitives::{EonError, EonResult};

use crate::journal::{JournalWriter, MAX_PAYLOAD_LEN};

/// Upper bound on frames per commit, so one burst cannot hold acknowledgements back indefinitely.
pub const MAX_BATCH: usize = 512;

struct Request {
    payload: Vec<u8>,
    reply: Sender<EonResult<u64>>,
}

pub struct GroupCommitJournal {
    directory: std::sync::Arc<crate::owned_dir::OwnedDirectory>,
    tx: Option<SyncSender<Request>>,
    worker: Option<JoinHandle<()>>,
}

impl GroupCommitJournal {
    /// Opens the journal in `dir` (repairing a torn tail, as `JournalWriter::open` does) and starts the worker.
    pub fn spawn(dir: impl AsRef<std::path::Path>, queue_depth: usize) -> EonResult<Self> {
        let mut writer = JournalWriter::open(dir)?;
        let directory = writer.dir.clone();
        let (tx, rx) = mpsc::sync_channel::<Request>(queue_depth.max(1));
        let worker = thread::Builder::new()
            .name("eon-journal".into())
            .spawn(move || run(&mut writer, rx))
            .map_err(|e| EonError::InternalError(format!("cannot start journal worker: {e}")))?;
        Ok(Self {
            directory,
            tx: Some(tx),
            worker: Some(worker),
        })
    }

    pub fn directory(&self) -> &crate::owned_dir::OwnedDirectory {
        &self.directory
    }

    /// Enqueues a payload. The receiver yields the assigned sequence number once it is durable.
    pub fn append(&self, payload: Vec<u8>) -> EonResult<Receiver<EonResult<u64>>> {
        if payload.len() > MAX_PAYLOAD_LEN {
            return Err(EonError::ValidationError(format!(
                "journal payload of {} bytes exceeds {MAX_PAYLOAD_LEN}",
                payload.len()
            )));
        }
        let (reply, ack) = mpsc::channel();
        let tx = self
            .tx
            .as_ref()
            .ok_or_else(|| EonError::InternalError("journal is shutting down".into()))?;
        tx.send(Request { payload, reply })
            .map_err(|_| EonError::InternalError("journal worker stopped".into()))?;
        Ok(ack)
    }

    /// Scrubs recognized secret patterns and invisible characters before journaling free text.
    /// Do not rely on text heuristics to protect arbitrary credentials.
    pub fn append_scrubbed(&self, text: &str) -> EonResult<u64> {
        self.append_durable(crate::scrubber::scrub(text).into_bytes())
    }

    /// Enqueues and waits for durability.
    pub fn append_durable(&self, payload: Vec<u8>) -> EonResult<u64> {
        self.append(payload)?
            .recv()
            .map_err(|_| EonError::InternalError("journal worker dropped the request".into()))?
    }
}

impl Drop for GroupCommitJournal {
    fn drop(&mut self) {
        drop(self.tx.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run(writer: &mut JournalWriter, rx: Receiver<Request>) {
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        while batch.len() < MAX_BATCH {
            match rx.try_recv() {
                Ok(next) => batch.push(next),
                Err(_) => break,
            }
        }
        let payloads: Vec<&[u8]> = batch.iter().map(|r| r.payload.as_slice()).collect();
        match writer.append_batch(&payloads) {
            Ok(seqs) => {
                for (request, seq) in batch.iter().zip(seqs) {
                    let _ = request.reply.send(Ok(seq));
                }
            }
            Err(err) => {
                for request in &batch {
                    let _ = request.reply.send(Err(err.clone()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::replay;

    #[test]
    fn concurrent_producers_get_unique_contiguous_durable_sequences() {
        let dir = tempfile::tempdir().unwrap();
        let journal = std::sync::Arc::new(GroupCommitJournal::spawn(dir.path(), 64).unwrap());
        let handles: Vec<_> = (0..8)
            .map(|t| {
                let journal = journal.clone();
                thread::spawn(move || {
                    (0..200)
                        .map(|i| journal.append_durable(format!("t{t}-{i}").into_bytes()).unwrap())
                        .collect::<Vec<u64>>()
                })
            })
            .collect();
        let mut seqs: Vec<u64> = handles.into_iter().flat_map(|h| h.join().unwrap()).collect();
        seqs.sort_unstable();
        assert_eq!(seqs, (1..=1600).collect::<Vec<u64>>());
        drop(journal);
        assert_eq!(replay(dir.path()).unwrap().len(), 1600);
    }

    #[test]
    fn acknowledged_events_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        {
            let journal = GroupCommitJournal::spawn(dir.path(), 8).unwrap();
            assert_eq!(journal.append_durable(b"first".to_vec()).unwrap(), 1);
            assert_eq!(journal.append_durable(b"second".to_vec()).unwrap(), 2);
        }
        let journal = GroupCommitJournal::spawn(dir.path(), 8).unwrap();
        assert_eq!(journal.append_durable(b"third".to_vec()).unwrap(), 3);
        drop(journal);
        let payloads: Vec<Vec<u8>> = replay(dir.path()).unwrap().into_iter().map(|f| f.payload).collect();
        assert_eq!(payloads, vec![b"first".to_vec(), b"second".to_vec(), b"third".to_vec()]);
    }

    #[test]
    fn scrubbed_appends_never_persist_the_raw_secret() {
        use crate::journal::HEADER_LEN;
        use crate::scrubber::redaction_marker;

        let token_body = "paper-deploy-token";
        let token = format!("ghp_{token_body}");
        let segments = ["paperheader", "paperclaims", "papersignature"];
        let bearer = ["Bearer", &segments.join(".")].join(" ");
        let key_lines = ["paper-key-line-one", "paper-key-line-two"];
        let label = "PRIVATE KEY";
        let key = format!("-----BEGIN {label}-----\r\n{}\r\n-----END {label}-----", key_lines.join("\r\n"));
        let hex40 = "0123456789abcdef".repeat(2) + "01234567";
        let hex64 = "FEDCBA9876543210".repeat(4);
        let digest = "02468ace13579bdf".repeat(4);
        let hashes = format!("sha256:{digest}\ncommit {}", &digest[..40]);
        let input = format!("deploy {token}\nauth: {bearer}\nkey: {key}\ntoken={hex40}\n{hex64}\n{hashes}");
        let expected = format!(
            "deploy {}\nauth: {}\nkey: {}\ntoken={}\n{}\n{hashes}",
            redaction_marker(&token), redaction_marker(&bearer), redaction_marker(&key),
            redaction_marker(&hex40), redaction_marker(&hex64),
        );

        let dir = tempfile::tempdir().unwrap();
        let journal = GroupCommitJournal::spawn(dir.path(), 8).unwrap();
        let seq = journal.append_scrubbed(&input).unwrap();
        drop(journal);

        let disk = std::fs::read(dir.path().join(format!("seg_{seq:020}.eonj"))).unwrap();
        for raw in [token_body, &hex40, &hex64].into_iter().chain(segments).chain(key_lines) {
            assert!(!disk.windows(raw.len()).any(|w| w == raw.as_bytes()), "stored fixture: {raw}");
        }
        assert_eq!(&disk[HEADER_LEN..], expected.as_bytes());
        let frames = replay(dir.path()).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].seq_no, seq);
        assert_eq!(frames[0].payload, expected.as_bytes());
    }

    #[test]
    fn oversized_payload_is_rejected_before_queueing() {
        let dir = tempfile::tempdir().unwrap();
        let journal = GroupCommitJournal::spawn(dir.path(), 8).unwrap();
        assert!(matches!(
            journal.append(vec![0u8; MAX_PAYLOAD_LEN + 1]),
            Err(EonError::ValidationError(_))
        ));
        assert_eq!(journal.append_durable(b"ok".to_vec()).unwrap(), 1, "queue must still work");
    }
}
