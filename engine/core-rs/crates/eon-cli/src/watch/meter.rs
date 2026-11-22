//! Local input meter. The driver callback exports one peak, never a buffer of PCM.

use std::fmt;
use std::time::{Duration, Instant};

const HISTORY: usize = 48;
const STALL: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Peak {
    level: u8,
    last_callback: Instant,
}

trait Input {
    /// `None` means no callback, distinct from a callback reporting silence.
    fn take_peak(&self) -> Result<Option<Peak>, &'static str>;
}

#[derive(Default)]
pub(super) struct Meter {
    input: Option<Box<dyn Input>>,
    last_callback: Option<Instant>,
    message: Option<&'static str>,
    history: Vec<u64>,
}

impl fmt::Debug for Meter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Meter")
            .field("active", &self.active())
            .finish()
    }
}

impl Meter {
    pub(super) fn active(&self) -> bool {
        self.input.is_some()
    }

    pub(super) fn stop(&mut self) {
        *self = Self::default();
    }

    fn toggle_with(
        &mut self,
        now: impl FnOnce() -> Instant,
        open: impl FnOnce() -> Result<Box<dyn Input>, &'static str>,
    ) {
        let was_active = self.active();
        self.stop();
        if !was_active {
            match open() {
                Ok(input) => {
                    self.input = Some(input);
                    // Opening and starting a device can block. Give it a full grace period.
                    self.last_callback = Some(now());
                }
                Err(message) => self.message = Some(message),
            }
        }
    }

    pub(super) fn toggle(&mut self) {
        #[cfg(feature = "audio-input")]
        self.toggle_with(Instant::now, device::open);
        #[cfg(not(feature = "audio-input"))]
        self.toggle_with(Instant::now, || Err("Audio input support is not built in."));
    }

    pub(super) fn poll(&mut self, now: Instant) {
        let Some(input) = &self.input else { return };
        let fresh = |at| now.saturating_duration_since(at) < STALL;
        let error = match input.take_peak() {
            Ok(Some(peak)) if fresh(peak.last_callback) => {
                self.last_callback = Some(peak.last_callback);
                if self.history.len() == HISTORY {
                    self.history.remove(0);
                }
                self.history.push(u64::from(peak.level));
                return;
            }
            Ok(None) if self.last_callback.is_some_and(fresh) => return,
            Ok(_) => "Input stopped delivering samples.",
            Err(message) => message,
        };
        self.stop();
        self.message = Some(error);
    }

    pub(super) fn status(&self) -> String {
        if self.active() {
            let level = self.history.last().map_or_else(
                || "waiting for samples".into(),
                |peak| format!("peak {peak}%"),
            );
            format!("Input ON: {level}. Press F2 to stop or leave this view.")
        } else {
            let why = self.message.unwrap_or("Input OFF.");
            if cfg!(feature = "audio-input") {
                format!("{why} Press F2 to meter the default microphone locally.")
            } else {
                format!("{why} Rebuild with --features audio-input to meter a microphone.")
            }
        }
    }

    pub(super) fn visible_peaks(&self, width: u16) -> &[u64] {
        &self.history[self.history.len().saturating_sub(usize::from(width))..]
    }
}

#[cfg(feature = "audio-input")]
mod device {
    use super::{Input, Peak};
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{ErrorKind, FromSample, SampleFormat, SizedSample, Stream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    struct Callback {
        origin: Instant,
        last_micros: AtomicU64,
        // 0 = no samples since the last poll; 1..=101 = observed 0..=100%.
        level: AtomicU32,
        failed: AtomicBool,
    }

    impl Callback {
        fn new() -> Self {
            Self {
                origin: Instant::now(),
                last_micros: AtomicU64::new(0),
                level: AtomicU32::new(0),
                failed: AtomicBool::new(false),
            }
        }

        fn observe<T: SizedSample>(&self, samples: &[T], at: Instant)
        where
            f32: FromSample<T>,
        {
            if samples.is_empty() {
                return;
            }
            let peak = samples
                .iter()
                .fold(0.0_f32, |p, s| p.max(s.to_sample::<f32>().abs()));
            // Publish the time before the level; even quieter callbacks count as live.
            self.last_micros.store(
                at.duration_since(self.origin).as_micros() as u64,
                Ordering::Relaxed,
            );
            self.level.fetch_max(
                1 + (peak.min(1.0) * 100.0).round() as u32,
                Ordering::Release,
            );
        }

        fn on_error(&self, kind: ErrorKind) {
            // CPAL recovers xruns. If callbacks do not resume, the normal stall timer wins.
            if kind != ErrorKind::Xrun {
                self.failed.store(true, Ordering::Release);
            }
        }

        fn take_peak(&self) -> Result<Option<Peak>, &'static str> {
            if self.failed.load(Ordering::Acquire) {
                return Err("The input device reported an error.");
            }
            Ok(self
                .level
                .swap(0, Ordering::Acquire)
                .checked_sub(1)
                .map(|level| Peak {
                    level: level as u8,
                    last_callback: self.origin
                        + Duration::from_micros(self.last_micros.load(Ordering::Relaxed)),
                }))
        }
    }

    struct DeviceInput {
        _stream: Stream,
        callback: Arc<Callback>,
    }

    impl Input for DeviceInput {
        fn take_peak(&self) -> Result<Option<Peak>, &'static str> {
            self.callback.take_peak()
        }
    }

    fn build<T>(
        device: &cpal::Device,
        config: cpal::StreamConfig,
        callback: &Arc<Callback>,
    ) -> Result<Stream, &'static str>
    where
        T: SizedSample,
        f32: FromSample<T>,
    {
        let (data, error) = (callback.clone(), callback.clone());
        device
            .build_input_stream::<T, _, _>(
                config,
                move |samples: &[T], _| data.observe(samples, Instant::now()),
                move |err| error.on_error(err.kind()),
                None,
            )
            .map_err(|_| "Cannot open the default microphone.")
    }

    pub(super) fn open() -> Result<Box<dyn Input>, &'static str> {
        let device = cpal::default_host()
            .default_input_device()
            .ok_or("No default microphone.")?;
        let config = device
            .default_input_config()
            .map_err(|_| "No available input configuration.")?;
        let callback = Arc::new(Callback::new());
        let stream = match config.sample_format() {
            SampleFormat::F32 => build::<f32>(&device, config.config(), &callback),
            SampleFormat::I16 => build::<i16>(&device, config.config(), &callback),
            SampleFormat::I24 => build::<cpal::I24>(&device, config.config(), &callback),
            SampleFormat::I32 => build::<i32>(&device, config.config(), &callback),
            SampleFormat::U16 => build::<u16>(&device, config.config(), &callback),
            _ => Err("Unsupported microphone sample format."),
        }?;
        stream
            .play()
            .map_err(|_| "Cannot start the default microphone.")?;
        Ok(Box::new(DeviceInput {
            _stream: stream,
            callback,
        }))
    }

    #[test]
    fn i24_peak_and_latest_callback_survive_an_xrun_but_not_device_failure() {
        let data = Callback::new();
        let at = data.origin + Duration::from_secs(2);
        data.observe(&[cpal::I24::new(-(1 << 22)).unwrap()], data.origin);
        data.on_error(ErrorKind::Xrun);
        data.observe(&[0.0_f32], at);
        data.observe(&[] as &[f32], at + Duration::from_secs(1));
        assert_eq!(
            data.take_peak(),
            Ok(Some(Peak {
                level: 50,
                last_callback: at
            }))
        );
        assert_eq!(data.take_peak(), Ok(None));
        data.on_error(ErrorKind::DeviceNotAvailable);
        data.on_error(ErrorKind::Xrun);
        assert!(data.take_peak().is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Page, WatchState};
    use super::*;
    use crate::render_watch_snapshot;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use std::cell::Cell;
    use std::rc::Rc;

    type Feed = Rc<Cell<Option<Peak>>>;
    struct Counter(Feed);

    impl Input for Counter {
        fn take_peak(&self) -> Result<Option<Peak>, &'static str> {
            Ok(self.0.take())
        }
    }

    fn attach(meter: &mut Meter, at: Instant) -> Feed {
        let feed = Feed::default();
        meter.toggle_with(|| at, || Ok(Box::new(Counter(feed.clone()))));
        feed
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn silence_is_live_but_stalled_input_closes_and_erases_its_history() {
        let mut now = Instant::now();
        let mut meter = Meter::default();
        let feed = attach(&mut meter, now);
        for _ in 0..(2 * HISTORY) {
            now += Duration::from_secs(1);
            feed.set(Some(Peak {
                level: 0,
                last_callback: now,
            }));
            meter.poll(now);
        }
        assert_eq!(meter.history, &[0; HISTORY]);
        assert!(meter.status().contains("peak 0%"));
        meter.poll(now + STALL);
        assert!(!meter.active() && meter.history.is_empty());
        assert!(meter.status().contains("stopped delivering samples"));
        assert_eq!(Rc::strong_count(&feed), 1);
    }

    #[test]
    fn buffered_peak_at_five_seconds_does_not_revive_stalled_input() {
        let now = Instant::now();
        let mut meter = Meter::default();
        let feed = attach(&mut meter, now);
        feed.set(Some(Peak {
            level: 50,
            last_callback: now,
        }));
        meter.poll(now + Duration::from_secs(5));
        assert!(!meter.active() && meter.history.is_empty());
        assert!(meter.status().contains("stopped delivering samples"));
        assert_eq!(Rc::strong_count(&feed), 1);
    }

    #[test]
    fn first_sample_grace_begins_after_a_slow_open() {
        let now = Instant::now();
        let played = now + Duration::from_secs(5);
        let clock = Cell::new(now);
        let mut meter = Meter::default();
        meter.toggle_with(
            || clock.get(),
            || {
                clock.set(played);
                Ok(Box::new(Counter(Feed::default())))
            },
        );
        meter.poll(played + STALL - Duration::from_millis(1));
        assert!(meter.active());
        meter.poll(played + STALL);
        assert!(!meter.active());
    }

    #[test]
    fn narrow_graph_renders_the_newest_suffix() {
        let mut state = WatchState::default();
        state.open(Page::Audio);
        state.meter.history = vec![0; HISTORY];
        state.meter.history[HISTORY - 1] = 100;
        assert!(state.meter.visible_peaks(0).is_empty());
        let screen = render_watch_snapshot(&state, 8, 9).unwrap();
        assert!(screen.lines().any(|line| line == "│     █│"), "{screen}");
    }

    #[test]
    fn audio_toggle_requires_an_unmodified_nontext_press_never_paste() {
        let mut state = WatchState::default();
        state.open(Page::Audio);
        let feed = attach(&mut state.meter, Instant::now());
        for event in [
            Event::Paste("m/\n\x1bOQq".into()),
            Event::Key(press(KeyCode::Char('m'))),
            Event::Key(KeyEvent::new(KeyCode::F(2), KeyModifiers::SHIFT)),
            Event::Key(KeyEvent::new_with_kind(
                KeyCode::F(2),
                KeyModifiers::NONE,
                KeyEventKind::Repeat,
            )),
            Event::Key(KeyEvent::new_with_kind(
                KeyCode::F(2),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            )),
        ] {
            state.handle_event(event);
            assert!(state.meter.active() && state.prompt.is_none() && !state.quit);
        }
        state.handle_event(Event::Key(press(KeyCode::F(2))));
        assert!(!state.meter.active());
        assert_eq!(Rc::strong_count(&feed), 1);
    }

    #[test]
    fn navigation_completion_prompt_and_quit_never_leave_the_input_running() {
        let mut state = WatchState::default();
        for key in "/a".chars() {
            state.on_key(key);
        }
        state.handle_event(Event::Paste("udio\nm".into()));
        assert_eq!(state.prompt.as_ref().unwrap().text, "/a");
        state.handle_key(press(KeyCode::Tab));
        state.handle_key(press(KeyCode::Enter));
        assert_eq!(state.page, Page::Audio);
        assert!(
            render_watch_snapshot(&state, 90, 12)
                .unwrap()
                .contains("Input OFF.")
        );

        for exit in [
            press(KeyCode::Char('/')),
            press(KeyCode::Esc),
            press(KeyCode::Char('q')),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            state.open(Page::Audio);
            let now = Instant::now();
            let feed = attach(&mut state.meter, now);
            feed.set(Some(Peak {
                level: 0,
                last_callback: now,
            }));
            state.meter.poll(now);
            assert!(
                render_watch_snapshot(&state, 90, 12)
                    .unwrap()
                    .contains("Input ON: peak 0%")
            );
            state.handle_key(exit);
            assert_eq!(Rc::strong_count(&feed), 1);
            assert!(state.meter.history.is_empty());
            if exit.code == KeyCode::Char('/') {
                assert!(state.prompt.is_some());
                state.handle_key(press(KeyCode::Esc));
                assert_eq!(state.page, Page::Audio);
                assert!(!state.meter.active());
            }
        }
    }

    #[cfg(all(feature = "audio-input", target_os = "linux"))]
    #[test]
    #[ignore = "requires a dedicated PulseAudio eon_meter null sink playing a half-scale tone"]
    fn opt_in_virtual_input_reports_half_scale_then_releases_device() {
        use std::env;
        use std::fs;
        use std::os::unix::fs::{FileTypeExt, PermissionsExt};
        use std::path::Path;
        use std::process::Command;

        assert_eq!(env::var("EON_TEST_VIRTUAL_MIC").as_deref(), Ok("1"));
        let server = env::var("PULSE_SERVER").expect("explicit private Pulse server required");
        let socket = Path::new(server.strip_prefix("unix:").expect("local Unix socket"));
        assert!(socket.is_absolute() && fs::metadata(socket).unwrap().file_type().is_socket());
        assert_eq!(
            fs::metadata(socket.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o077,
            0,
            "use a dedicated private directory for the Pulse socket"
        );
        // Verify the *entire* default PCM route before asking CPAL for any device.
        // In particular, a missing server, system config include or fallback PCM is not OK.
        assert!(!server.contains(['"', '\\']) && !server.chars().any(char::is_whitespace));
        let config =
            fs::read_to_string(env::var_os("ALSA_CONFIG_PATH").expect("private ALSA PCM")).unwrap();
        let expected = format!(
            "pcm.!default {{ type pulse server \"{server}\" device \"eon_meter.monitor\" }}"
        );
        assert!(config.split_whitespace().eq(expected.split_whitespace()));
        assert_eq!(cpal::default_host().id(), cpal::HostId::Alsa);
        let list = |kind| {
            let result = Command::new("pactl")
                .args(["--server", &server, "list", "short", kind])
                .output()
                .unwrap();
            assert!(result.status.success(), "private pactl list {kind} failed");
            String::from_utf8(result.stdout).unwrap()
        };
        let sources = list("sources");
        assert_eq!(sources.lines().count(), 1);
        let fields: Vec<_> = sources.split_whitespace().collect();
        assert_eq!(&fields[1..3], ["eon_meter.monitor", "module-null-sink.c"]);
        let monitor = fields[0];
        assert!(list("source-outputs").is_empty());

        let mut state = WatchState::default();
        state.handle_key(press(KeyCode::F(2)));
        assert!(!state.meter.active());
        state.open(Page::Audio);
        state.handle_event(Event::Paste("m\x1bOQ".into()));
        state.on_key('m');
        assert!(!state.meter.active() && list("source-outputs").is_empty());
        state.handle_key(press(KeyCode::F(2)));
        assert!(state.meter.active(), "{}", state.meter.status());
        let start = Instant::now();
        let observed = loop {
            state.meter.poll(Instant::now());
            if let Some(&level @ 48..=52) = state.meter.history.last() {
                break level;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "{}",
                state.meter.status()
            );
            std::thread::sleep(Duration::from_millis(25));
        };
        let active = list("source-outputs");
        assert_eq!(active.lines().count(), 1);
        let mut fields = active.split_whitespace();
        let id = fields.next().unwrap();
        assert_eq!(fields.next(), Some(monitor));
        state.on_key('/');
        state.handle_key(press(KeyCode::F(2)));
        assert!(!state.meter.active() && state.meter.history.is_empty());
        let stop = Instant::now();
        while !list("source-outputs").is_empty() {
            assert!(
                stop.elapsed() < Duration::from_secs(3),
                "capture not released"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        eprintln!(
            "Pulse source-output {id} on eon_meter.monitor (source {monitor}): peak {observed}%; \
                 zero source-outputs after /, test PID {} still alive",
            std::process::id()
        );
    }
}
