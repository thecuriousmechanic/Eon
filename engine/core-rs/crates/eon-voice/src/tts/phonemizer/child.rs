use std::io;
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{OnceLock, mpsc};
use std::time::Duration;

use super::{EonResult, tts_err};

const MAX_CHILDREN: usize = 8;
static IN_USE: AtomicUsize = AtomicUsize::new(0);

struct Slot(&'static AtomicUsize);

impl Slot {
    fn reserve(count: &'static AtomicUsize) -> EonResult<Self> {
        let mut used = count.load(Ordering::Relaxed);
        while used < MAX_CHILDREN {
            match count.compare_exchange_weak(used, used + 1, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return Ok(Self(count)),
                Err(actual) => used = actual,
            }
        }
        Err(tts_err("phonemize", "all subprocess slots occupied"))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

// The child and its slot travel together. Even a failed kill or an
// uninterruptible child can consume at most one slot until confirmed reaped.
type Admitted = (Child, Slot);

pub(super) struct Running {
    live: Option<Admitted>,
    reaper: &'static mpsc::Sender<Admitted>,
}

impl Running {
    pub(super) fn spawn(command: &mut Command) -> EonResult<Self> {
        let slot = Slot::reserve(&IN_USE)?;
        let reaper = reaper()?;
        let child = command.spawn().map_err(|e| tts_err("phonemize.spawn", e))?;
        Ok(Self {
            live: Some((child, slot)),
            reaper,
        })
    }

    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if let Some((child, _)) = &mut self.live
            && let Some(status) = child.try_wait()?
        {
            self.live = None;
            return Ok(Some(status));
        }
        Ok(None)
    }

    pub(super) fn reaped(&self) -> bool {
        self.live.is_none()
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(super) fn id(&self) -> u32 {
        self.live.as_ref().unwrap().0.id()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some((mut child, slot)) = self.live.take() {
            let _ = child.kill();
            if !matches!(child.try_wait(), Ok(Some(_))) {
                // SIGKILL/TerminateProcess is not a synchronous wait.
                let _ = self.reaper.send((child, slot));
            }
        }
    }
}

// Created before spawning. Only MAX_CHILDREN slots exist, so the queue and
// pending list are bounded without ever blocking a submitter for capacity.
fn reaper() -> EonResult<&'static mpsc::Sender<Admitted>> {
    static REAPER: OnceLock<io::Result<mpsc::Sender<Admitted>>> = OnceLock::new();
    REAPER
        .get_or_init(|| {
            let (tx, rx) = mpsc::channel::<Admitted>();
            std::thread::Builder::new()
                .name("espeak-reaper".into())
                .spawn(move || {
                    while let Ok(child) = rx.recv() {
                        let mut children = vec![child];
                        while !children.is_empty() {
                            children.retain_mut(|(c, _)| !matches!(c.try_wait(), Ok(Some(_))));
                            if !children.is_empty() {
                                std::thread::sleep(Duration::from_millis(20));
                                children.extend(rx.try_iter());
                            }
                        }
                    }
                })?;
            Ok(tx)
        })
        .as_ref()
        .map_err(|e| tts_err("phonemize.reaper", e))
}

#[cfg(all(test, target_os = "linux"))]
#[test]
fn admission_is_fail_fast_and_cleanup_keeps_its_slot_until_reap() {
    use std::process::Stdio;
    use std::time::Instant;

    // Isolate this capacity exercise from parallel tests using the real pool.
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    let mut slots: Vec<_> = (0..MAX_CHILDREN).map(|_| Slot::reserve(&COUNT).unwrap()).collect();
    let mut child = Command::new("/bin/cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let held = child.stdin.take().unwrap(); // Simulate delayed OS teardown deterministically.
    reaper().unwrap().send((child, slots.pop().unwrap())).unwrap();
    assert!(Slot::reserve(&COUNT).is_err());
    drop(held);
    let deadline = Instant::now() + Duration::from_secs(2);
    while Slot::reserve(&COUNT).is_err() {
        assert!(Instant::now() < deadline, "reaping did not release its slot");
        std::thread::sleep(super::POLL);
    }
}
