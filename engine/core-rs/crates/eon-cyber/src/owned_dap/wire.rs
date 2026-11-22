use super::{Error, Result};
use serde_json::{Value, json};
use std::io::{self, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

const MAX_BODY: usize = 64 * 1024;
const MAX_MESSAGES: u8 = 48;

fn io_error(e: io::Error) -> Error {
    match e.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => Error::Deadline,
        _ => Error::Transport,
    }
}

fn length(header: &[u8]) -> Result<usize> {
    let digits = header
        .strip_prefix(b"Content-Length: ")
        .and_then(|s| s.strip_suffix(b"\r\n\r\n"))
        .filter(|s| !s.is_empty() && s.iter().all(u8::is_ascii_digit))
        .ok_or(Error::Protocol)?;
    std::str::from_utf8(digits)
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|n| (1..=MAX_BODY).contains(n))
        .ok_or(Error::Protocol)
}

#[derive(Default)]
pub(super) struct Events {
    pub initialized: bool,
    pub launched: bool,
    pub stop: Option<(u64, u64)>,
    resume_sent: bool,
    exited: bool,
    pub terminated: bool,
}

impl Events {
    fn accept(&mut self, msg: &Value) -> Result<()> {
        let body = &msg["body"];
        match msg["event"].as_str() {
            Some("initialized") if !self.initialized => self.initialized = true,
            Some("stopped")
                if self.stop.is_none() && !self.exited && body["reason"] == "breakpoint" =>
            {
                let thread = body["threadId"]
                    .as_u64()
                    .filter(|n| (1..=i32::MAX as u64).contains(n))
                    .ok_or(Error::Protocol)?;
                let ids = body["hitBreakpointIds"]
                    .as_array()
                    .filter(|v| v.len() == 1)
                    .ok_or(Error::Protocol)?;
                self.stop = Some((thread, ids[0].as_u64().ok_or(Error::Protocol)?));
            }
            Some("exited") if self.resume_sent && !self.exited && body["exitCode"] == 0 => {
                self.exited = true
            }
            Some("terminated") if self.exited && !self.terminated => self.terminated = true,
            Some("process" | "module" | "output" | "breakpoint" | "thread" | "continued") => (),
            _ => return Err(Error::UnexpectedFixture),
        }
        Ok(())
    }
}

pub(super) struct Wire {
    io: BufReader<UnixStream>,
    end: Instant,
    sequence: u32,
    pending_launch: Option<u32>,
    remaining: u8,
    events: Events,
}

impl Wire {
    pub fn new(socket: UnixStream, end: Instant) -> Self {
        Self {
            io: BufReader::new(socket),
            end,
            sequence: 0,
            pending_launch: None,
            remaining: MAX_MESSAGES,
            events: Events::default(),
        }
    }

    fn budget(&self) -> Result<Duration> {
        self.end
            .checked_duration_since(Instant::now())
            .filter(|t| !t.is_zero())
            .ok_or(Error::Deadline)
    }

    fn fill(&mut self, mut dst: &mut [u8]) -> Result<()> {
        while !dst.is_empty() {
            self.io
                .get_ref()
                .set_read_timeout(Some(self.budget()?))
                .map_err(io_error)?;
            let n = self.io.read(dst).map_err(io_error)?;
            if n == 0 {
                return Err(Error::Transport);
            }
            dst = &mut dst[n..];
        }
        Ok(())
    }

    fn receive(&mut self) -> Result<Value> {
        self.remaining = self.remaining.checked_sub(1).ok_or(Error::Protocol)?;
        let mut header = Vec::new();
        loop {
            if header.len() >= 64 {
                return Err(Error::Protocol);
            }
            let mut byte = [0];
            self.fill(&mut byte)?;
            header.push(byte[0]);
            if header.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let mut body = vec![0; length(&header)?];
        self.fill(&mut body)?;
        serde_json::from_slice(&body).map_err(|_| Error::Protocol)
    }

    pub fn wait_for(&mut self, predicate: impl Fn(&Events) -> bool) -> Result<&Events> {
        while !predicate(&self.events) {
            self.dispatch(None)?;
        }
        Ok(&self.events)
    }

    fn send(&mut self, command: &'static str, args: Value) -> Result<u32> {
        self.sequence += 1;
        let body = serde_json::to_vec(
            &json!({"seq":self.sequence, "type":"request", "command":command, "arguments":args}),
        )
        .map_err(|_| Error::Protocol)?;
        if body.len() > 4096 {
            return Err(Error::Protocol);
        }
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        for mut bytes in [header.as_bytes(), &body] {
            while !bytes.is_empty() {
                self.io
                    .get_ref()
                    .set_write_timeout(Some(self.budget()?))
                    .map_err(io_error)?;
                let n = self.io.get_ref().write(bytes).map_err(io_error)?;
                if n == 0 {
                    return Err(Error::Transport);
                }
                bytes = &bytes[n..];
            }
        }
        Ok(self.sequence)
    }

    // CodeLLDB acknowledges launch only after configurationDone. It is the
    // only request allowed to remain outstanding alongside a synchronous call.
    pub fn begin_launch(&mut self, args: Value) -> Result<()> {
        if self.pending_launch.is_some() || self.events.launched {
            return Err(Error::Protocol);
        }
        self.pending_launch = Some(self.send("launch", args)?);
        Ok(())
    }

    fn dispatch(&mut self, expected: Option<(u32, &str)>) -> Result<Option<Value>> {
        let mut msg = self.receive()?;
        match msg["type"].as_str() {
            Some("event") => self.events.accept(&msg)?,
            Some("response") if msg["success"] == true => {
                if self
                    .pending_launch
                    .is_some_and(|seq| msg["request_seq"] == seq && msg["command"] == "launch")
                {
                    self.pending_launch = None;
                    self.events.launched = true;
                } else if expected
                    .is_some_and(|(seq, cmd)| msg["request_seq"] == seq && msg["command"] == cmd)
                {
                    return Ok(Some(msg["body"].take()));
                } else {
                    return Err(Error::Protocol);
                }
            }
            _ => return Err(Error::Protocol),
        }
        Ok(None)
    }

    /// Private framing only. All commands and arguments originate in the fixed
    /// fixture sequence; reverse requests are rejected, never dispatched.
    pub fn call(&mut self, command: &'static str, args: Value) -> Result<Value> {
        let seq = self.send(command, args)?;
        loop {
            if let Some(body) = self.dispatch(Some((seq, command)))? {
                return Ok(body);
            }
        }
    }

    pub fn resume(&mut self, thread: u64) -> Result<()> {
        self.events.resume_sent = true;
        self.call("continue", json!({"threadId":thread}))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_is_unique_and_bounded_before_allocating() {
        assert_eq!(length(b"Content-Length: 65536\r\n\r\n"), Ok(65536));
        for bad in [
            "Content-Length: 65537\r\n\r\n",
            "Content-Length: 0\r\n\r\n",
            "Content-Length: -1\r\n\r\n",
            "Content-Length: 1\r\nContent-Length: 2\r\n\r\n",
            "Content-Length: 1\n\n",
        ] {
            assert_eq!(length(bad.as_bytes()), Err(Error::Protocol));
        }
    }

    #[test]
    fn rejects_reverse_requests_stale_responses_and_duplicate_stops() {
        for msg in [
            json!({"type":"request","command":"runInTerminal"}),
            json!({"type":"response","command":"initialize","request_seq":2,"success":true}),
            json!({"type":"response","command":"stackTrace","request_seq":1,"success":true}),
        ] {
            let (mut peer, socket) = UnixStream::pair().unwrap();
            let body = msg.to_string();
            write!(peer, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
            let mut wire = Wire::new(socket, Instant::now() + Duration::from_secs(1));
            assert_eq!(wire.call("initialize", json!({})), Err(Error::Protocol));
        }
        let stop = json!({"event":"stopped","body":{"reason":"breakpoint","threadId":1,"hitBreakpointIds":[2]}});
        let mut events = Events::default();
        events.accept(&stop).unwrap();
        assert_eq!(events.accept(&stop), Err(Error::UnexpectedFixture));
    }

    #[test]
    fn only_a_single_matching_deferred_launch_response_is_accepted() {
        fn response(seq: u32, command: &str, success: bool) -> Value {
            json!({"type":"response", "request_seq":seq, "command":command, "success":success})
        }
        let launch = response(1, "launch", true);
        let done = response(2, "configurationDone", true);
        for (msgs, ok) in [
            (vec![launch.clone(), done.clone()], true),
            (vec![done.clone(), launch.clone()], true),
            (vec![response(1, "launch", false)], false),
            (vec![response(2, "launch", true)], false),
            (vec![response(1, "configurationDone", true)], false),
            (vec![launch.clone(), launch], false),
        ] {
            let (mut peer, socket) = UnixStream::pair().unwrap();
            for msg in msgs {
                let body = msg.to_string();
                write!(peer, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
            }
            let mut wire = Wire::new(socket, Instant::now() + Duration::from_secs(1));
            wire.begin_launch(json!({})).unwrap();
            let result = wire
                .call("configurationDone", json!({}))
                .and_then(|_| wire.wait_for(|e| e.launched).map(|_| ()));
            assert_eq!(result, if ok { Ok(()) } else { Err(Error::Protocol) });
        }
    }

    #[test]
    fn termination_requires_resume_then_successful_exit() {
        let exit = json!({"event":"exited", "body":{"exitCode":0}});
        let term = json!({"event":"terminated"});
        let mut events = Events::default();
        assert_eq!(events.accept(&exit), Err(Error::UnexpectedFixture));
        events.resume_sent = true;
        assert_eq!(events.accept(&term), Err(Error::UnexpectedFixture));
        assert_eq!(
            events.accept(&json!({"event":"exited", "body":{"exitCode":1}})),
            Err(Error::UnexpectedFixture)
        );
        events.accept(&exit).unwrap();
        assert!(!events.terminated);
        events.accept(&term).unwrap();
        assert!(events.terminated);
        assert_eq!(events.accept(&term), Err(Error::UnexpectedFixture));
    }

    #[test]
    fn partial_frames_and_event_floods_are_bounded() {
        let (mut peer, socket) = UnixStream::pair().unwrap();
        peer.write_all(b"Content-Length: 2\r\n\r\n{").unwrap();
        let mut wire = Wire::new(socket, Instant::now() + Duration::from_millis(40));
        assert_eq!(wire.receive(), Err(Error::Deadline));

        let (mut peer, socket) = UnixStream::pair().unwrap();
        let body = r#"{"type":"event","event":"output"}"#;
        let msg = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        peer.write_all(msg.repeat(usize::from(MAX_MESSAGES) + 1).as_bytes())
            .unwrap();
        let mut wire = Wire::new(socket, Instant::now() + Duration::from_secs(1));
        assert!(matches!(
            wire.wait_for(|e| e.initialized),
            Err(Error::Protocol)
        ));
    }
}
