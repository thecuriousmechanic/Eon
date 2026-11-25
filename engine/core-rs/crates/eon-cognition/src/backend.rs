//! Local LLM generation over llama.cpp (docs/implementation/02, section 3).
//!
//! Greedy decoding only, so output is deterministic for a fixed model, prompt, and context size. That property is
//! what the end-to-end test relies on. Sampling policies come later and must not change this path.

use std::num::NonZeroU32;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Instant;

use eon_primitives::{EonError, EonResult};
use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::context::session::{LlamaStateSeqFlags, SeqState};
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::{LlamaModel, params::LlamaModelParams};
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::token::LlamaToken;
use llama_cpp_2::token::data::LlamaTokenData;
use llama_cpp_2::token::data_array::LlamaTokenDataArray;

use crate::persona::SpeechAct;
use crate::prompts::{QWEN_VOICE_REPLY, qwen_voice_header, speech_grammar};
use crate::router::{
    CognitiveRouter, DEFAULT_TURN_TIMEOUT, RouteError, SpeechBackend, SpeechBudget, SpeechRequest,
    SpeechTokens, Workload,
};

const CONTEXT_TOKENS: u32 = 2048;
// The safe binding does not interrupt an in-flight llama_decode. Do not hand it
// the whole voice prompt in one uninterruptible call.
const SPEECH_PREFILL_BATCH: usize = 32;

#[cfg(test)]
mod speech_parity;

/// llama.cpp allows one backend initialization per process, so every model shares this one.
fn shared_backend() -> EonResult<&'static LlamaBackend> {
    static BACKEND: OnceLock<Result<LlamaBackend, String>> = OnceLock::new();
    BACKEND
        .get_or_init(|| LlamaBackend::init().map_err(|e| e.to_string()))
        .as_ref()
        .map_err(|reason| llm_err("backend", reason))
}

/// A decoded prompt prefix and the context state after it.
pub struct PrefixCache {
    tokens: Vec<llama_cpp_2::token::LlamaToken>,
    state: Vec<u8>,
}

impl PrefixCache {
    pub fn token_count(&self) -> usize {
        self.tokens.len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedGeneration {
    pub text: String,
    pub reused_tokens: usize,
}

pub struct LocalModel {
    model: LlamaModel,
    // Per model and per act, never per caller or transcript. No live context is pooled.
    voice_headers: [OnceLock<VoiceHeader>; SpeechAct::ALL.len()],
}

struct VoiceHeader {
    tokens: Vec<LlamaToken>,
    checkpoint: OnceLock<SeqState>,
}

fn llm_err(context: &str, e: impl std::fmt::Display) -> EonError {
    EonError::OperationFailed {
        operation: format!("llm.{context}"),
        reason: e.to_string(),
    }
}

// If the unconstrained maximum is legal it is also the constrained maximum.
// Applying the grammar + greedy chain masks logits but does not advance it.
fn try_accept_speech_winner(constrained: &mut LlamaSampler, raw: LlamaToken) -> EonResult<bool> {
    let probe = LlamaTokenDataArray::from_iter([LlamaTokenData::new(raw, 0.0, 0.0)], false);
    Ok(accept_legal(constrained, probe)?.is_some())
}

fn accept_legal(
    constrained: &mut LlamaSampler,
    mut probe: LlamaTokenDataArray,
) -> EonResult<Option<LlamaToken>> {
    constrained.apply(&mut probe);
    let selected = probe.selected.and_then(|i| probe.data.get(i));
    if let Some(data) = selected.filter(|data| data.logit().is_finite()) {
        constrained
            .try_accept(data.id())
            .map_err(|e| llm_err("speech_grammar", e))?;
        return Ok(Some(data.id()));
    }
    Ok(None)
}

// Exact pruning, not top-k sampling. If *any* token in this band survives
// the grammar, it outranks every omitted token. Include all boundary ties in
// vocab order. If the band is too wide, or entirely illegal, use the full mask.
fn speech_band(logits: &[f32], raw: LlamaToken) -> Option<LlamaTokenDataArray> {
    let floor = logits[raw.0 as usize] - 8.0;
    if !floor.is_finite() {
        return None;
    }
    let mut data = Vec::new();
    for (id, &logit) in logits.iter().enumerate() {
        if logit >= floor {
            if data.len() == 256 {
                return None;
            }
            data.push(LlamaTokenData::new(LlamaToken(id as i32), logit, 0.0));
        }
    }
    Some(LlamaTokenDataArray::new(data, false))
}

// Speech contexts have no backend sampler. Read their raw logits directly,
// instead of materializing a full llama_token_data array to find one token.
// As in llama_sampler_greedy_apply, a tie never displaces the earlier id.
fn speech_winner(logits: &[f32]) -> EonResult<LlamaToken> {
    let mut best = *logits
        .first()
        .ok_or_else(|| llm_err("sample", "empty vocabulary"))?;
    let mut winner = 0;
    for (id, &logit) in logits.iter().enumerate().skip(1) {
        if logit > best {
            best = logit;
            winner = id;
        }
    }
    Ok(LlamaToken(winner as i32))
}

impl LocalModel {
    pub fn load(path: impl AsRef<Path>) -> EonResult<Self> {
        let model = LlamaModel::load_from_file(shared_backend()?, path, &LlamaModelParams::default())
            .map_err(|e| llm_err("load", e))?;
        Ok(Self {
            model,
            voice_headers: Default::default(),
        })
    }

    /// Runs one short generation so the weights and the compute graph are paged in before the first real request. Returns
    /// how long the warm-up took.
    pub fn warm_up(&self) -> EonResult<std::time::Duration> {
        let started = std::time::Instant::now();
        self.generate_greedy("Warm-up.", 1)?;
        Ok(started.elapsed())
    }

    /// Generates raw text, not verbal output. For speech use `generate_speech`.
    pub fn generate_greedy(&self, prompt: &str, max_tokens: usize) -> EonResult<String> {
        self.generate_with(prompt, max_tokens, LlamaSampler::greedy())
    }

    /// Answers an interactive turn locally: an exact reflex needs no inference;
    /// all other requests use this model's validated Qwen3.5 speech path.
    pub fn generate_speech(
        &self,
        act: SpeechAct,
        request: &str,
        max_tokens: usize,
    ) -> EonResult<String> {
        let budget = SpeechBudget::until(Instant::now() + DEFAULT_TURN_TIMEOUT);
        self.generate_speech_with_budget(act, request, max_tokens, &budget)
    }

    pub fn generate_speech_with_budget(
        &self,
        act: SpeechAct,
        request: &str,
        max_tokens: usize,
        budget: &SpeechBudget,
    ) -> EonResult<String> {
        let router = CognitiveRouter {
            tactical_local: Some(self),
            ..Default::default()
        };
        let request = SpeechRequest { act, text: request, max_tokens };
        router.plan(request, Workload::Interactive)
            .and_then(|plan| plan.execute_with_budget(budget))
            .map(|reply| reply.speech)
            .map_err(|error| match error {
                RouteError::InvalidRequest | RouteError::InvalidSpeech(_) => {
                    EonError::ValidationError(error.to_string())
                }
                _ => llm_err("speech", error),
            })
    }

    fn stream_speech(&self, request: SpeechRequest<'_>, out: &mut SpeechTokens) -> EonResult<()> {
        let arch = self
            .model
            .meta_val_str("general.architecture")
            .map_err(|e| llm_err("metadata", e))?;
        if !matches!(arch.as_str(), "qwen35" | "qwen35moe") {
            return Err(EonError::ValidationError(
                "unsupported model for Qwen3.5 speech template".into(),
            ));
        }
        out.check_active()?;
        let vocab = self.model.vocab();
        let header = self.voice_header(request.act);
        let mut tokens = header.tokens.clone();
        tokens.extend(vocab.tokenize(request.text.trim().as_bytes(), false, false));
        tokens.extend(vocab.tokenize(QWEN_VOICE_REPLY.as_bytes(), false, true));
        let max_tokens = request.max_tokens;
        if max_tokens > (CONTEXT_TOKENS as usize).saturating_sub(tokens.len()) || max_tokens == 0 {
            return Err(EonError::ValidationError(
                "invalid speech generation token budget".into(),
            ));
        }
        out.check_active()?;
        let (mut ctx, from) = self.start_speech(request.act, || out.check_active())?;
        let logits = Self::decode_checked(
            &tokens,
            from,
            SPEECH_PREFILL_BATCH,
            true,
            || out.check_active(),
            |batch| ctx.decode(batch).map_err(|e| llm_err("decode", e)),
        )?;
        out.check_active()?;
        self.sample_speech(request.act, &mut ctx, tokens.len(), max_tokens, logits, |token| {
            // Preserve model control tokens for the filter, not for speech.
            out.push_token(&vocab.token_to_piece(token, true, None))
        })
    }

    fn sample_speech(
        &self,
        act: SpeechAct,
        ctx: &mut LlamaContext<'_>,
        prompt_len: usize,
        max_tokens: usize,
        logits: i32,
        emit: impl FnMut(LlamaToken) -> EonResult<()>,
    ) -> EonResult<()> {
        let vocab = self.model.vocab();
        let constraint = LlamaSampler::grammar(&self.model, &speech_grammar(act), "root")
            .map_err(|e| llm_err("speech_grammar", e))?;
        let mut constrained = LlamaSampler::chain_simple([constraint, LlamaSampler::greedy()]);
        let complete = self.sample_into(
            ctx,
            |ctx, idx| {
                // Never mask a natural stop to manufacture a required body, or let the
                // grammar's length limit force EOG in the middle of an answer.
                let logits = ctx.get_logits_ith(idx);
                let raw = speech_winner(logits)?;
                if vocab.is_eog(raw) || try_accept_speech_winner(&mut constrained, raw)? {
                    return Ok(raw);
                }
                let shortlist = match speech_band(logits, raw) {
                    Some(band) => accept_legal(&mut constrained, band)?,
                    None => None,
                };
                let token = shortlist.unwrap_or_else(|| constrained.sample(ctx, idx));
                if vocab.is_eog(token) {
                    return Err(EonError::ValidationError("incomplete verbal turn".into()));
                }
                Ok(token)
            },
            prompt_len,
            // The sink caps non-EOG tokens. Probe the final logits for EOG even if
            // the last permitted text token exactly filled the budget.
            max_tokens + 1,
            logits,
            emit,
        )?;
        if !complete {
            return Err(EonError::ValidationError(
                "verbal turn exceeded its token budget".into(),
            ));
        }
        Ok(())
    }

    fn voice_header(&self, act: SpeechAct) -> &VoiceHeader {
        self.voice_headers[act as usize].get_or_init(|| VoiceHeader {
            tokens: self.model.vocab().tokenize(
                qwen_voice_header(act).as_bytes(), true, true,
            ),
            checkpoint: OnceLock::new(),
        })
    }

    // Only this function can publish a checkpoint. Start with a new context and
    // give it *only* template tokens: neither the prompt nor an old context is
    // an input. All private suffix, recurrent state, logits and sampler history
    // die with the turn, on success, error or cancellation.
    fn start_speech(
        &self,
        act: SpeechAct,
        mut check: impl FnMut() -> EonResult<()>,
    ) -> EonResult<(LlamaContext<'_>, usize)> {
        let mut ctx = self.new_speech_context()?;
        let header = self.voice_header(act);
        // Keep the original batch boundaries/numerics, including the batch
        // straddling the header/user boundary. Never ask for prefix logits.
        let len = header.tokens.len() / SPEECH_PREFILL_BATCH * SPEECH_PREFILL_BATCH;
        if len != 0 {
            check()?;
            if let Some(state) = header.checkpoint.get() {
                ctx.state_seq_set(state, 0)
                    .map_err(|e| llm_err("voice_prefix", e))?;
            } else {
                Self::decode_checked(
                    &header.tokens[..len],
                    0,
                    SPEECH_PREFILL_BATCH,
                    false,
                    &mut check,
                    |batch| ctx.decode(batch).map_err(|e| llm_err("decode", e)),
                )?;
                check()?;
                // Empty flags include *both* attention KV and recurrent memory
                // (Qwen3.5 is hybrid). A racing first turn can use its own decode.
                let state = ctx
                    .state_seq_get(0, LlamaStateSeqFlags::empty())
                    .map_err(|e| llm_err("voice_prefix", e))?;
                let _ = header.checkpoint.set(state);
            }
        }
        Ok((ctx, len))
    }

    fn new_speech_context(&self) -> EonResult<LlamaContext<'_>> {
        // Avoid a many-socket OpenMP team for these small, synchronous batches.
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(16)) as i32;
        let params = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(CONTEXT_TOKENS))
            .with_n_batch(SPEECH_PREFILL_BATCH as u32)
            .with_n_ubatch(SPEECH_PREFILL_BATCH as u32)
            .with_n_threads(threads)
            .with_n_threads_batch(threads);
        self.model
            .new_context(shared_backend()?, params)
            .map_err(|e| llm_err("context", e))
    }

    /// Generates greedily under a GBNF grammar, so every token keeps the output inside the grammar. The grammar's
    /// root rule is `root`. Generation stops when the grammar is complete or `max_tokens` is reached.
    pub fn generate_constrained(&self, prompt: &str, grammar: &str, max_tokens: usize) -> EonResult<String> {
        let constraint = LlamaSampler::grammar(&self.model, grammar, "root")
            .map_err(|e| llm_err("grammar", e))?;
        self.generate_with(prompt, max_tokens, LlamaSampler::chain_simple([constraint, LlamaSampler::greedy()]))
    }

    fn new_context(&self) -> EonResult<llama_cpp_2::context::LlamaContext<'_>> {
        let ctx_size = NonZeroU32::new(CONTEXT_TOKENS).expect("nonzero constant");
        self.model
            .new_context(shared_backend()?, LlamaContextParams::default().with_n_ctx(Some(ctx_size)))
            .map_err(|e| llm_err("context", e))
    }

    /// Decodes `tokens[from..]` as one batch, with logits for the last token only. Returns the batch index of those
    /// logits. `from` must leave at least one token.
    fn decode_from(
        ctx: &mut llama_cpp_2::context::LlamaContext<'_>,
        tokens: &[llama_cpp_2::token::LlamaToken],
        from: usize,
    ) -> EonResult<i32> {
        Self::decode_checked(
            tokens,
            from,
            tokens.len(),
            true,
            || Ok(()),
            |batch| ctx.decode(batch).map_err(|e| llm_err("decode", e)),
        )
    }

    fn decode_checked(
        tokens: &[LlamaToken],
        from: usize,
        batch_size: usize,
        last_logits: bool,
        mut check: impl FnMut() -> EonResult<()>,
        mut submit: impl FnMut(&mut LlamaBatch<'_>) -> EonResult<()>,
    ) -> EonResult<i32> {
        if from >= tokens.len() {
            return Err(EonError::ValidationError("at least one token must follow the cached prefix".into()));
        }
        let mut batch = LlamaBatch::new(batch_size.min(tokens.len() - from), 1);
        let last = tokens.len() - 1;
        for (chunk, pieces) in tokens[from..].chunks(batch_size).enumerate() {
            check()?;
            batch.clear();
            let start = from + chunk * batch_size;
            for (offset, token) in pieces.iter().enumerate() {
                let i = start + offset;
                batch
                    .add(*token, i as i32, &[0], last_logits && i == last)
                    .map_err(|e| llm_err("batch", e))?;
            }
            submit(&mut batch)?;
        }
        Ok(batch.n_tokens() - 1)
    }

    /// Samples from the logits at `logits_index` of the last decode, then keeps decoding one token at a time until
    /// end-of-generation (`true`) or `max_tokens` (`false`). The sink chooses how to interpret each token.
    fn sample_into(
        &self,
        ctx: &mut llama_cpp_2::context::LlamaContext<'_>,
        mut sample: impl FnMut(&LlamaContext<'_>, i32) -> EonResult<LlamaToken>,
        prompt_len: usize,
        max_tokens: usize,
        logits_index: i32,
        mut emit: impl FnMut(LlamaToken) -> EonResult<()>,
    ) -> EonResult<bool> {
        let vocab = self.model.vocab();
        let mut batch = LlamaBatch::new(1, 1);
        let mut logits_index = logits_index;
        for position in (prompt_len as i32..).take(max_tokens) {
            // `sample` also accepts the token into the sampler chain, so it must not be accepted again.
            let token = sample(ctx, logits_index)?;
            if vocab.is_eog(token) {
                return Ok(true);
            }
            emit(token)?;

            batch.clear();
            batch.add(token, position, &[0], true).map_err(|e| llm_err("batch", e))?;
            ctx.decode(&mut batch).map_err(|e| llm_err("decode", e))?;
            logits_index = 0;
        }
        Ok(false)
    }

    fn sample_loop(
        &self,
        ctx: &mut llama_cpp_2::context::LlamaContext<'_>,
        mut sampler: LlamaSampler,
        prompt_len: usize,
        max_tokens: usize,
        logits_index: i32,
    ) -> EonResult<String> {
        let mut output = Vec::new();
        let vocab = self.model.vocab();
        self.sample_into(
            ctx,
            |ctx, idx| Ok(sampler.sample(ctx, idx)),
            prompt_len,
            max_tokens,
            logits_index,
            |token| {
                output.extend(vocab.token_to_piece(token, false, None));
                Ok(())
            },
        )?;
        String::from_utf8(output).map_err(|e| llm_err("utf8", e))
    }

    fn generate_with(&self, prompt: &str, max_tokens: usize, sampler: LlamaSampler) -> EonResult<String> {
        let tokens = self.model.vocab().tokenize(prompt.as_bytes(), true, false);
        if tokens.is_empty() || tokens.len() + max_tokens > CONTEXT_TOKENS as usize {
            return Err(EonError::ValidationError("prompt does not fit the context window".into()));
        }
        let mut ctx = self.new_context()?;
        let logits = Self::decode_from(&mut ctx, &tokens, 0)?;
        self.sample_loop(&mut ctx, sampler, tokens.len(), max_tokens, logits)
    }

    /// Decodes a fixed prefix once and captures the resulting context state. Later prompts that start with the same
    /// tokens reuse it, so only their new tokens are decoded (docs/implementation/02, KV pre-cognition).
    pub fn build_prefix_cache(&self, prefix: &str) -> EonResult<PrefixCache> {
        let tokens = self.model.vocab().tokenize(prefix.as_bytes(), true, false);
        if tokens.is_empty() {
            return Err(EonError::ValidationError("prefix is empty".into()));
        }
        let mut ctx = self.new_context()?;
        Self::decode_from(&mut ctx, &tokens, 0)?;
        let mut state = vec![0u8; ctx.get_state_size()];
        // SAFETY: `state` has exactly `get_state_size()` bytes, which is the maximum llama.cpp writes.
        let written = unsafe { ctx.copy_state_data(state.as_mut_ptr()) };
        state.truncate(written);
        Ok(PrefixCache { tokens, state })
    }

    /// Greedy generation for `prompt`, reusing `cache` when the prompt starts with its tokens. Returns the text and the
    /// number of prompt tokens that were served from the cache (zero when it did not apply).
    pub fn generate_with_prefix(&self, cache: &PrefixCache, prompt: &str, max_tokens: usize) -> EonResult<CachedGeneration> {
        let tokens = self.model.vocab().tokenize(prompt.as_bytes(), true, false);
        if tokens.len() + max_tokens > CONTEXT_TOKENS as usize {
            return Err(EonError::ValidationError("prompt does not fit the context window".into()));
        }
        let reusable = tokens.len() > cache.tokens.len() && tokens.starts_with(&cache.tokens);
        let mut ctx = self.new_context()?;
        let reused = if reusable {
            // SAFETY: the state was produced by `copy_state_data` on a context of the same model and parameters.
            let read = unsafe { ctx.set_state_data(&cache.state) };
            if read != cache.state.len() {
                return Err(llm_err("state", format!("restored {read} of {} bytes", cache.state.len())));
            }
            cache.tokens.len()
        } else {
            0
        };
        let logits = Self::decode_from(&mut ctx, &tokens, reused)?;
        let text = self.sample_loop(&mut ctx, LlamaSampler::greedy(), tokens.len(), max_tokens, logits)?;
        Ok(CachedGeneration {
            text,
            reused_tokens: reused,
        })
    }
}

impl SpeechBackend for LocalModel {
    fn generate(&self, request: SpeechRequest<'_>, out: &mut SpeechTokens) -> EonResult<()> {
        self.stream_speech(request, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn speech_winner_matches_native_strict_greater_ordering() {
        for (logits, id) in [
            (&[-2., 1., 1., -3.][..], 1),
            (&[-0., 0.][..], 0),
            (&[f32::NEG_INFINITY, f32::NAN, f32::INFINITY][..], 2),
            (&[f32::NAN, 1.][..], 0),
        ] {
            assert_eq!(speech_winner(logits).unwrap(), LlamaToken(id));
        }
        assert!(speech_winner(&[]).is_err());
    }

    #[test]
    fn speech_band_keeps_boundary_ties_in_vocab_order_or_defers_to_full_mask() {
        let logits = [1.0, -0.5, 9.0, 1.0, 4.0];
        let band = speech_band(&logits, speech_winner(&logits).unwrap()).unwrap();
        let ids: Vec<_> = band.data.iter().map(|t| t.id().0).collect();
        assert_eq!(ids, [0, 2, 3, 4]);
        assert!(speech_band(&[1.; 257], LlamaToken(0)).is_none());
        assert!(speech_band(&[f32::INFINITY], LlamaToken(0)).is_none());
    }

    #[test]
    #[ignore = "requires the pinned Qwen GGUF"]
    fn speech_winner_probe_advances_only_on_acceptance() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../../perception/models/slm/Qwen3.5-0.8B-Q4_K_M.gguf");
        let model = LocalModel::load(path).unwrap();
        let token = |text: &[u8]| {
            let tokens = model.model.vocab().tokenize(text, false, false);
            let [id]: [_; 1] = tokens.try_into().unwrap();
            id
        };
        let (x, y) = (token(b"x"), token(b"y"));
        let grammar = LlamaSampler::grammar(&model.model, "root ::= \"xy\"", "root").unwrap();
        let mut constrained = LlamaSampler::chain_simple([grammar, LlamaSampler::greedy()]);
        // The entire high-score band is illegal. It must leave the parser
        // untouched for the exhaustive fallback, not accept its masked winner.
        let rejected = LlamaTokenDataArray::from_iter([
            LlamaTokenData::new(y, 9., 0.),
            LlamaTokenData::new(token(b"z"), 8., 0.),
        ], false);
        assert_eq!(accept_legal(&mut constrained, rejected).unwrap(), None);
        // Even a masked singleton is selected by greedy: inspect its logit, not its index.
        for (raw, allowed) in [(y, false), (x, true), (x, false), (y, true), (y, false)] {
            assert_eq!(
                try_accept_speech_winner(&mut constrained, raw).unwrap(),
                allowed
            );
        }
    }

    // Exercise native batch construction and the prefill stop points, not inference.
    #[test]
    fn speech_prefill_checks_before_each_bounded_batch() {
        let tokens = vec![LlamaToken(0); 2 * SPEECH_PREFILL_BATCH + 3];
        let mut lengths = Vec::new();
        let logits = LocalModel::decode_checked(
            &tokens,
            0,
            SPEECH_PREFILL_BATCH,
            true,
            || Ok(()),
            |b| {
                lengths.push(b.n_tokens());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(lengths, [32, 32, 3]);
        assert_eq!(logits, 2, "index must be relative to the final batch");

        let submitted = Cell::new(0);
        let stopped = LocalModel::decode_checked(
            &tokens,
            0,
            SPEECH_PREFILL_BATCH,
            true,
            || {
                if submitted.get() == 0 {
                    Ok(())
                } else {
                    Err(llm_err("test", "stopped"))
                }
            },
            |_| {
                submitted.set(submitted.get() + 1);
                Ok(())
            },
        );
        assert!(stopped.is_err());
        assert_eq!(submitted.get(), 1);
    }
}
