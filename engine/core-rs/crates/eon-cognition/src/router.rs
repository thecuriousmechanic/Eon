//! Speech-only tier selection. Planning does no I/O or inference. In particular,
//! a host can take a strategic plan to its background worker before executing it.

mod speech;
pub use speech::{SpeechRouter, SpeechTokens};

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::persona::{SpeechAct, format_speech};
use crate::profile::ModelProfile;
use eon_primitives::{EonError, EonResult};

pub const MAX_REQUEST_BYTES: usize = 16 * 1024;
pub const MAX_OUTPUT_TOKENS: usize = 2048;
/// Full-turn safety timeout, not a time-to-first-token target.
pub const DEFAULT_TURN_TIMEOUT: Duration = Duration::from_secs(30);

/// A final text turn, not speculative ASR or an authorization to act.
#[derive(Clone, Copy)]
pub struct SpeechRequest<'a> {
    pub act: SpeechAct,
    pub text: &'a str,
    pub max_tokens: usize,
}

/// Host intent, never inferred from instructions inside a transcript. Remote
/// consent applies to one strategic turn and only to the supplied public text.
#[derive(Default)]
pub enum Workload<'a> {
    #[default]
    Interactive,
    StrategicLocalOnly,
    StrategicAllowRemote {
        approved_text: &'a str,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Route {
    Reflex,
    TacticalLocal,
    StrategicLocal,
    StrategicRemote,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouteError {
    InvalidRequest,
    Unavailable(Route),
    BackendFailed(Route),
    InvalidSpeech(Route),
    Cancelled(Route),
    DeadlineExceeded(Route),
}

impl fmt::Display for RouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest => f.write_str("invalid speech routing request"),
            Self::Unavailable(route) => write!(f, "{route:?}: backend unavailable"),
            Self::BackendFailed(route) => write!(f, "{route:?}: generation did not complete"),
            Self::InvalidSpeech(route) => write!(f, "{route:?}: verbal turn rejected"),
            Self::Cancelled(route) => write!(f, "{route:?}: turn cancelled"),
            Self::DeadlineExceeded(route) => write!(f, "{route:?}: turn deadline exceeded"),
        }
    }
}

impl std::error::Error for RouteError {}

/// Host-owned, one-way stop signal and absolute monotonic deadline. Create it
/// before queuing the turn; another thread can cancel it without waiting for inference.
pub struct SpeechBudget {
    deadline: Instant,
    cancelled: AtomicBool,
}

impl SpeechBudget {
    pub fn until(deadline: Instant) -> Self {
        Self {
            deadline,
            cancelled: AtomicBool::new(false),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    fn check(&self, route: Route) -> Result<(), RouteError> {
        if self.cancelled.load(Ordering::Acquire) {
            Err(RouteError::Cancelled(route))
        } else if Instant::now() >= self.deadline {
            Err(RouteError::DeadlineExceeded(route))
        } else {
            Ok(())
        }
    }
}

/// An already-configured speech generator, not a tool dispatcher. Stream raw
/// token pieces to `out`; return Ok only after end-of-generation, never on a
/// timeout, truncation or partial response. Poll `out.check_active()` between
/// units of work and cap any blocking I/O at `out.deadline()`.
pub trait SpeechBackend: Sync {
    fn generate(&self, request: SpeechRequest<'_>, out: &mut SpeechTokens) -> EonResult<()>;
}

#[derive(Default)]
pub struct CognitiveRouter<'a> {
    /// Trusted host bindings: local slots must not delegate off device.
    pub tactical_local: Option<&'a dyn SpeechBackend>,
    pub strategic_local: Option<&'a dyn SpeechBackend>,
    pub strategic_remote: Option<&'a dyn SpeechBackend>,
}

impl<'a> CognitiveRouter<'a> {
    /// Slots are preloaded local models in `ModelProfile::ALL` order. Reserve
    /// P3 for strategic work; choose the heaviest available interactive model
    /// the device fits. Never load a model, probe a network or retry here.
    pub fn for_profile(device: ModelProfile, local: [Option<&'a dyn SpeechBackend>; 4]) -> Self {
        let [p0, p1, p2, p3] = local;
        Self {
            tactical_local: match device {
                ModelProfile::P0 => p0,
                ModelProfile::P1 => p1.or(p0),
                ModelProfile::P2 | ModelProfile::P3 => p2.or(p1).or(p0),
            },
            strategic_local: if device == ModelProfile::P3 { p3 } else { None },
            strategic_remote: None,
        }
    }

    pub fn plan<'r>(
        &self,
        request: SpeechRequest<'r>,
        workload: Workload<'r>,
    ) -> Result<SpeechPlan<'r>, RouteError>
    where
        'a: 'r,
    {
        let start = Instant::now();
        if !valid_text(request.text) || !(1..=MAX_OUTPUT_TOKENS).contains(&request.max_tokens) {
            return Err(RouteError::InvalidRequest);
        }
        let selected = match (workload, self.strategic_local) {
            (Workload::Interactive, _) => {
                if let Some(body) = self.reflex(request) {
                    let speech = format_speech(request.act, body)
                        .map_err(|_| RouteError::InvalidSpeech(Route::Reflex))?;
                    Selected::Reflex(speech)
                } else {
                    Selected::model(Route::TacticalLocal, self.tactical_local, request)?
                }
            }
            (_, Some(local)) => Selected::Model(Route::StrategicLocal, local, request),
            (Workload::StrategicLocalOnly, None) => {
                return Err(RouteError::Unavailable(Route::StrategicLocal));
            }
            (Workload::StrategicAllowRemote { approved_text }, None) => {
                if !valid_text(approved_text) || approved_text.contains(['<', '>']) {
                    return Err(RouteError::InvalidRequest);
                }
                // The plan handed to the remote worker retains no private transcript.
                Selected::model(
                    Route::StrategicRemote,
                    self.strategic_remote,
                    SpeechRequest {
                        text: approved_text,
                        act: request.act,
                        max_tokens: request.max_tokens,
                    },
                )?
            }
        };
        Ok(SpeechPlan {
            selected,
            started: start,
            route_elapsed: start.elapsed(),
        })
    }

    fn reflex(&self, request: SpeechRequest<'_>) -> Option<&'static str> {
        let text = request.text.trim().trim_end_matches(['.', '!', '?']);
        match request.act {
            SpeechAct::Affirmative
                if ["ping", "acknowledge", "affirmative", "are you there"]
                    .iter()
                    .any(|command| text.eq_ignore_ascii_case(command)) =>
            {
                Some("")
            }
            SpeechAct::Report if text.eq_ignore_ascii_case("router status") => {
                Some(if self.tactical_local.is_some() {
                    "Local tactical backend configured."
                } else {
                    "No local tactical backend configured."
                })
            }
            _ => None,
        }
    }
}

fn valid_text(text: &str) -> bool {
    text.len() <= MAX_REQUEST_BYTES && !text.trim().is_empty() && !text.contains('\0')
}

enum Selected<'a> {
    Reflex(String),
    Model(Route, &'a dyn SpeechBackend, SpeechRequest<'a>),
}

impl<'a> Selected<'a> {
    fn model(
        route: Route,
        backend: Option<&'a dyn SpeechBackend>,
        request: SpeechRequest<'a>,
    ) -> Result<Self, RouteError> {
        Ok(Self::Model(
            route,
            backend.ok_or(RouteError::Unavailable(route))?,
            request,
        ))
    }
}

pub struct SpeechPlan<'a> {
    selected: Selected<'a>,
    started: Instant,
    route_elapsed: Duration,
}

#[derive(Debug)]
pub struct RoutedSpeech {
    pub route: Route,
    pub speech: String,
    /// Selection, including input checks and rendering a local reflex.
    pub route_elapsed: Duration,
    /// Generator call (including sink writes), not routing time or model TTFT.
    pub backend_elapsed: Option<Duration>,
}

impl SpeechPlan<'_> {
    pub fn route(&self) -> Route {
        match self.selected {
            Selected::Reflex(_) => Route::Reflex,
            Selected::Model(route, ..) => route,
        }
    }

    /// Consumes the plan. At most one backend is called; generation or speech
    /// validation failure is terminal, never a reason to try a remote backend.
    pub fn execute(self) -> Result<RoutedSpeech, RouteError> {
        let budget = SpeechBudget::until(self.started + DEFAULT_TURN_TIMEOUT);
        self.execute_with_budget(&budget)
    }

    /// The caller can set the deadline relative to end-of-utterance and cancel
    /// on interruption. No worker is detached. A blocking backend may return
    /// late, but cannot publish its output past the final budget check.
    pub fn execute_with_budget(self, budget: &SpeechBudget) -> Result<RoutedSpeech, RouteError> {
        let route = self.route();
        budget.check(route)?;
        let (speech, backend_elapsed) = match self.selected {
            Selected::Reflex(speech) => (speech, None),
            Selected::Model(_, backend, request) => {
                let mut out = SpeechTokens::new(request.act, request.max_tokens, route, budget);
                let start = Instant::now();
                let completion = backend.generate(request, &mut out);
                let elapsed = start.elapsed();
                budget.check(route)?;
                // Adapter diagnostics can contain the prompt or raw completion.
                completion.map_err(|error| match error {
                    EonError::ValidationError(_) => RouteError::InvalidSpeech(route),
                    _ => RouteError::BackendFailed(route),
                })?;
                (
                    out.finish().map_err(|_| RouteError::InvalidSpeech(route))?,
                    Some(elapsed),
                )
            }
        };
        budget.check(route)?;
        Ok(RoutedSpeech {
            route,
            speech,
            route_elapsed: self.route_elapsed,
            backend_elapsed,
        })
    }
}
