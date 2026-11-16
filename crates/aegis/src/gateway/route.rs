//! Where a request to a provider goes, and which wire format it carries.
//!
//! A subscription provider has one upstream and one format. A Bedrock or
//! Vertex provider serves several clients, each speaking its own format to
//! its own endpoint, so the request path decides both.
use crate::{
    config::{is_cloud_region, vertex_url},
    providers::Provider,
};
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct ProviderTarget {
    pub upstream: Upstream,
    /// Whose prices apply to this provider's requests, as LiteLLM names the
    /// provider: `anthropic`, `openai`, `typesafe`, `vertex_ai`, or `bedrock`
    /// with its region.
    pub price_scope: Arc<str>,
}

#[derive(Clone)]
pub(crate) enum Upstream {
    Fixed {
        protocol: Provider,
        base: Base,
    },
    /// The runtime endpoint (Claude Code's InvokeModel, pi's Converse) and
    /// the OpenAI-compatible endpoint Codex uses. A custom base URL serves both.
    Bedrock {
        runtime: Base,
        openai: Base,
    },
    /// `None` picks the Google host from the location in each request path.
    Vertex {
        custom: Option<Base>,
    },
}

#[derive(Clone)]
pub(crate) struct Base {
    origin: Arc<str>,
    path: Arc<str>,
}

impl Base {
    pub fn parse(url: &str) -> anyhow::Result<Self> {
        let parsed = url::Url::parse(url)?;
        Ok(Self {
            origin: Arc::from(parsed.origin().ascii_serialization()),
            path: Arc::from(parsed.path().trim_end_matches('/')),
        })
    }

    fn url(&self, path: &str) -> String {
        if self.path.is_empty() || begins_with_segments(path, &self.path) {
            format!("{}{path}", self.origin)
        } else {
            format!("{}{}{path}", self.origin, self.path)
        }
    }
}

fn begins_with_segments(path: &str, prefix: &str) -> bool {
    let Some(rest) = path.strip_prefix(prefix) else {
        return false;
    };
    rest.is_empty() || rest.starts_with('/') || rest.starts_with('?')
}

/// One request's destination.
pub(crate) struct Route {
    pub url: String,
    /// The format of a model request, which is recorded with its usage.
    /// `None` is any other call, such as a token count: it is still masked
    /// unless the gateway's bypass list lets it through, but not recorded.
    pub protocol: Option<Provider>,
    /// The model a path names, for formats that keep it out of the body.
    pub model: Option<String>,
}

impl ProviderTarget {
    pub fn route(&self, path: &str) -> Route {
        let bare = path.split('?').next().unwrap_or(path);
        match &self.upstream {
            Upstream::Fixed { protocol, base } => Route {
                url: base.url(path),
                protocol: Some(*protocol),
                model: None,
            },
            Upstream::Bedrock { runtime, openai } => {
                // Bedrock's OpenAI-compatible host serves one API per prefix:
                // Responses for OpenAI models under `/openai/v1`, Anthropic
                // Messages for Claude under `/anthropic/v1`, and Chat
                // Completions for partner models under `/v1`.
                if ["/openai/", "/anthropic/", "/v1/"]
                    .iter()
                    .any(|prefix| bare.starts_with(prefix))
                {
                    return Route {
                        url: openai.url(path),
                        protocol: openai_compatible(bare),
                        model: None,
                    };
                }
                let (model, operation) = bedrock_model(bare).unzip();
                Route {
                    url: runtime.url(path),
                    protocol: match operation {
                        Some("invoke" | "invoke-with-response-stream") => Some(Provider::Anthropic),
                        Some("converse" | "converse-stream") => Some(Provider::BedrockConverse),
                        _ => None,
                    },
                    model,
                }
            }
            Upstream::Vertex { custom } => {
                // The Anthropic SDK leaves the version to its default base
                // URL, so a path without one would miss the API.
                let versioned = if path.starts_with("/projects/") {
                    format!("/v1{path}")
                } else {
                    path.to_owned()
                };
                let url = match custom {
                    Some(base) => base.url(&versioned),
                    None => format!("{}{versioned}", vertex_base(bare)),
                };
                let (model, method) = vertex_model(bare).unzip();
                let counting = model.as_deref() == Some("count-tokens");
                let protocol = if counting {
                    None
                } else if bare.contains("/publishers/openai/")
                    || bare.contains("/endpoints/openapi/")
                {
                    openai_compatible(bare)
                } else {
                    match method {
                        Some("rawPredict" | "streamRawPredict")
                            if bare.contains("/publishers/anthropic/") =>
                        {
                            Some(Provider::Anthropic)
                        }
                        Some("generateContent" | "streamGenerateContent") => Some(Provider::Gemini),
                        _ => None,
                    }
                };
                Route {
                    url,
                    protocol,
                    model: model.filter(|_| protocol.is_some()),
                }
            }
        }
    }
}

/// The format of a call to an OpenAI- or Anthropic-compatible API, by its
/// final path segments.
fn openai_compatible(path: &str) -> Option<Provider> {
    if path.ends_with("/responses") {
        Some(Provider::Codex)
    } else if path.ends_with("/chat/completions") {
        Some(Provider::ChatCompletions)
    } else if path.ends_with("/messages") {
        Some(Provider::Anthropic)
    } else {
        None
    }
}

/// `/model/{id}/{operation}`, with the ID percent-decoded: an inference
/// profile ARN arrives encoded.
fn bedrock_model(path: &str) -> Option<(String, &str)> {
    let rest = path.strip_prefix("/model/")?;
    let (id, operation) = rest.split_once('/')?;
    let id = percent_decode(id);
    (!id.is_empty()).then_some((id, operation))
}

/// `…/models/{model}:{method}`.
fn vertex_model(path: &str) -> Option<(String, &str)> {
    let (_, rest) = path.rsplit_once("/models/")?;
    let (model, method) = rest.split_once(':')?;
    (!model.is_empty() && !model.contains('/')).then(|| (percent_decode(model), method))
}

/// Vertex's base URL for the location in a path, as `vertex_url` names it.
/// A missing or malformed location falls back to the global host rather than
/// reaching a host built from arbitrary text.
fn vertex_base(path: &str) -> String {
    let location = path
        .split_once("/locations/")
        .map(|(_, rest)| rest.split('/').next().unwrap_or_default())
        .filter(|location| is_cloud_region(location));
    vertex_url(location.unwrap_or("global"))
}

fn percent_decode(value: &str) -> String {
    url::form_urlencoded::parse(value.replace('+', "%2B").as_bytes())
        .next()
        .map(|(key, _)| key.into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bedrock(url: &str) -> ProviderTarget {
        let runtime = Base::parse(url).unwrap();
        let openai = match crate::config::bedrock_region(url) {
            Some(region) => {
                Base::parse(&format!("https://bedrock-mantle.{region}.api.aws")).unwrap()
            }
            None => runtime.clone(),
        };
        ProviderTarget {
            upstream: Upstream::Bedrock { runtime, openai },
            price_scope: Arc::from("bedrock"),
        }
    }

    fn vertex() -> ProviderTarget {
        ProviderTarget {
            upstream: Upstream::Vertex { custom: None },
            price_scope: Arc::from("vertex_ai"),
        }
    }

    #[test]
    fn bedrock_routes_each_client_to_its_endpoint() {
        let target = bedrock("https://bedrock-runtime.eu-west-1.amazonaws.com");

        let claude = target
            .route("/model/eu.anthropic.claude-sonnet-4-6-v1%3A0/invoke-with-response-stream");
        assert_eq!(
            claude.url,
            "https://bedrock-runtime.eu-west-1.amazonaws.com/model/eu.anthropic.claude-sonnet-4-6-v1%3A0/invoke-with-response-stream"
        );
        assert_eq!(claude.protocol, Some(Provider::Anthropic));
        assert_eq!(
            claude.model.as_deref(),
            Some("eu.anthropic.claude-sonnet-4-6-v1:0")
        );

        let pi = target.route("/model/amazon.nova-pro-v1%3A0/converse-stream");
        assert_eq!(pi.protocol, Some(Provider::BedrockConverse));

        let codex = target.route("/openai/v1/responses");
        assert_eq!(
            codex.url,
            "https://bedrock-mantle.eu-west-1.api.aws/openai/v1/responses"
        );
        assert_eq!(codex.protocol, Some(Provider::Codex));

        let partner = target.route("/v1/chat/completions");
        assert_eq!(
            partner.url,
            "https://bedrock-mantle.eu-west-1.api.aws/v1/chat/completions"
        );
        assert_eq!(partner.protocol, Some(Provider::ChatCompletions));

        let messages = target.route("/anthropic/v1/messages");
        assert_eq!(
            messages.url,
            "https://bedrock-mantle.eu-west-1.api.aws/anthropic/v1/messages"
        );
        assert_eq!(messages.protocol, Some(Provider::Anthropic));
        assert_eq!(
            target.route("/anthropic/v1/messages/count_tokens").protocol,
            None
        );

        assert_eq!(target.route("/openai/v1/models").protocol, None);
        assert_eq!(
            target.route("/v1/models").url,
            "https://bedrock-mantle.eu-west-1.api.aws/v1/models"
        );
        assert_eq!(target.route("/model/x/count-tokens").protocol, None);

        let custom = bedrock("https://vpce.example.com/bedrock");
        assert_eq!(
            custom.route("/openai/v1/responses").url,
            "https://vpce.example.com/bedrock/openai/v1/responses"
        );
    }

    #[test]
    fn vertex_routes_by_location_and_publisher() {
        let target = vertex();
        let claude = target.route(
            "/v1/projects/p/locations/us-east5/publishers/anthropic/models/claude-sonnet-4-6:streamRawPredict",
        );
        assert_eq!(
            claude.url,
            "https://us-east5-aiplatform.googleapis.com/v1/projects/p/locations/us-east5/publishers/anthropic/models/claude-sonnet-4-6:streamRawPredict"
        );
        assert_eq!(claude.protocol, Some(Provider::Anthropic));
        assert_eq!(claude.model.as_deref(), Some("claude-sonnet-4-6"));

        let unversioned = target.route(
            "/projects/p/locations/global/publishers/anthropic/models/claude-opus-4-7:rawPredict",
        );
        assert!(
            unversioned
                .url
                .starts_with("https://aiplatform.googleapis.com/v1/projects/p/")
        );

        let codex = target.route("/v1/projects/p/locations/global/publishers/openai/v1/responses");
        assert_eq!(codex.protocol, Some(Provider::Codex));

        let partner =
            target.route("/v1/projects/p/locations/us-central1/endpoints/openapi/chat/completions");
        assert!(
            partner
                .url
                .starts_with("https://us-central1-aiplatform.googleapis.com/v1/")
        );
        assert_eq!(partner.protocol, Some(Provider::ChatCompletions));

        let gemini = target.route(
            "/v1/projects/p/locations/eu/publishers/google/models/gemini-3-pro:streamGenerateContent?alt=sse",
        );
        assert!(
            gemini
                .url
                .starts_with("https://aiplatform.eu.rep.googleapis.com/v1/")
        );
        assert!(gemini.url.ends_with("?alt=sse"));
        assert_eq!(gemini.protocol, Some(Provider::Gemini));
        assert_eq!(gemini.model.as_deref(), Some("gemini-3-pro"));

        let counted = target.route(
            "/v1/projects/p/locations/global/publishers/anthropic/models/count-tokens:rawPredict",
        );
        assert_eq!(
            counted.protocol, None,
            "token counts are not model requests"
        );
    }

    #[test]
    fn a_location_never_builds_an_arbitrary_host() {
        for path in [
            "/v1/projects/p/locations/evil.example.com%2F/publishers/google/models/m:generateContent",
            "/v1/projects/p/locations/EVIL/publishers/google/models/m:generateContent",
            "/v1/publishers/google/models/m:generateContent",
        ] {
            assert!(
                vertex()
                    .route(path)
                    .url
                    .starts_with("https://aiplatform.googleapis.com/"),
                "{path}"
            );
        }
    }
}
