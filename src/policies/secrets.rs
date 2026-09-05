use regex::Regex;
use std::sync::LazyLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Detector {
    AnthropicApiKey,
    OpenAiApiKey,
    GitHubToken,
    GitLabToken,
    AwsAccessKeyId,
    SlackToken,
    GoogleApiKey,
    StripeKey,
    NpmToken,
    Jwt,
    PemPrivateKey,
    AegisApiKey,
}

impl Detector {
    pub const ALL: [Detector; 12] = [
        Self::AnthropicApiKey,
        Self::OpenAiApiKey,
        Self::GitHubToken,
        Self::GitLabToken,
        Self::AwsAccessKeyId,
        Self::SlackToken,
        Self::GoogleApiKey,
        Self::StripeKey,
        Self::NpmToken,
        Self::Jwt,
        Self::PemPrivateKey,
        Self::AegisApiKey,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::AnthropicApiKey => "anthropic_api_key",
            Self::OpenAiApiKey => "openai_api_key",
            Self::GitHubToken => "github_token",
            Self::GitLabToken => "gitlab_token",
            Self::AwsAccessKeyId => "aws_access_key_id",
            Self::SlackToken => "slack_token",
            Self::GoogleApiKey => "google_api_key",
            Self::StripeKey => "stripe_key",
            Self::NpmToken => "npm_token",
            Self::Jwt => "jwt",
            Self::PemPrivateKey => "pem_private_key",
            Self::AegisApiKey => "aegis_api_key",
        }
    }

    fn pattern(self) -> &'static str {
        match self {
            Self::AnthropicApiKey => r"\bsk-ant-[A-Za-z0-9_\-]{20,}",
            Self::OpenAiApiKey => r"\bsk-(?:proj-|svcacct-|admin-)?[A-Za-z0-9_\-]{32,}",
            Self::GitHubToken => r"\b(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{22,})",
            Self::GitLabToken => r"\bglpat-[A-Za-z0-9_\-]{20,}",
            Self::AwsAccessKeyId => r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b",
            Self::SlackToken => r"\bxox[baprs]-[A-Za-z0-9\-]{10,}",
            Self::GoogleApiKey => r"\bAIza[0-9A-Za-z_\-]{35}\b",
            Self::StripeKey => r"\b[sr]k_live_[0-9a-zA-Z]{24,}",
            Self::NpmToken => r"\bnpm_[A-Za-z0-9]{36,}",
            Self::Jwt => r"\beyJ[A-Za-z0-9_\-]{10,}\.eyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}",
            Self::PemPrivateKey => {
                r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----"
            }
            Self::AegisApiKey => r"\baegis_sk_[0-9a-f]{32}_[A-Za-z0-9_\-]{43}",
        }
    }
}

static DETECTORS: LazyLock<Regex> = LazyLock::new(|| {
    let alternatives = Detector::ALL
        .iter()
        .map(|detector| format!("(?P<{}>{})", detector.name(), detector.pattern()))
        .collect::<Vec<_>>()
        .join("|");
    Regex::new(&alternatives).expect("detector patterns are valid")
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding<'a> {
    pub detector: Detector,
    pub secret: &'a str,
    pub start: usize,
    pub end: usize,
}

pub fn find_secrets(text: &str) -> Vec<Finding<'_>> {
    DETECTORS
        .captures_iter(text)
        .filter_map(|captures| {
            Detector::ALL.iter().find_map(|detector| {
                captures.name(detector.name()).map(|matched| Finding {
                    detector: *detector,
                    secret: matched.as_str(),
                    start: matched.start(),
                    end: matched.end(),
                })
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Assembled at runtime so the fixture is not itself flagged as a live key.
    fn stripe_shaped(prefix: &str) -> String {
        format!("{prefix}_live_{}", "ABCDEFGHIJKLMNOPQRSTUVWXYZ")
    }

    fn detectors(text: &str) -> Vec<Detector> {
        find_secrets(text)
            .into_iter()
            .map(|finding| finding.detector)
            .collect()
    }

    #[test]
    fn every_vendor_prefix_is_recognised() {
        let cases = [
            (
                "sk-ant-api03-ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcd",
                Detector::AnthropicApiKey,
            ),
            (
                "sk-proj-ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789",
                Detector::OpenAiApiKey,
            ),
            (
                "sk-ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789",
                Detector::OpenAiApiKey,
            ),
            (
                "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789ab",
                Detector::GitHubToken,
            ),
            (
                "github_pat_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_abcdef",
                Detector::GitHubToken,
            ),
            ("glpat-ABCDEFGHIJKLMNOPQRSTUVWX", Detector::GitLabToken),
            ("AKIAABCDEFGHIJKLMNOP", Detector::AwsAccessKeyId),
            ("xoxb-1234567890-ABCDEFGHIJ", Detector::SlackToken),
            (
                "AIzaSyABCDEFGHIJKLMNOPQRSTUVWXYZ0123456",
                Detector::GoogleApiKey,
            ),
            (&stripe_shaped("sk"), Detector::StripeKey),
            (&stripe_shaped("rk"), Detector::StripeKey),
            (
                "npm_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789ab",
                Detector::NpmToken,
            ),
            (
                "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ0ZXN0In0.c2lnbmF0dXJlLXNpZ25hdHVyZQ",
                Detector::Jwt,
            ),
            (
                "-----BEGIN RSA PRIVATE KEY-----\nMIIEfake\nlines\n-----END RSA PRIVATE KEY-----",
                Detector::PemPrivateKey,
            ),
            (
                "aegis_sk_0123456789abcdef0123456789abcdef_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopq",
                Detector::AegisApiKey,
            ),
        ];
        for (secret, expected) in cases {
            let text = format!("export X={secret} # done");
            let findings = find_secrets(&text);
            let [finding] = findings.as_slice() else {
                panic!("{secret} should produce exactly one finding");
            };
            assert_eq!(finding.detector, expected, "{secret}");
            assert_eq!(finding.secret, secret);
        }
    }

    #[test]
    fn ordinary_text_and_placeholders_produce_no_findings() {
        for text in [
            "let key = derive_key(&root, b\"aegis/v1/oauth-hmac\");",
            "AEGIS_SECRET_0123456789abcdef012345_END",
            "the task-1234 ticket mentions sk-later",
            "sk-shortkeyshortkeyshortkey",
            "AIzaSyABCDEFGHIJKLMNOPQRSTUVWXYZ0123456TOOLONG",
            "xoxo",
            "AKIA is an abbreviation",
        ] {
            assert!(detectors(text).is_empty(), "{text}");
        }
    }

    #[test]
    fn findings_are_reported_in_order_with_their_spans() {
        let text = "a ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789ab then AKIAABCDEFGHIJKLMNOP";
        let findings = find_secrets(text);
        assert_eq!(
            findings
                .iter()
                .map(|finding| finding.detector)
                .collect::<Vec<_>>(),
            [Detector::GitHubToken, Detector::AwsAccessKeyId]
        );
        assert_eq!(
            &text[findings[1].start..findings[1].end],
            findings[1].secret
        );
    }
}
