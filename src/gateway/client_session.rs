//! The client's own session (a CLI conversation, not a web sign-in), read from request headers.
use crate::providers::Provider;
use axum::http::HeaderMap;

/// Longer values are ignored, not truncated, so distinct sessions never merge.
pub(super) const MAX_SESSION_ID_LEN: usize = 128;

fn client_session_headers(protocol: Provider) -> &'static [&'static str] {
    match protocol {
        Provider::Anthropic => &["x-claude-code-session-id", "x-session-affinity"],
        // Older Codex releases send `session_id` or `conversation_id`.
        Provider::Codex => &["session-id", "session_id", "conversation_id"],
        Provider::ChatCompletions => &["x-session-affinity"],
        Provider::Gemini | Provider::TypeSafe | Provider::BedrockConverse => &[],
    }
}

pub(super) fn client_session_id(protocol: Provider, headers: &HeaderMap) -> Option<&str> {
    client_session_headers(protocol).iter().find_map(|name| {
        let value = headers.get(*name)?.to_str().ok()?;
        (!value.is_empty()
            && value.len() <= MAX_SESSION_ID_LEN
            && value.bytes().all(|byte| (b' '..=b'~').contains(&byte)))
        .then_some(value)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(name, value)| {
                (
                    axum::http::HeaderName::from_static(name),
                    HeaderValue::from_bytes(value.as_bytes()).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn claude_code_is_preferred_over_session_affinity() {
        let both = headers(&[
            ("x-session-affinity", "pi-session"),
            ("x-claude-code-session-id", "claude-session"),
        ]);
        assert_eq!(
            client_session_id(Provider::Anthropic, &both),
            Some("claude-session")
        );
        let pi = headers(&[("x-session-affinity", "pi-session")]);
        assert_eq!(
            client_session_id(Provider::Anthropic, &pi),
            Some("pi-session")
        );
    }

    #[test]
    fn codex_reads_session_id_then_the_legacy_names() {
        let all = headers(&[
            ("conversation_id", "conversation"),
            ("session_id", "legacy"),
            ("session-id", "current"),
        ]);
        assert_eq!(client_session_id(Provider::Codex, &all), Some("current"));
        let legacy = headers(&[
            ("conversation_id", "conversation"),
            ("session_id", "legacy"),
        ]);
        assert_eq!(client_session_id(Provider::Codex, &legacy), Some("legacy"));
        let oldest = headers(&[("conversation_id", "conversation")]);
        assert_eq!(
            client_session_id(Provider::Codex, &oldest),
            Some("conversation")
        );
    }

    #[test]
    fn a_header_is_read_only_for_the_format_whose_clients_send_it() {
        let claude = headers(&[("x-claude-code-session-id", "claude-session")]);
        assert_eq!(client_session_id(Provider::Codex, &claude), None);
        let codex = headers(&[("session-id", "codex-session")]);
        assert_eq!(client_session_id(Provider::Anthropic, &codex), None);
        let pi = headers(&[("x-session-affinity", "pi-session")]);
        assert_eq!(
            client_session_id(Provider::ChatCompletions, &pi),
            Some("pi-session")
        );
        for protocol in [
            Provider::Gemini,
            Provider::TypeSafe,
            Provider::BedrockConverse,
        ] {
            assert_eq!(client_session_id(protocol, &pi), None);
            assert_eq!(client_session_id(protocol, &codex), None);
        }
    }

    #[test]
    fn an_unusable_value_is_ignored_and_the_next_header_is_read() {
        let too_long = "a".repeat(MAX_SESSION_ID_LEN + 1);
        let longest = "a".repeat(MAX_SESSION_ID_LEN);
        for bad in ["", too_long.as_str(), "tab\there", "caf\u{e9}"] {
            let with_fallback = headers(&[
                ("x-claude-code-session-id", bad),
                ("x-session-affinity", "fallback"),
            ]);
            assert_eq!(
                client_session_id(Provider::Anthropic, &with_fallback),
                Some("fallback"),
                "{bad:?}"
            );
            let alone = headers(&[("session-id", bad)]);
            assert_eq!(client_session_id(Provider::Codex, &alone), None, "{bad:?}");
        }
        let printable = headers(&[("session-id", "a b~!{}")]);
        assert_eq!(
            client_session_id(Provider::Codex, &printable),
            Some("a b~!{}")
        );
        let at_the_cap = headers(&[("session-id", longest.as_str())]);
        assert_eq!(
            client_session_id(Provider::Codex, &at_the_cap),
            Some(longest.as_str())
        );
    }
}
