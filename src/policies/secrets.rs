use super::detect::Detector;

pub static DETECTORS: [Detector; 12] = [
    Detector {
        name: "anthropic_api_key",
        pattern: r"\bsk-ant-[A-Za-z0-9_\-]{20,}",
        validate: None,
    },
    Detector {
        name: "openai_api_key",
        pattern: r"\bsk-(?:proj-|svcacct-|admin-)?[A-Za-z0-9_\-]{32,}",
        validate: None,
    },
    Detector {
        name: "github_token",
        pattern: r"\b(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{22,})",
        validate: None,
    },
    Detector {
        name: "gitlab_token",
        pattern: r"\bglpat-[A-Za-z0-9_\-]{20,}",
        validate: None,
    },
    Detector {
        name: "aws_access_key_id",
        pattern: r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b",
        validate: None,
    },
    Detector {
        name: "slack_token",
        pattern: r"\bxox[baprs]-[A-Za-z0-9\-]{10,}",
        validate: None,
    },
    Detector {
        name: "google_api_key",
        pattern: r"\bAIza[0-9A-Za-z_\-]{35}\b",
        validate: None,
    },
    Detector {
        name: "stripe_key",
        pattern: r"\b[sr]k_live_[0-9a-zA-Z]{24,}",
        validate: None,
    },
    Detector {
        name: "npm_token",
        pattern: r"\bnpm_[A-Za-z0-9]{36,}",
        validate: None,
    },
    Detector {
        name: "jwt",
        pattern: r"\beyJ[A-Za-z0-9_\-]{10,}\.eyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}",
        validate: None,
    },
    Detector {
        name: "pem_private_key",
        pattern: r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
        validate: None,
    },
    Detector {
        name: "aegis_api_key",
        pattern: r"\baegis_sk_[0-9a-f]{32}_[A-Za-z0-9_\-]{43}",
        validate: None,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policies::detect::{DetectorSet, select};

    // Assembled at runtime so the fixture is not itself flagged as a live key.
    fn stripe_shaped(prefix: &str) -> String {
        format!("{prefix}_live_{}", "ABCDEFGHIJKLMNOPQRSTUVWXYZ")
    }

    fn all() -> DetectorSet {
        DetectorSet::new(select(&DETECTORS, None))
    }

    #[test]
    fn every_vendor_prefix_is_recognised() {
        let cases = [
            (
                "sk-ant-api03-ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcd",
                "anthropic_api_key",
            ),
            (
                "sk-proj-ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789",
                "openai_api_key",
            ),
            ("sk-ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789", "openai_api_key"),
            ("ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789ab", "github_token"),
            (
                "github_pat_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_abcdef",
                "github_token",
            ),
            ("glpat-ABCDEFGHIJKLMNOPQRSTUVWX", "gitlab_token"),
            ("AKIAABCDEFGHIJKLMNOP", "aws_access_key_id"),
            ("xoxb-1234567890-ABCDEFGHIJ", "slack_token"),
            ("AIzaSyABCDEFGHIJKLMNOPQRSTUVWXYZ0123456", "google_api_key"),
            (&stripe_shaped("sk"), "stripe_key"),
            (&stripe_shaped("rk"), "stripe_key"),
            ("npm_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789ab", "npm_token"),
            (
                "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ0ZXN0In0.c2lnbmF0dXJlLXNpZ25hdHVyZQ",
                "jwt",
            ),
            (
                "-----BEGIN RSA PRIVATE KEY-----\nMIIEfake\nlines\n-----END RSA PRIVATE KEY-----",
                "pem_private_key",
            ),
            (
                "aegis_sk_0123456789abcdef0123456789abcdef_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopq",
                "aegis_api_key",
            ),
        ];
        let set = all();
        for (secret, expected) in cases {
            let text = format!("export X={secret} # done");
            let findings = set.find(&text);
            let [finding] = findings.as_slice() else {
                panic!("{secret} should produce exactly one finding");
            };
            assert_eq!(finding.detector, expected, "{secret}");
            assert_eq!(finding.secret, secret);
        }
    }

    #[test]
    fn ordinary_text_and_placeholders_produce_no_findings() {
        let set = all();
        for text in [
            "let key = derive_key(&root, b\"aegis/v1/oauth-hmac\");",
            "AEGIS_MASKED_GITHUB_TOKEN_0123456789abcdef012345_END",
            "AEGIS_MASKED_AEGIS_API_KEY_0123456789abcdef012345_END",
            "the task-1234 ticket mentions sk-later",
            "sk-shortkeyshortkeyshortkey",
            "AIzaSyABCDEFGHIJKLMNOPQRSTUVWXYZ0123456TOOLONG",
            "xoxo",
            "AKIA is an abbreviation",
        ] {
            assert!(set.find(text).is_empty(), "{text}");
        }
    }

    #[test]
    fn findings_are_reported_in_order_with_their_spans() {
        let text = "a ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789ab then AKIAABCDEFGHIJKLMNOP";
        let set = all();
        let findings = set.find(text);
        assert_eq!(
            findings
                .iter()
                .map(|finding| finding.detector)
                .collect::<Vec<_>>(),
            ["github_token", "aws_access_key_id"]
        );
        assert_eq!(
            &text[findings[1].start..findings[1].end],
            findings[1].secret
        );
    }

    #[test]
    fn a_disabled_detector_stops_matching() {
        let without_aws = DetectorSet::new(
            select(&DETECTORS, None)
                .into_iter()
                .filter(|detector| detector.name != "aws_access_key_id")
                .collect(),
        );
        assert!(without_aws.find("AKIAABCDEFGHIJKLMNOP").is_empty());
    }
}
