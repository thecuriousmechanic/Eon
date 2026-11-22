//! Public-answer prompt policy. Prompts guide the model; the formatter enforces the wire format.

use crate::persona::SpeechAct;

pub fn speech_instructions(act: SpeechAct) -> String {
    format!(
        "You are Manas: Ciel. Give only the concise public answer, in one or two sentences. \
         Begin with exactly {} once. State facts, status or actions directly. No greetings, \
         filler, generic disclaimers, follow-up offers, analysis or internal monologue. \
         Do not claim execution or verification without evidence.",
        act.prefix()
    )
}

// Single-turn Qwen3.5 ChatML rendering with enable_thinking=false. Keep the untrusted user
// text in its own tokenizer call (parse_special=false), separate from both template fragments.
#[cfg(feature = "llama-backend")]
pub(crate) fn qwen_voice_header(act: SpeechAct) -> String {
    format!(
        "<|im_start|>system\n{}<|im_end|>\n<|im_start|>user\n",
        speech_instructions(act)
    )
}

#[cfg(feature = "llama-backend")]
// The pinned GGUF's tokenizer.chat_template emits an already CLOSED empty reasoning block
// for enable_thinking=false. A user-text switch is not equivalent to this assistant header.
pub(crate) const QWEN_VOICE_REPLY: &str =
    "<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n";

// Constrain the non-thinking turn, not its answer. The model generates the opener too;
// SpeechRouter remains responsible for validating the complete utterance after EOG.
#[cfg(feature = "llama-backend")]
pub(crate) fn speech_grammar(act: SpeechAct) -> String {
    let optional = if act == SpeechAct::Affirmative {
        "?"
    } else {
        ""
    };
    format!(
        r#"root ::= "{}" body{optional}
body ::= " " [^<>\x00-\x20\x7f-\x9f] [^<>\x00-\x1f\x7f-\x9f]{{0,255}}
"#,
        act.prefix()
    )
}
