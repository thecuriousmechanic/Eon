//! Three-stage journal scrubber (docs/implementation/01, section 3.4).
//!
//! Stage 0 strips invisible Unicode steganography. Stage 1 redacts PEM private
//! keys and known secret prefixes. Stage 2 redacts high-entropy tokens that
//! are not paths, URLs, or hashes. Every redaction is replaced by a keyed-free
//! BLAKE3 tag so equal secrets still compare equal without being revealed.

use std::sync::OnceLock;

use aho_corasick::AhoCorasick;

const PEM_BEGIN: &str = "-----BEGIN ";
const JWT_PREFIX: &str = "eyJhbGci";
const BEARER: &str = "Bearer";
const KNOWN_PREFIXES: &[&str] = &[
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "github_pat_",
    "sk-proj-",
    "sk-ant-",
    "AKIA",
    "ASIA",
    "AIza",
    "xoxb-",
    "xoxp-",
    JWT_PREFIX,
    BEARER,
];

const BASE64_ENTROPY_BITS: f64 = 4.65;
const HEX_ENTROPY_BITS: f64 = 3.75;
const MIN_TOKEN_LEN: usize = 24;
const PATH_SEGMENT_MAX: usize = 24;

/// Removes Unicode Tag, zero-width, and bidi-override code points that can hide instructions.
pub fn strip_invisible(input: &str) -> String {
    input.chars().filter(|&c| !is_invisible(c)).collect()
}

fn is_invisible(c: char) -> bool {
    matches!(
        c as u32,
        0xE0000..=0xE007F | 0x200B..=0x200F | 0xFEFF | 0x202A..=0x202E | 0x2066..=0x2069
    )
}

fn known_prefixes() -> &'static AhoCorasick {
    static MATCHER: OnceLock<AhoCorasick> = OnceLock::new();
    MATCHER.get_or_init(|| {
        AhoCorasick::builder()
            .ascii_case_insensitive(true)
            .build(KNOWN_PREFIXES)
            .expect("static prefixes are valid")
    })
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-')
}

/// Shannon entropy in bits per byte.
pub fn shannon_bits_per_char(s: &str) -> f64 {
    let mut counts = [0usize; 256];
    for &b in s.as_bytes() {
        counts[b as usize] += 1;
    }
    let len = s.len() as f64;
    counts
        .iter()
        .filter(|&&n| n > 0)
        .map(|&n| {
            let p = n as f64 / len;
            -p * p.log2()
        })
        .sum()
}

/// Returns the `[REDACTED_SECRET:b3_...]` marker for a secret, identical for identical secrets.
pub fn redaction_marker(secret: &str) -> String {
    let digest = blake3::hash(secret.as_bytes());
    let hex = digest.to_hex();
    format!("[REDACTED_SECRET:b3_{}]", &hex.as_str()[..8])
}

/// Applies all three stages and returns the scrubbed text.
pub fn scrub(input: &str) -> String {
    let stripped = strip_invisible(input);
    let after_credentials = redact_credentials(&stripped);
    redact_high_entropy(&after_credentials)
}

fn redact_credentials(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0;
    for (start, begin) in input.match_indices(PEM_BEGIN) {
        if start < cursor {
            continue;
        }
        // Even for a malformed label, this scan stops no later than the next BEGIN's dashes.
        let Some((label, body)) = input[start + begin.len()..].split_once("-----") else {
            break;
        };
        if !is_private_key_label(label) {
            continue;
        }
        let footer = format!("-----END {label}-----");
        // A missing or mismatched END leaves the rest of this journal entry sensitive.
        let end = body
            .split_once(&footer)
            .map_or(input.len(), |(_, rest)| input.len() - rest.len());
        redact_known_prefixes(&input[cursor..start], &mut out);
        out.push_str(&redaction_marker(&input[start..end]));
        cursor = end;
    }
    redact_known_prefixes(&input[cursor..], &mut out);
    out
}

fn is_private_key_label(label: &str) -> bool {
    matches!(label, "PRIVATE KEY" | "PGP PRIVATE KEY BLOCK")
        || (label.ends_with(" PRIVATE KEY")
            && label.bytes().all(|b| {
                b.is_ascii_uppercase() || b.is_ascii_digit() || matches!(b, b' ' | b'-')
            }))
}

fn redact_known_prefixes(input: &str, out: &mut String) {
    let mut cursor = 0;
    for hit in known_prefixes().find_iter(input) {
        if hit.start() < cursor || starts_inside_token(input, hit.start()) {
            continue;
        }
        let prefix = KNOWN_PREFIXES[hit.pattern().as_usize()];
        let tail = &input[hit.end()..];
        let value = if prefix == BEARER {
            let value = tail.trim_start_matches([' ', '\t']);
            if value.len() == tail.len() || !value.starts_with(is_bearer_char) {
                continue;
            }
            value
        } else {
            // Only the HTTP authentication scheme is case-insensitive.
            if &input[hit.start()..hit.end()] != prefix {
                continue;
            }
            tail
        };
        let rest = value.trim_start_matches(|c| match prefix {
            BEARER => is_bearer_char(c),
            JWT_PREFIX => is_token_char(c) || c == '.',
            _ => is_token_char(c),
        });
        let end = input.len() - rest.len();
        out.push_str(&input[cursor..hit.start()]);
        out.push_str(&redaction_marker(&input[hit.start()..end]));
        cursor = end;
    }
    out.push_str(&input[cursor..]);
}

fn is_bearer_char(c: char) -> bool {
    is_token_char(c) || matches!(c, '.' | '~')
}

/// A prefix such as `xoxb-` inside `axoxb--0` is part of a longer word, not the start of a secret.
fn starts_inside_token(input: &str, at: usize) -> bool {
    input[..at].chars().next_back().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn redact_high_entropy(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut token_start: Option<usize> = None;
    for (i, c) in input.char_indices() {
        match (token_start, is_token_char(c)) {
            (None, true) => token_start = Some(i),
            (Some(start), false) => {
                out.push_str(&redact_token_if_secret(&input[..i], start));
                token_start = None;
                out.push(c);
            }
            (None, false) => out.push(c),
            (Some(_), true) => {}
        }
    }
    if let Some(start) = token_start {
        out.push_str(&redact_token_if_secret(input, start));
    }
    out
}

/// Redacts the token beginning at `start`. `text` must contain the token as its suffix.
fn redact_token_if_secret(text: &str, start: usize) -> String {
    let token = &text[start..];
    if is_secret_candidate(token, &text[..start]) {
        return redaction_marker(token);
    }
    // '=' is also a base64 token character. Only split off an exact hex value
    // after checking the whole token, so no previously redacted prefix is disclosed.
    if let Some((key, value)) = token.rsplit_once('=')
        && is_hex_token(value)
        && shannon_bits_per_char(value) > HEX_ENTROPY_BITS
    {
        format!("{key}={}", redaction_marker(value))
    } else {
        token.to_string()
    }
}

fn is_hex_token(token: &str) -> bool {
    matches!(token.len(), 40 | 64) && token.bytes().all(|b| b.is_ascii_hexdigit())
}

fn has_hash_label(before: &str, label: &str) -> bool {
    before.strip_suffix(label).is_some_and(|prefix| {
        prefix.is_empty()
            || prefix.ends_with(|c: char| {
                c.is_whitespace() || matches!(c, '=' | '@' | '(' | '[' | '{' | '"' | '\'' | ',' | ';')
            })
    })
}

fn is_secret_candidate(token: &str, before: &str) -> bool {
    if is_hex_token(token) {
        let labelled = (token.len() == 64 && has_hash_label(before, "sha256:"))
            || has_hash_label(before, "commit ");
        return !labelled && shannon_bits_per_char(token) > HEX_ENTROPY_BITS;
    }
    if token.len() < MIN_TOKEN_LEN || looks_like_path(token) {
        return false;
    }
    let has_upper = token.bytes().any(|b| b.is_ascii_uppercase());
    let has_lower = token.bytes().any(|b| b.is_ascii_lowercase());
    let has_digit = token.bytes().any(|b| b.is_ascii_digit());
    if !(has_upper && has_lower && has_digit) {
        return false;
    }
    shannon_bits_per_char(token) > BASE64_ENTROPY_BITS
}

/// Workspace paths such as `crates/eon-cognition/src/tactical_slm` are `/`-separated words.
/// Random base64 also contains `/`, so each segment must read as a word: one optional leading capital,
/// then only lowercase letters, digits, `_`, `-`, or `.`.
fn looks_like_path(token: &str) -> bool {
    if !token.contains('/') || token.contains('+') || token.contains('=') {
        return false;
    }
    token
        .split('/')
        .filter(|segment| !segment.is_empty())
        .all(|segment| segment.len() <= PATH_SEGMENT_MAX && is_word_like(segment))
}

fn is_word_like(segment: &str) -> bool {
    let mut chars = segment.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_uppercase());
    first_ok
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODULE_PATH: &str = "engine/core-rs/crates/eon-cognition/src/tactical_slm";

    #[test]
    fn invisible_code_points_are_stripped() {
        let hidden = "run\u{200B}this\u{E0041}\u{202E}now";
        assert_eq!(strip_invisible(hidden), "runthisnow");
    }

    #[test]
    fn github_token_is_redacted_by_prefix() {
        let line = "auth=ghp_FIXTURE-not-a-real-token end";
        let scrubbed = scrub(line);
        assert!(!scrubbed.contains("ghp_FIXTURE"));
        assert!(scrubbed.contains("[REDACTED_SECRET:b3_"));
        assert!(scrubbed.ends_with(" end"));
    }

    fn pem(label: &str, body: &str, newline: &str) -> String {
        format!("-----BEGIN {label}-----{newline}{body}{newline}-----END {label}-----")
    }

    #[test]
    fn multiline_private_keys_become_one_stable_marker() {
        for label in [
            "PRIVATE KEY", "ENCRYPTED PRIVATE KEY", "RSA PRIVATE KEY", "EC PRIVATE KEY",
            "DSA PRIVATE KEY", "OPENSSH PRIVATE KEY", "PGP PRIVATE KEY BLOCK",
        ] {
            for newline in ["\n", "\r\n"] {
                let body = ["paper", "pretend"].join(newline);
                let block = pem(label, &body, newline);
                let expected = format!("before {}{newline}after", redaction_marker(&block));
                assert_eq!(scrub(&format!("before {block}{newline}after")), expected);
                assert_eq!(scrub(&expected), expected);
            }
        }
    }

    #[test]
    fn multiple_blocks_preserve_public_certificate_and_intervening_text() {
        let cert = pem("CERTIFICATE", "cGFwZXI=", "\r\n");
        let first = pem("PRIVATE KEY", "paper", "\r\n");
        let last = pem("EC PRIVATE KEY", "pretend", "\n");
        let input = format!("{first}\r\n{cert}\nnotes: {MODULE_PATH}\n{last} done");
        let expected = format!(
            "{}\r\n{cert}\nnotes: {MODULE_PATH}\n{} done",
            redaction_marker(&first), redaction_marker(&last),
        );
        assert_eq!(scrub(&input), expected);
        assert_eq!(scrub(&expected), expected);
    }

    #[test]
    fn public_certificate_body_uses_the_ordinary_entropy_policy() {
        let body = ["ABCDEFGHIJKLMNOPQRSTUVWXYZ", "abcdefghijklmnopqrstuvwxyz", "0123456789+/"].concat();
        let cert = pem("CERTIFICATE", &body, "\n");
        assert_eq!(scrub(&cert), pem("CERTIFICATE", &redaction_marker(&body), "\n"));
    }

    #[test]
    fn private_key_without_matching_footer_redacts_through_end_of_entry() {
        let label = "RSA PRIVATE KEY";
        for tail in ["", "\r\n-----END CERTIFICATE-----\r\nmore notes"] {
            let block = format!("-----BEGIN {label}-----\r\npaper\r\npretend{tail}");
            assert_eq!(scrub(&format!("keep\n{block}")), format!("keep\n{}", redaction_marker(&block)));
        }
    }

    #[test]
    fn bearer_and_recognized_jwt_consume_all_dotted_segments() {
        let dotted = ["paper", "claims", "signature"].join(".");
        let jwt = format!("{JWT_PREFIX}{dotted}");
        for credential in [format!("{BEARER} {dotted}"), format!("bEaReR\t  {dotted}"), jwt] {
            let expected = format!("auth: {}, done.", redaction_marker(&credential));
            assert_eq!(scrub(&format!("auth: {credential}, done.")), expected);
            assert_eq!(scrub(&expected), expected);
        }
        let marker = format!("{BEARER} {}", redaction_marker(&dotted));
        assert_eq!(scrub(&marker), marker);
        let plain = "Bearer\nnext line; asia.example; crates/Bearer.rs";
        assert_eq!(scrub(plain), plain);
    }

    #[test]
    fn module_paths_survive_even_with_high_character_variety() {
        assert!(!scrub(MODULE_PATH).contains("REDACTED"));
    }

    #[test]
    fn hex_redaction_uses_digits_not_letter_case_and_marks_the_original_bytes() {
        for alphabet in ["0123456789abcdef", "0123456789ABCDEF", "0123456789AbCdEf"] {
            let digits = alphabet.repeat(4);
            for len in [40, 64] {
                let hex = &digits[..len];
                for prefix in ["", "token=", "api_key=", "commit token=", "sha256:token="] {
                    let expected = format!("{prefix}{}", redaction_marker(hex));
                    assert_eq!(scrub(&format!("{prefix}{hex}")), expected);
                    assert_eq!(scrub(&expected), expected);
                }
            }
        }
    }

    #[test]
    fn hex_entropy_threshold_is_strict_without_weakening_mixed_case_redaction() {
        // Twelve digits occur four times each; two occur eight times each: exactly 3.75 bits/digit.
        let at_limit = ["0123456789ab".repeat(4), "cd".repeat(8)].concat();
        assert_eq!(shannon_bits_per_char(&at_limit), HEX_ENTROPY_BITS);
        let below = at_limit.replacen('0', "c", 1);
        let above = at_limit.replacen('c', "e", 1);
        let mixed = below.replacen("cdcd", "cDCd", 1);
        assert!(shannon_bits_per_char(&below) < HEX_ENTROPY_BITS);
        assert!(shannon_bits_per_char(&mixed) > HEX_ENTROPY_BITS);
        assert_eq!(scrub(&mixed), redaction_marker(&mixed));
        for safe in [below, at_limit] {
            assert_eq!(scrub(&safe), safe);
            let assignment = format!("token={safe}");
            assert_eq!(scrub(&assignment), assignment);
        }
        assert_eq!(scrub(&above), redaction_marker(&above));
    }

    #[test]
    fn only_a_hash_with_its_own_standalone_label_is_exempt() {
        let digest = "02468ACE13579BDF".repeat(4);
        let commit = &digest[..40];
        for safe in [
            format!("sha256:{digest}"), format!("digest=sha256:{digest}"),
            format!("(commit {commit})"), format!("git commit {digest}"),
        ] {
            assert_eq!(scrub(&safe), safe);
        }
        for prefix in [
            "notsha256:", "metadata.sha256:", "user_sha256:", "notcommit ", "pre-commit ",
            "sha256=", "commit=", "sha256: ", "commit  ", "commit id: ", "SHA256:",
        ] {
            assert_eq!(scrub(&format!("{prefix}{digest}")), format!("{prefix}{}", redaction_marker(&digest)));
        }
        assert_eq!(scrub(&format!("sha256:{commit}")), format!("sha256:{}", redaction_marker(commit)));
    }

    #[test]
    fn hex_rule_does_not_extract_substrings_or_split_base64_padding() {
        let digits = "abcdef0123456789".repeat(5);
        for len in [39, 41, 63, 65] {
            let value = format!("token={}", &digits[..len]);
            assert_eq!(scrub(&value), value);
        }
        for (prefix, suffix) in [("id-", ""), ("src/", "/lib"), ("token=", "g"), ("", "==")] {
            let value = format!("{prefix}{}{suffix}", &digits[..64]);
            assert_eq!(scrub(&value), value);
        }
        let base64 = ["ABCDEFGHIJKLMNOPQRSTUVWXYZ", "abcdefghijklmnopqrstuvwxyz", "0123456789+/=="].concat();
        let assignment = format!("token={base64}");
        assert_eq!(scrub(&assignment), redaction_marker(&assignment));

        // Splitting first would disclose the prefix of this previously redacted mixed token.
        let mixed = format!("GHIJKLmnopQRstUVwxyz+/1={}", &digits[..64]);
        assert!(shannon_bits_per_char(&mixed) > BASE64_ENTROPY_BITS);
        assert_eq!(scrub(&mixed), redaction_marker(&mixed));
    }

    #[test]
    fn base64_secret_with_slashes_is_not_mistaken_for_a_path() {
        let scrubbed = scrub("key wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY end");
        assert!(!scrubbed.contains("bPxRfiCYEXAMPLEKEY"), "AWS-style key leaked: {scrubbed}");
    }

    #[test]
    fn leading_slash_fragment_after_a_dot_is_still_a_path() {
        assert!(!scrub("a./Src/Main/Lib/Module/Parser").contains("REDACTED"));
    }

    #[test]
    fn known_prefix_inside_a_longer_word_is_not_a_secret() {
        let path = "crates/axoxb--0/notes";
        assert_eq!(scrub(path), path);
    }

    #[test]
    fn unlabelled_random_token_is_redacted() {
        let token = "aB3dE5gH7jK9mN1pQ2rS4tU6vW8xY0zA1bC2dE3f";
        assert!(
            shannon_bits_per_char(token) > BASE64_ENTROPY_BITS,
            "fixture must be high-entropy"
        );
        assert!(scrub(&format!("token {token}")).contains("[REDACTED_SECRET:b3_"));
    }

    #[test]
    fn equal_secrets_produce_equal_markers() {
        let secret = "ghp_FIXTURE-not-a-real-token";
        assert_eq!(scrub(secret), scrub(secret));
        assert_ne!(
            scrub(secret),
            scrub("ghp_OTHERsecret0000000000000000000000")
        );
    }
}
