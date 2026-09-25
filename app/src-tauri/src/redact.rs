//! Redaction for anything that leaves the process as text: the details log
//! shown in the UI, stderr, and (Phase 5) diagnostics.
//!
//! Two layers: exact known secret values, then shape-based patterns so
//! that secrets we don't know about still get caught (API keys, tickets,
//! launch secrets, bearer headers).

const MASK: &str = "[REDACTED]";

/// Runs of `[A-Za-z0-9_-]` this long that look random are masked. Vast keys
/// are 64 hex, CivitAI keys 32 hex, launch secrets and tickets 43 base64url.
const MIN_TOKEN_LEN: usize = 32;

pub fn redact(text: &str, known: &[&str]) -> String {
    let mut out = text.to_string();
    for s in known {
        // Very short "secrets" would mangle ordinary text; patterns cover them.
        if s.len() >= 8 {
            out = out.replace(s, MASK);
        }
    }
    let out = redact_bearer(&out);
    redact_tokens(&out)
}

fn redact_bearer(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = find_ci(rest, "bearer ") {
        let (head, tail) = rest.split_at(pos + "bearer ".len());
        out.push_str(head);
        let end = tail
            .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ',')
            .unwrap_or(tail.len());
        if end > 0 && &tail[..end] != MASK {
            out.push_str(MASK);
        } else {
            out.push_str(&tail[..end]);
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    hay.to_ascii_lowercase().find(needle)
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn looks_secret(run: &str) -> bool {
    if run.len() < MIN_TOKEN_LEN {
        return false;
    }
    let hex = run.chars().all(|c| c.is_ascii_hexdigit());
    let has_digit = run.chars().any(|c| c.is_ascii_digit());
    let has_alpha = run.chars().any(|c| c.is_ascii_alphabetic());
    // Word-like runs (e.g. long hostnames of dictionary words joined by '-')
    // have few digits and many dashes; keep those readable.
    let dashes = run.chars().filter(|&c| c == '-').count();
    hex || (has_digit && has_alpha && dashes * 8 < run.len())
}

fn redact_tokens(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if looks_secret(run) {
            out.push_str(MASK);
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in text.chars() {
        if is_token_char(c) {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_secrets_are_masked() {
        assert_eq!(
            redact("key=hunter2hunter2 end", &["hunter2hunter2"]),
            "key=[REDACTED] end"
        );
    }

    #[test]
    fn vast_key_shape() {
        let key = "a".repeat(20) + &"0123456789abcdef".repeat(3)[..44];
        assert_eq!(key.len(), 64);
        let r = redact(&format!("using {key} now"), &[]);
        assert_eq!(r, "using [REDACTED] now");
    }

    #[test]
    fn civitai_key_shape() {
        let r = redact("token 0123456789abcdef0123456789abcdef.", &[]);
        assert_eq!(r, "token [REDACTED].");
    }

    #[test]
    fn launch_secret_and_ticket_shapes() {
        // secrets.token_urlsafe(32)-shaped values
        let secret = "q3Jx_9bK-2mPzL8vR4tY7wN1cF6hD0sA5eG3uI9oX2k";
        let r = redact(
            &format!("https://h.trycloudflare.com/__auth?t={secret}"),
            &[],
        );
        assert_eq!(r, "https://h.trycloudflare.com/__auth?t=[REDACTED]");
    }

    #[test]
    fn bearer_headers() {
        assert_eq!(
            redact("Authorization: Bearer abc.def, next", &[]),
            "Authorization: Bearer [REDACTED], next"
        );
        assert_eq!(
            redact("authorization: bearer short", &[]),
            "authorization: bearer [REDACTED]"
        );
    }

    #[test]
    fn ordinary_text_survives() {
        let s = "instance 52519537 on offer 48328454 at thunder-west-textile-brothers.trycloudflare.com: $0.1068/hr";
        assert_eq!(redact(s, &[]), s);
        let long_words = "registering bananaSplitzXXL_121.safetensors";
        assert_eq!(redact(long_words, &[]), long_words);
    }

    #[test]
    fn short_known_values_are_not_blanket_replaced() {
        assert_eq!(redact("a b c", &["a"]), "a b c");
    }
}
