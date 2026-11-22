use super::grant::{FRAME, Grant, denied};
use super::*;
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags, SocketFlags,
    SocketType, recvmsg, socketpair, sockopt,
};
use std::io::IoSliceMut;
use std::os::fd::{AsFd, OwnedFd};
use std::process::Stdio;

fn receive(socket: &OwnedFd, pid: u32) -> io::Result<File> {
    let mut data = [0; FRAME.len()];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1), ScmCredentials(1))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let msg = recvmsg(
        socket,
        &mut [IoSliceMut::new(&mut data)],
        &mut control,
        RecvFlags::CMSG_CLOEXEC,
    )?;
    if msg.bytes != FRAME.len()
        || data != FRAME
        || msg
            .flags
            .intersects(ReturnFlags::CTRUNC | ReturnFlags::TRUNC)
    {
        return Err(denied());
    }
    let mut master = None;
    let mut authenticated = false;
    let mut valid = true;
    // Finish draining even after a bad header. All excess rights must be
    // closed, including those following padded, rejected SCM_CREDENTIALS.
    for cmsg in control.drain() {
        match cmsg {
            RecvAncillaryMessage::ScmCredentials(cred)
                if !authenticated
                    && cred.pid.as_raw_nonzero().get() as u32 == pid
                    && cred.uid == process::geteuid()
                    && cred.gid == process::getegid() =>
            {
                authenticated = true;
            }
            RecvAncillaryMessage::ScmRights(mut fds) if master.is_none() && fds.len() == 1 => {
                master = fds.next();
            }
            _ => valid = false,
        }
    }
    master
        .filter(|_| valid && authenticated)
        .map(File::from)
        .ok_or_else(denied)
}

fn no_trailing_packet(socket: &OwnedFd) -> io::Result<()> {
    // Called after exit. An empty datagram, unlike EOF, has SCM_CREDENTIALS
    // and consequently CTRUNC when received without a control buffer.
    match recvmsg(
        socket,
        &mut [IoSliceMut::new(&mut [0])],
        &mut RecvAncillaryBuffer::default(),
        RecvFlags::DONTWAIT,
    ) {
        Ok(msg) if msg.bytes == 0 && msg.flags.is_empty() => Ok(()),
        Err(rustix::io::Errno::AGAIN) => Ok(()),
        _ => Err(denied()),
    }
}

pub(super) fn acquire(grant: &Grant) -> io::Result<(File, File)> {
    let (rx, tx) = socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )?;
    sockopt::set_socket_passcred(&rx, true)?;
    let timeout = Duration::from_secs(2);
    sockopt::set_socket_timeout(&rx, sockopt::Timeout::Recv, Some(timeout))?;
    let mut cmd = Command::new(fd_path(&grant.image));
    cmd.arg0("eon-pty-allocator")
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(tx)
        .stderr(Stdio::null());
    let parent = process::getpid();
    // SAFETY: only child-side syscalls. Retain Command's spawn-error pipe and
    // image until exec. Even non-CLOEXEC host descriptors do not reach main.
    unsafe {
        cmd.pre_exec(move || {
            process::set_parent_process_death_signal(Some(Signal::KILL))?;
            if process::getppid() != Some(parent) {
                return Err(io::Error::from_raw_os_error(libc::ESRCH));
            }
            limit(Resource::Core, 0)?;
            limit(Resource::As, 64 * 1024 * 1024)?;
            cvt(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0))?;
            if libc::syscall(
                libc::SYS_close_range,
                3u32,
                u32::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            ) < 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    drop(cmd);
    // Authenticate while the child (even a zombie) still reserves its PID.
    // Never block waiting for EOF from an endpoint a host fork might hold.
    let deadline = Instant::now() + timeout;
    let result = receive(&rx, child.id()).and_then(|master| {
        loop {
            match child.try_wait()? {
                Some(status) if status.success() => break Ok(master),
                Some(_) => break Err(denied()),
                None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(1)),
                None => break Err(ErrorKind::TimedOut.into()),
            }
        }
    });
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let master = result?;
    no_trailing_packet(&rx)?;
    let mut locked: i32 = 0;
    let mut index: u32 = 0;
    // SAFETY: query the received descriptor before treating it as a PTY.
    unsafe {
        cvt(libc::ioctl(master.as_raw_fd(), TIOCGPTLCK, &mut locked))?;
        cvt(libc::ioctl(master.as_raw_fd(), TIOCGPTN, &mut index))?;
    }
    let pin = peer(&master, libc::O_PATH | libc::O_CLOEXEC)?;
    let m = pin.metadata()?;
    if locked != 1
        || m.dev() != grant.ptmx.metadata()?.dev()
        || !m.file_type().is_char_device()
        || (fs::major(m.rdev()), fs::minor(m.rdev())) != (136, index)
        || m.mode() & 0o7777 != 0o600
        || m.uid() != process::geteuid().as_raw()
        || m.gid() != process::getegid().as_raw()
    {
        return Err(denied());
    }
    let flags = fs::fcntl_getfl(master.as_fd())?;
    fs::fcntl_setfl(&master, flags | fs::OFlags::NONBLOCK)?;
    Ok((master, pin))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags, sendmsg};
    use std::io::IoSlice;

    #[test]
    fn only_one_authenticated_exact_frame_and_atomic_cloexec_right() {
        let pid = std::process::id();
        for (frame, count, passcred, expected, ok) in [
            (FRAME, 1, true, pid, true),
            (b"badmagic", 1, true, pid, false),
            (b"EON-PTY\x01+", 1, true, pid, false),
            (FRAME, 0, true, pid, false),
            (FRAME, 2, true, pid, false),
            (FRAME, 8, true, pid, false),
            (FRAME, 1, false, pid, false),
            (FRAME, 1, true, pid + 1, false),
        ] {
            let (rx, tx) = socketpair(
                AddressFamily::UNIX,
                SocketType::SEQPACKET,
                SocketFlags::CLOEXEC,
                None,
            )
            .unwrap();
            sockopt::set_socket_passcred(&rx, passcred).unwrap();
            let (file, mut witness) = std::os::unix::net::UnixStream::pair().unwrap();
            let fds = [file.as_fd(); 8];
            let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(8))];
            let mut control = SendAncillaryBuffer::new(&mut space);
            if count != 0 {
                assert!(control.push(SendAncillaryMessage::ScmRights(&fds[..count])));
            }
            sendmsg(
                &tx,
                &[IoSlice::new(frame)],
                &mut control,
                SendFlags::NOSIGNAL,
            )
            .unwrap();
            let received = receive(&rx, expected);
            assert_eq!(received.is_ok(), ok);
            if let Ok(fd) = received {
                assert!(
                    rustix::io::fcntl_getfd(fd)
                        .unwrap()
                        .contains(rustix::io::FdFlags::CLOEXEC)
                );
                assert!(no_trailing_packet(&rx).is_ok());
                for extra in [b"".as_slice(), FRAME] {
                    rustix::net::send(&tx, extra, SendFlags::NOSIGNAL).unwrap();
                    assert!(no_trailing_packet(&rx).is_err());
                }
                drop(tx);
                assert!(no_trailing_packet(&rx).is_ok());
            }
            // No installed rights survive *any* rejection (including CTRUNC).
            drop(file);
            // A parallel test can have forked with a transient pre-exec copy.
            witness
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            assert_eq!(witness.read(&mut [0]).unwrap(), 0);
        }
    }

    #[test]
    fn untouched_private_mount_allocates_0600_before_first_unlock() {
        let grant = Grant::pin().unwrap();
        // Independently observe what the *kernel* creates before grantpt,
        // unlockpt, chmod or chown. The defaults apply to upstream openpty too.
        let master: File = fs::open(
            "/dev/ptmx",
            fs::OFlags::RDWR | fs::OFlags::CLOEXEC | fs::OFlags::NOCTTY,
            fs::Mode::empty(),
        )
        .unwrap()
        .into();
        let pin = peer(&master, libc::O_PATH | libc::O_CLOEXEC).unwrap();
        let meta = pin.metadata().unwrap();
        let mut locked = 0_i32;
        // SAFETY: query on this fresh master writes a single integer.
        assert_eq!(
            unsafe { libc::ioctl(master.as_raw_fd(), TIOCGPTLCK, &mut locked) },
            0
        );
        assert_eq!(locked, 1);
        assert_eq!(meta.dev(), grant.ptmx.metadata().unwrap().dev());
        assert_eq!(
            (meta.mode() & 0o7777, meta.uid(), meta.gid()),
            (
                0o600,
                process::geteuid().as_raw(),
                process::getegid().as_raw()
            )
        );
        let (upstream, peer) = acquire(&grant).unwrap();
        assert_eq!(peer.metadata().unwrap().dev(), meta.dev());
        assert_ne!(peer.metadata().unwrap().ino(), meta.ino());
        assert!(
            rustix::io::fcntl_getfd(&upstream)
                .unwrap()
                .contains(rustix::io::FdFlags::CLOEXEC)
        );
        assert!(
            fs::fcntl_getfl(&upstream)
                .unwrap()
                .contains(fs::OFlags::NONBLOCK)
        );
    }

    #[test]
    #[ignore = "separate negative fixture: invalid devpts mode or altered installer digest"]
    fn bad_provisioning_is_refused_before_launch() {
        assert!(matches!(Grant::pin(), Err(e) if e.kind() == ErrorKind::PermissionDenied));
    }
}
