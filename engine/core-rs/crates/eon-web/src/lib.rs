//! Local web UI over the Eon engine.
//!
//! The server binds loopback only and has no authentication, so every route is either a read-only view
//! of the journal or a bounded computation over caller-supplied input. Routes that mutate state, approve
//! capabilities, or expose credentials and private memory are deliberately absent.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use eon_canvas::layered;
use eon_cognition::profile::ModelProfile;
use eon_cyber::cpg::FunctionBlock;
use eon_cyber::cpg::{Lang, function_blocks};
use eon_cyber::defense::taint_auditor::{SourceReport, review_source};
use eon_cyber::defense::{Assessment, MAX_SOURCE_BYTES};
use eon_cyber::function_review::{Status, match_blocks};
use eon_foundation::journal::replay_from;
use eon_primitives::EonError;
use eon_voice::asr::ZipformerAsr;
use eon_voice::tts::{KokoroTts, SAMPLE_RATE_HZ};
use serde::{Deserialize, Serialize};

const INDEX_HTML: &str = include_str!("index.html");
const DEFAULT_PAGE: usize = 50;
const MAX_PAGE: usize = 500;
const PREVIEW_BYTES: usize = 512;
const MAX_TTS_CHARS: usize = 4000;
const MAX_LAYOUT_NODES: usize = 256;
const MAX_LAYOUT_EDGES: usize = 1024;
const ASR_SAMPLE_RATE_HZ: usize = 16_000;
const ASR_MAX_SECONDS: usize = 60;
/// Largest request body any route accepts: the ASR upload of 60 s of 16 kHz f32 PCM.
pub const MAX_BODY_BYTES: usize = ASR_SAMPLE_RATE_HZ * ASR_MAX_SECONDS * 4;

#[derive(Clone)]
pub struct AppState {
    journal_dir: Arc<PathBuf>,
    tts: Option<Arc<Mutex<KokoroTts>>>,
    asr: Option<Arc<Mutex<ZipformerAsr>>>,
}

impl AppState {
    pub fn new(journal_dir: impl Into<PathBuf>) -> Self {
        Self {
            journal_dir: Arc::new(journal_dir.into()),
            tts: None,
            asr: None,
        }
    }

    pub fn with_tts(mut self, tts: KokoroTts) -> Self {
        self.tts = Some(Arc::new(Mutex::new(tts)));
        self
    }

    pub fn with_asr(mut self, asr: ZipformerAsr) -> Self {
        self.asr = Some(Arc::new(Mutex::new(asr)));
        self
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/status", get(status))
        .route("/api/journal", get(journal))
        .route("/api/profiles", get(profiles))
        .route("/api/profiles/select", get(select_profile))
        .route("/api/cyber/review", post(cyber_review))
        .route("/api/review/diff", post(review_diff))
        .route("/api/canvas/layout", post(canvas_layout))
        .route("/api/voice/tts", post(voice_tts))
        .route("/api/voice/asr", post(voice_asr))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn unavailable(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

impl From<EonError> for ApiError {
    fn from(error: EonError) -> Self {
        Self::bad_request(error.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

/// Runs blocking file and inference work off the async executor.
async fn blocking<T, F>(work: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ApiError> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| ApiError::internal(format!("worker failed: {e}")))?
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

#[derive(Serialize)]
struct JournalView {
    segments: usize,
    frames: usize,
    last_seq: Option<u64>,
}

#[derive(Serialize)]
struct VoiceView {
    tts: bool,
    asr: bool,
}

#[derive(Serialize)]
struct StatusView {
    journal: JournalView,
    voice: VoiceView,
}

async fn status(State(state): State<AppState>) -> ApiResult<StatusView> {
    let dir = state.journal_dir.clone();
    let journal = blocking(move || {
        let status = eon_cli::read_status(&dir).map_err(|e| ApiError::internal(e.to_string()))?;
        Ok(JournalView {
            segments: status.segments,
            frames: status.frames,
            last_seq: status.last_seq,
        })
    })
    .await?;
    Ok(Json(StatusView {
        journal,
        voice: VoiceView {
            tts: state.tts.is_some(),
            asr: state.asr.is_some(),
        },
    }))
}

#[derive(Deserialize)]
struct JournalQuery {
    #[serde(default)]
    from: u64,
    #[serde(default = "default_page")]
    limit: usize,
}

fn default_page() -> usize {
    DEFAULT_PAGE
}

#[derive(Serialize)]
struct FrameView {
    seq: u64,
    mono_ts_ns: u64,
    flags: u16,
    bytes: usize,
    preview: String,
}

#[derive(Serialize)]
struct JournalPage {
    frames: Vec<FrameView>,
    next_from: Option<u64>,
}

async fn journal(
    State(state): State<AppState>,
    Query(query): Query<JournalQuery>,
) -> ApiResult<JournalPage> {
    if query.limit == 0 || query.limit > MAX_PAGE {
        return Err(ApiError::bad_request(format!(
            "limit must be between 1 and {MAX_PAGE}"
        )));
    }
    let dir = state.journal_dir.clone();
    let page = blocking(move || {
        let mut frames =
            replay_from(&*dir, query.from).map_err(|e| ApiError::internal(e.to_string()))?;
        let next_from = frames.get(query.limit).map(|f| f.seq_no);
        frames.truncate(query.limit);
        let frames = frames
            .into_iter()
            .map(|f| {
                let shown = &f.payload[..f.payload.len().min(PREVIEW_BYTES)];
                FrameView {
                    seq: f.seq_no,
                    mono_ts_ns: f.mono_ts_ns,
                    flags: f.flags,
                    bytes: f.payload.len(),
                    preview: String::from_utf8_lossy(shown).into_owned(),
                }
            })
            .collect();
        Ok(JournalPage { frames, next_from })
    })
    .await?;
    Ok(Json(page))
}

#[derive(Serialize)]
struct ProfileView {
    profile: String,
    lock_key: &'static str,
    min_device_memory_gb: f64,
}

impl From<ModelProfile> for ProfileView {
    fn from(profile: ModelProfile) -> Self {
        Self {
            profile: profile.to_string(),
            lock_key: profile.lock_key(),
            min_device_memory_gb: profile.min_device_memory_gb(),
        }
    }
}

async fn profiles() -> Json<Vec<ProfileView>> {
    Json(
        ModelProfile::ALL
            .iter()
            .copied()
            .map(ProfileView::from)
            .collect(),
    )
}

#[derive(Deserialize)]
struct SelectQuery {
    memory_gb: f64,
}

async fn select_profile(Query(query): Query<SelectQuery>) -> ApiResult<ProfileView> {
    let profile = ModelProfile::select_for_memory_gb(query.memory_gb)?;
    Ok(Json(profile.into()))
}

fn parse_lang(language: &str) -> Result<Lang, ApiError> {
    match language {
        "rust" => Ok(Lang::Rust),
        "python" => Ok(Lang::Python),
        other => Err(ApiError::bad_request(format!(
            "unsupported language {other:?}; expected \"rust\" or \"python\""
        ))),
    }
}

fn check_source_len(source: &str) -> Result<(), ApiError> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(ApiError::bad_request(format!(
            "source is {} bytes; the limit is {MAX_SOURCE_BYTES}",
            source.len()
        )));
    }
    Ok(())
}

#[derive(Deserialize)]
struct SourceInput {
    language: String,
    source: String,
}

#[derive(Serialize)]
struct CheckView {
    rule: String,
    cwe: &'static str,
    recommendation: &'static str,
    observed: bool,
    range: Option<[usize; 2]>,
    gap: Option<String>,
}

#[derive(Serialize)]
struct ReviewView {
    language: String,
    checks: Vec<CheckView>,
}

fn range_pair(range: &Range<usize>) -> [usize; 2] {
    [range.start, range.end]
}

fn review_view(language: &str, report: SourceReport) -> ReviewView {
    let checks = report
        .checks
        .iter()
        .map(|check| {
            let (observed, range, gap) = match &check.evidence {
                Assessment::Observed(range) => (true, Some(range_pair(range)), None),
                Assessment::Unverified(gap) => (false, None, Some(format!("{gap:?}"))),
            };
            CheckView {
                rule: format!("{:?}", check.rule),
                cwe: check.rule.cwe(),
                recommendation: check.rule.recommendation(),
                observed,
                range,
                gap,
            }
        })
        .collect();
    ReviewView {
        language: language.to_owned(),
        checks,
    }
}

async fn cyber_review(Json(input): Json<SourceInput>) -> ApiResult<ReviewView> {
    let lang = parse_lang(&input.language)?;
    check_source_len(&input.source)?;
    let language = input.language.clone();
    let view = blocking(move || {
        let report = review_source(lang, &input.source);
        Ok(review_view(&language, report))
    })
    .await?;
    Ok(Json(view))
}

#[derive(Deserialize)]
struct DiffInput {
    language: String,
    old: String,
    new: String,
}

#[derive(Serialize)]
struct BlockView {
    name: String,
    start_line: usize,
    text: String,
}

impl From<&FunctionBlock> for BlockView {
    fn from(block: &FunctionBlock) -> Self {
        Self {
            name: block.name.clone(),
            start_line: block.start_line,
            text: block.text.clone(),
        }
    }
}

#[derive(Serialize)]
struct PairingView {
    status: &'static str,
    old: Option<usize>,
    new: Option<usize>,
}

#[derive(Serialize)]
struct DiffView {
    old: Vec<BlockView>,
    new: Vec<BlockView>,
    pairings: Vec<PairingView>,
}

fn status_name(status: Status) -> &'static str {
    match status {
        Status::Unchanged => "unchanged",
        Status::Changed => "changed",
        Status::Added => "added",
        Status::Removed => "removed",
    }
}

async fn review_diff(Json(input): Json<DiffInput>) -> ApiResult<DiffView> {
    let lang = parse_lang(&input.language)?;
    check_source_len(&input.old)?;
    check_source_len(&input.new)?;
    let view = blocking(move || {
        let old = function_blocks(lang, &input.old)?;
        let new = function_blocks(lang, &input.new)?;
        let pairings = match_blocks(&old, &new)
            .into_iter()
            .map(|p| PairingView {
                status: status_name(p.status),
                old: p.old,
                new: p.new,
            })
            .collect();
        Ok(DiffView {
            old: old.iter().map(BlockView::from).collect(),
            new: new.iter().map(BlockView::from).collect(),
            pairings,
        })
    })
    .await?;
    Ok(Json(view))
}

#[derive(Deserialize)]
struct LayoutInput {
    nodes: usize,
    edges: Vec<[usize; 2]>,
}

#[derive(Serialize)]
struct LayoutView {
    rank: Vec<usize>,
    slot: Vec<usize>,
    edge_paths: Vec<Vec<[usize; 2]>>,
    crossings: usize,
}

async fn canvas_layout(Json(input): Json<LayoutInput>) -> ApiResult<LayoutView> {
    if input.nodes > MAX_LAYOUT_NODES || input.edges.len() > MAX_LAYOUT_EDGES {
        return Err(ApiError::bad_request(format!(
            "layout is limited to {MAX_LAYOUT_NODES} nodes and {MAX_LAYOUT_EDGES} edges"
        )));
    }
    let edges: Vec<(usize, usize)> = input.edges.iter().map(|&[u, v]| (u, v)).collect();
    let layout =
        layered(input.nodes, &edges).map_err(|e| ApiError::bad_request(format!("{e:?}")))?;
    Ok(Json(LayoutView {
        rank: layout.rank,
        slot: layout.slot,
        edge_paths: layout
            .edge_paths
            .into_iter()
            .map(|path| path.into_iter().map(|(r, s)| [r, s]).collect())
            .collect(),
        crossings: layout.crossings,
    }))
}

#[derive(Deserialize)]
struct TtsInput {
    text: String,
    #[serde(default = "default_speed")]
    speed: f32,
}

fn default_speed() -> f32 {
    1.0
}

async fn voice_tts(
    State(state): State<AppState>,
    Json(input): Json<TtsInput>,
) -> Result<Response, ApiError> {
    let tts = state.tts.clone().ok_or_else(|| {
        ApiError::unavailable(
            "TTS is not configured; start eon-web with --tts-model and --tts-voice",
        )
    })?;
    if input.text.trim().is_empty() || input.text.chars().count() > MAX_TTS_CHARS {
        return Err(ApiError::bad_request(format!(
            "text must be 1 to {MAX_TTS_CHARS} characters"
        )));
    }
    if !(0.5..=2.0).contains(&input.speed) {
        return Err(ApiError::bad_request("speed must be between 0.5 and 2.0"));
    }
    let samples = blocking(move || {
        let mut tts = tts
            .lock()
            .map_err(|_| ApiError::internal("TTS engine is poisoned"))?;
        tts.synthesize(&input.text, input.speed)
            .map_err(|e| ApiError::internal(e.to_string()))
    })
    .await?;
    Ok((
        [(header::CONTENT_TYPE, "audio/wav")],
        wav_bytes(&samples, SAMPLE_RATE_HZ),
    )
        .into_response())
}

async fn voice_asr(State(state): State<AppState>, body: Bytes) -> ApiResult<serde_json::Value> {
    let asr = state.asr.clone().ok_or_else(|| {
        ApiError::unavailable("ASR is not configured; start eon-web with --asr-dir")
    })?;
    if !body.len().is_multiple_of(4) || body.len() > MAX_BODY_BYTES || body.is_empty() {
        return Err(ApiError::bad_request(
            "body must be 16 kHz little-endian f32 PCM, up to 60 seconds",
        ));
    }
    let (chunks, _) = body.as_chunks::<4>();
    let samples: Vec<f32> = chunks.iter().map(|c| f32::from_le_bytes(*c)).collect();
    if samples.iter().any(|s| !s.is_finite()) {
        return Err(ApiError::bad_request("PCM contains non-finite samples"));
    }
    let text = blocking(move || {
        let mut asr = asr
            .lock()
            .map_err(|_| ApiError::internal("ASR engine is poisoned"))?;
        asr.transcribe(&samples)
            .map_err(|e| ApiError::internal(e.to_string()))
    })
    .await?;
    Ok(Json(serde_json::json!({ "text": text })))
}

/// Encodes mono f32 samples as a 16-bit PCM WAV file.
pub fn wav_bytes(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for &s in samples {
        let clamped = s.clamp(-1.0, 1.0);
        out.extend_from_slice(&((clamped * i16::MAX as f32) as i16).to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use eon_foundation::journal::JournalWriter;
    use tower::ServiceExt;

    use super::*;

    async fn call(app: Router, request: Request<Body>) -> (StatusCode, Bytes) {
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        (
            status,
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        )
    }

    fn get_request(uri: &str) -> Request<Body> {
        Request::builder().uri(uri).body(Body::empty()).unwrap()
    }

    fn post_json(uri: &str, body: &serde_json::Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    #[tokio::test]
    async fn journal_pages_frames_from_a_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = JournalWriter::open(dir.path()).unwrap();
        for i in 0..5 {
            writer
                .append_batch(&[format!("event {i}").as_bytes()])
                .unwrap();
        }
        drop(writer);
        let app = router(AppState::new(dir.path()));
        let (status, body) = call(app, get_request("/api/journal?from=2&limit=2")).await;
        assert_eq!(status, StatusCode::OK);
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let seqs: Vec<u64> = page["frames"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["seq"].as_u64().unwrap())
            .collect();
        assert_eq!(seqs, vec![2, 3]);
        assert_eq!(page["frames"][0]["preview"], "event 1");
        assert_eq!(page["next_from"], 4);
    }

    #[tokio::test]
    async fn voice_routes_report_unavailable_without_models() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(AppState::new(dir.path()));
        let (status, _) = call(
            app.clone(),
            post_json("/api/voice/tts", &serde_json::json!({ "text": "hi" })),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let (status, body) = call(app, get_request("/api/status")).await;
        assert_eq!(status, StatusCode::OK);
        let view: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(view["voice"]["tts"], false);
        assert_eq!(view["voice"]["asr"], false);
    }

    #[tokio::test]
    async fn canvas_layout_rejects_cycles_and_oversized_input() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(AppState::new(dir.path()));
        let (status, _) = call(
            app.clone(),
            post_json(
                "/api/canvas/layout",
                &serde_json::json!({ "nodes": 2, "edges": [[0, 1], [1, 0]] }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = call(
            app.clone(),
            post_json(
                "/api/canvas/layout",
                &serde_json::json!({ "nodes": MAX_LAYOUT_NODES + 1, "edges": [] }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, body) = call(
            app,
            post_json(
                "/api/canvas/layout",
                &serde_json::json!({ "nodes": 3, "edges": [[0, 1], [1, 2]] }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let view: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(view["rank"], serde_json::json!([0, 1, 2]));
    }

    #[tokio::test]
    async fn source_routes_reject_unknown_language_and_oversized_source() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(AppState::new(dir.path()));
        let bad_lang = serde_json::json!({ "language": "cobol", "source": "x" });
        let (status, _) = call(app.clone(), post_json("/api/cyber/review", &bad_lang)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let big =
            serde_json::json!({ "language": "rust", "source": "a".repeat(MAX_SOURCE_BYTES + 1) });
        let (status, _) = call(app, post_json("/api/cyber/review", &big)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn diff_marks_a_changed_function() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(AppState::new(dir.path()));
        let body = serde_json::json!({
            "language": "rust",
            "old": "fn a() -> u8 { 1 }\n",
            "new": "fn a() -> u8 { 2 }\n",
        });
        let (status, bytes) = call(app, post_json("/api/review/diff", &body)).await;
        assert_eq!(status, StatusCode::OK);
        let view: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(view["pairings"][0]["status"], "changed");
    }

    #[test]
    fn wav_header_describes_the_samples() {
        let wav = wav_bytes(&[0.0, 1.0, -1.0], 24_000);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(
            u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]),
            24_000
        );
        assert_eq!(wav.len(), 44 + 6);
    }
}
