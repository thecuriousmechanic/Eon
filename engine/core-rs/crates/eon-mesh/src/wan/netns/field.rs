//! Run the field executable unmodified, with stdin on a real PTY at the receiver.
use super::{Duration, fs, mark, media_enabled, sleep, timeout, wait, write_stale_record};
use rustix::{
    io::ioctl_fionread,
    pty::{OpenptFlags, ioctl_tiocgptpeer, openpt, unlockpt},
};
use std::{
    fs::File,
    io::Write,
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
};

struct Role(Child);

impl Drop for Role {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Role {
    fn status(&mut self) -> Option<ExitStatus> {
        assert!(!Path::new("abort").exists(), "field fixture aborted");
        self.0.try_wait().unwrap()
    }

    async fn finish(mut self) {
        loop {
            if let Some(status) = self.status() {
                assert!(status.success(), "field CLI: {status}");
                return;
            }
            sleep(Duration::from_millis(20)).await;
        }
    }

    async fn prompt(&mut self, text: &str, tty: &File) {
        timeout(Duration::from_secs(55), async {
            loop {
                assert!(self.status().is_none(), "receiver exited");
                if fs::read_to_string("field-prompts.log")
                    .unwrap()
                    .contains(text)
                    && ioctl_fionread(tty).unwrap() == 0
                {
                    return;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("missing prompt or terminal input not flushed");
    }
}

pub(super) async fn run(receiver: bool) {
    let (side, other, role) = if receiver {
        ("a", "b", "receive")
    } else {
        ("b", "a", "send")
    };
    let profile = Path::new("keys").canonicalize().unwrap().join(side);
    fs::create_dir(&profile).unwrap();
    fs::copy(format!("keys/{side}.key"), profile.join("identity.key")).unwrap();
    fs::copy(format!("keys/{other}.pub"), profile.join("peer.pin")).unwrap();
    fs::copy("keys/ca.der", profile.join("ca.der")).unwrap();
    fs::write(profile.join("relay.ip"), "10.77.0.1").unwrap();
    fs::write(profile.join("pkarr.ip"), "10.77.0.4").unwrap();
    let key = fs::read_to_string(profile.join("identity.key")).unwrap();
    write_stale_record(side, &key.trim().parse().unwrap());

    let mut cmd = Command::new(std::env::var_os("EON_FIELD_EXE").unwrap());
    cmd.arg(role).arg(profile).arg("30");
    if media_enabled() {
        cmd.arg("--redundant-media");
    }
    match std::env::var("EON_FIELD_TIMING").as_deref() {
        Ok("0") => {}
        Ok("1") => {
            cmd.arg("--timing");
        }
        _ => panic!("missing fixture timing policy"),
    }
    if !receiver {
        let sender = cmd
            .stdin(Stdio::null())
            .stdout(File::create("field-sent.log").unwrap())
            .spawn()
            .unwrap();
        Role(sender).finish().await;
        return;
    }
    let flags = OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC;
    let mut terminal = File::from(openpt(flags).unwrap());
    unlockpt(&terminal).unwrap();
    let tty = File::from(ioctl_tiocgptpeer(&terminal, flags).unwrap());
    // A nonempty stale line MUST be flushed. Reading it as an answer would
    // disarm the shipping CLI, not accidentally mark the cut.
    terminal.write_all(b"x\n").unwrap();
    let mut child = Role(
        cmd.stdin(tty.try_clone().unwrap())
            .stdout(File::create("field-receipts.log").unwrap())
            .stderr(File::create("field-prompts.log").unwrap())
            .spawn()
            .unwrap(),
    );
    child.prompt("BEGINNING", &tty).await;
    timeout(Duration::from_secs(10), async {
        wait("direct-traffic").await;
        terminal.write_all(b"\nx\n").unwrap();
        child.prompt("ENDS", &tty).await;
        mark("field-begin");
        wait("cut").await;
        terminal.write_all(b"\n").unwrap();
    })
    .await
    .expect("cut not installed within the pre-cut budget");
    child.finish().await;
}
