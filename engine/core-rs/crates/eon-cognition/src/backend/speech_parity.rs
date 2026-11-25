use super::*;
use std::sync::Barrier;

// Unlike the warm gate, compare every generated token (and natural EOG) to
// a fresh context, fresh tokenizer calls and full-vocabulary grammar filtering.
#[test]
#[ignore = "requires the pinned Qwen GGUF"]
fn voice_checkpoint_and_direct_logits_match_uncached_constrained_greedy() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../perception/models/slm/Qwen3.5-0.8B-Q4_K_M.gguf");
    let model = LocalModel::load(path).unwrap();
    let vocab = model.model.vocab();
    let report = model.voice_header(SpeechAct::Report);
    let mut checks = 0;
    let stopped = model.start_speech(SpeechAct::Report, || {
        checks += 1;
        if checks == 3 {
            Err(llm_err("test", "cancelled"))
        } else {
            Ok(())
        }
    });
    assert!(stopped.is_err());
    assert!(
        report.checkpoint.get().is_none(),
        "must not publish a partial prefill"
    );
    for (act, text) in [
        (
            SpeechAct::Report,
            "Temperature: 19 degrees. State the reading in digits.",
        ),
        (
            SpeechAct::Notice,
            "The library is closed. Notify the listener briefly.",
        ),
        (SpeechAct::Report, "State the number of open tickets: 72."),
    ] {
        let mut tokens = vocab.tokenize(qwen_voice_header(act).as_bytes(), true, true);
        let header = model.voice_header(act);
        assert_eq!(tokens, header.tokens);
        let len = tokens.len();
        tokens.extend(vocab.tokenize(text.as_bytes(), false, false));
        tokens.extend(vocab.tokenize(QWEN_VOICE_REPLY.as_bytes(), false, true));

        let decode = |ctx: &mut LlamaContext<'_>, from| {
            LocalModel::decode_checked(
                &tokens,
                from,
                SPEECH_PREFILL_BATCH,
                true,
                || Ok(()),
                |b| ctx.decode(b).map_err(|e| llm_err("decode", e)),
            )
            .unwrap()
        };
        let mut full = model.new_context().unwrap();
        let logits = decode(&mut full, 0);
        let mut greedy = LlamaSampler::greedy();
        let grammar = LlamaSampler::grammar(&model.model, &speech_grammar(act), "root").unwrap();
        let mut constrained = LlamaSampler::chain_simple([grammar, LlamaSampler::greedy()]);
        let mut reference = Vec::new();
        assert!(
            model
                .sample_into(
                    &mut full,
                    |ctx, idx| {
                        let raw = greedy.sample(ctx, idx);
                        if vocab.is_eog(raw) {
                            return Ok(raw);
                        }
                        let chosen = constrained.sample(ctx, idx);
                        assert!(
                            !vocab.is_eog(chosen),
                            "reference hit grammar stop, not natural EOG"
                        );
                        Ok(chosen)
                    },
                    tokens.len(),
                    49,
                    logits,
                    |token| {
                        reference.push(token);
                        Ok(())
                    },
                )
                .unwrap()
        );
        assert!(!reference.is_empty());

        // Include both cold creation and an actual restore. On the last
        // case this Report checkpoint predates two different private turns.
        for _ in 0..2 {
            let (mut ctx, from) = model.start_speech(act, || Ok(())).unwrap();
            assert!(from > 0 && from <= len);
            assert!(header.checkpoint.get().unwrap().byte_len() > 0);
            let logits = decode(&mut ctx, from);
            let mut observed = Vec::new();
            model
                .sample_speech(act, &mut ctx, tokens.len(), 48, logits, |token| {
                    observed.push(token);
                    Ok(())
                })
                .unwrap();
            assert_eq!(observed, reference);
        }
        // A truncated request must not publish text or become the next prefix.
        assert!(model.generate_speech(act, text, 1).is_err());
    }
}

#[test]
#[ignore = "requires the pinned Qwen GGUF"]
fn concurrent_cold_reports_do_not_share_request_state() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../perception/models/slm/Qwen3.5-0.8B-Q4_K_M.gguf");
    let model = LocalModel::load(path).unwrap();
    let start = Barrier::new(2);
    let run = |prompt| {
        start.wait();
        model
            .generate_speech(SpeechAct::Report, prompt, 48)
            .unwrap()
    };
    std::thread::scope(|s| {
        let a = s.spawn(|| run("Gauge A: 15 psi. Report the pressure using digits."));
        let b = s.spawn(|| run("Gauge B: 63 psi. Report the pressure using digits."));
        for (reply, own, other) in [
            (a.join().unwrap(), "15", "63"),
            (b.join().unwrap(), "63", "15"),
        ] {
            let mut numbers = reply.split(|c: char| !c.is_ascii_digit());
            assert!(numbers.clone().any(|n| n == own));
            assert!(!numbers.any(|n| n == other));
        }
    });
}
