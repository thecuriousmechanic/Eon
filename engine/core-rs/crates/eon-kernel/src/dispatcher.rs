//! Blocking event loop for local workers. The scheduler stays single-owner; capability
//! work runs outside the loop. There is no polling interval and no timer per lease.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::Instant;

use eon_primitives::{EonError, EonResult};

use crate::events::EventSink;
use crate::model::Task;
use crate::scheduler::{Countermand, Dispatch, Outcome, TaskScheduler};

pub enum Command {
    Finish {
        dispatch: Dispatch,
        outcome: Outcome,
        reply: Sender<EonResult<()>>,
    },
    Countermand {
        epoch: u64,
        reply: Sender<EonResult<Countermand>>,
    },
}

/// Dispatch until nothing remains runnable, a countermand settles, or the sink/transport
/// fails. Quiescence does not imply workflow success: inspect task states for failed or
/// uncertain branches. The caller supplies worker and control channels scoped to this run.
pub fn drive<S: EventSink>(
    scheduler: &mut TaskScheduler<S>,
    workers: &Sender<Dispatch>,
    commands: &Receiver<Command>,
    mut undo: impl FnMut(&Task) -> EonResult<()>,
) -> EonResult<()> {
    // A transport failure, backwards-clock error, or unwinding sink must revoke any
    // worker-held permits even if the caller retains its borrowed scheduler.
    struct Guard<'a, S: EventSink> {
        scheduler: &'a mut TaskScheduler<S>,
        quiescent: bool,
    }
    impl<S: EventSink> Drop for Guard<'_, S> {
        fn drop(&mut self) {
            if !self.quiescent {
                self.scheduler.stop();
            }
        }
    }
    let mut guard = Guard {
        scheduler,
        quiescent: false,
    };
    let scheduler = &mut *guard.scheduler;
    let mut received = None;
    loop {
        // Check already-queued control messages before admitting a new batch.
        let command = match received.take() {
            some @ Some(_) => some,
            None => match commands.try_recv() {
                Ok(command) => Some(command),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => return Err(disconnected("command channel")),
            },
        };
        match command {
            None => {}
            Some(Command::Finish {
                dispatch,
                outcome,
                reply,
            }) => {
                let result = scheduler.finish(&dispatch, outcome, Instant::now());
                let _ = reply.send(result.clone());
                if scheduler.stopped() {
                    return result;
                }
            }
            Some(Command::Countermand { epoch, reply }) => {
                let result = scheduler.countermand(epoch, &mut undo);
                let status = result.as_ref().map(|_| ()).map_err(Clone::clone);
                let _ = reply.send(result);
                if scheduler.stopped() {
                    return status;
                }
            }
        }
        for dispatch in scheduler.tick(Instant::now())? {
            // A durable sink can be slow. Never send a grant already known to be expired.
            if dispatch.is_valid() && workers.send(dispatch).is_err() {
                return Err(disconnected("workers"));
            }
        }
        let Some(wait) = scheduler.wake_after(Instant::now()) else {
            guard.quiescent = true;
            return Ok(());
        };
        received = match commands.recv_timeout(wait) {
            Ok(command) => Some(command),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return Err(disconnected("command channel")),
        };
    }
}

fn disconnected(channel: &str) -> EonError {
    EonError::OperationFailed {
        operation: "dispatch".into(),
        reason: format!("{channel} disconnected with outstanding work; reconciliation required"),
    }
}
