use std::time::Instant;

use eon_primitives::EonResult;
use html5gum::{DefaultEmitter, HtmlString, Token, Tokenizer};

use super::{Format, MAX_DOCUMENT_BYTES, MAX_TEXT_BYTES, check_deadline, denied, too_large};

// Local, iterative text extraction, not an HTML renderer. Nothing consumes
// URLs, handlers, styles, or subresources. Malformed markup can lose text; it
// cannot change the response type or authorize another operation.
pub(super) fn extract(input: &str, format: Format, deadline: Instant) -> EonResult<String> {
    check_deadline(deadline)?;
    if input.len() > MAX_DOCUMENT_BYTES {
        return Err(too_large(
            "research document",
            input.len() as u64,
            MAX_DOCUMENT_BYTES,
        ));
    }
    let mut out = PlainText {
        value: String::with_capacity(MAX_TEXT_BYTES),
        pending: None,
    };
    match format {
        Format::Plain => out.append(input, true)?,
        Format::Html => {
            let mut emitter = DefaultEmitter::default();
            emitter.naively_switch_states(true); // script, style, and other raw-text elements
            let mut skipped: Option<(HtmlString, usize)> = None;
            for token in Tokenizer::new_with_emitter(input, emitter) {
                check_deadline(deadline)?;
                let token = token.unwrap_or_else(|never| match never {});
                if let Some((root, depth)) = &mut skipped {
                    match token {
                        Token::StartTag(ref tag)
                            if root.as_slice() == b"head" && tag.name.as_slice() == b"body" =>
                        {
                            skipped = None; // </head> may be omitted
                        }
                        Token::StartTag(ref tag) if tag.name == *root => *depth += 1,
                        Token::EndTag(ref tag) if tag.name == *root => {
                            *depth -= 1;
                            if *depth == 0 {
                                skipped = None;
                            }
                        }
                        _ => {}
                    }
                    continue;
                }
                match token {
                    Token::StartTag(tag) => {
                        let name = tag.name.as_slice();
                        if is_block(name) {
                            out.break_line();
                        }
                        if tag.self_closing && matches!(name, b"svg" | b"math") {
                            continue;
                        }
                        if is_omitted(name)
                            || (!is_void(name) && tag.attributes.contains_key(b"hidden".as_slice()))
                        {
                            out.space();
                            skipped = Some((tag.name, 1));
                        }
                    }
                    Token::EndTag(tag) if is_block(&tag.name) => out.break_line(),
                    Token::String(s) => {
                        let s = std::str::from_utf8(&s.value)
                            .map_err(|_| denied("invalid token text"))?;
                        out.append(s, false)?;
                    }
                    _ => {}
                }
            }
        }
    }
    if out.value.is_empty() {
        return Err(denied("document has no readable text"));
    }
    Ok(out.value)
}

fn is_block(name: &[u8]) -> bool {
    matches!(
        name,
        b"p" | b"div"
            | b"br"
            | b"hr"
            | b"h1"
            | b"h2"
            | b"h3"
            | b"h4"
            | b"h5"
            | b"h6"
            | b"li"
            | b"main"
            | b"article"
            | b"section"
            | b"blockquote"
            | b"pre"
            | b"th"
            | b"td"
            | b"tr"
    )
}

fn is_void(name: &[u8]) -> bool {
    matches!(
        name,
        b"area"
            | b"base"
            | b"br"
            | b"col"
            | b"embed"
            | b"hr"
            | b"img"
            | b"input"
            | b"link"
            | b"meta"
            | b"param"
            | b"source"
            | b"track"
            | b"wbr"
    )
}

fn is_omitted(name: &[u8]) -> bool {
    matches!(
        name,
        b"head"
            | b"script"
            | b"style"
            | b"template"
            | b"noscript"
            | b"iframe"
            | b"canvas"
            | b"object"
            | b"svg"
            | b"math"
            | b"form"
            | b"textarea"
            | b"audio"
            | b"video"
            | b"nav"
            | b"footer"
    )
}

struct PlainText {
    value: String,
    pending: Option<char>,
}

impl PlainText {
    fn break_line(&mut self) {
        self.pending = Some('\n');
    }

    fn space(&mut self) {
        self.pending.get_or_insert(' ');
    }

    fn append(&mut self, text: &str, keep_lines: bool) -> EonResult<()> {
        for ch in text.chars() {
            if keep_lines && matches!(ch, '\n' | '\r') {
                self.break_line();
            } else if ch.is_whitespace() {
                self.space();
            } else if !ch.is_control()
                && !matches!(ch, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
            {
                // Entity-decoded markup and markup in text/plain remain text.
                let ch = match ch {
                    '<' => '‹',
                    '>' => '›',
                    c => c,
                };
                let separator = self.pending.take().filter(|_| !self.value.is_empty());
                let len = self.value.len() + usize::from(separator.is_some()) + ch.len_utf8();
                if len > MAX_TEXT_BYTES {
                    return Err(too_large("research text", len as u64, MAX_TEXT_BYTES));
                }
                if let Some(sep) = separator {
                    self.value.push(sep);
                }
                self.value.push(ch);
            }
        }
        Ok(())
    }
}
