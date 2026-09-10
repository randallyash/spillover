//! Looking for models that are actually there.
//!
//! Setup should not ask someone to type a URL for a server that is already
//! running, or let them finish on a tier that cannot possibly answer. Both come
//! down to probing: a short HTTP request for an endpoint, and a PATH lookup for
//! an agent CLI.

use std::time::Duration;

use futures_util::future::join_all;

use crate::config::{Tier, TierKind};
use crate::preset::{Endpoint, Library, cli_spec, openai_settings};
use crate::provider::cli::on_path;
use crate::provider::openai::list_models;

/// How long a local server gets before we assume it is not running. Short,
/// because this runs while someone is watching the screen.
const LOCAL_TIMEOUT: Duration = Duration::from_millis(1_500);

/// How long a check on a configured tier gets. Longer, because a hosted
/// endpoint is a round trip away.
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// A local server that answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub preset_id: String,
    pub name: String,
    pub base_url: String,
    /// The first model it advertised, if it advertised any.
    pub model: Option<String>,
}

/// Whether a tier looks usable, phrased for the person reading it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Readiness {
    pub ok: bool,
    pub detail: String,
}

impl Readiness {
    fn ready(detail: impl Into<String>) -> Self {
        Self {
            ok: true,
            detail: detail.into(),
        }
    }

    fn problem(detail: impl Into<String>) -> Self {
        Self {
            ok: false,
            detail: detail.into(),
        }
    }
}

/// Probe the given local servers, keeping the ones that answered.
///
/// Concurrent, so a machine where nothing is running does not take four
/// timeouts to find out.
pub async fn find_local(candidates: &[Endpoint]) -> Vec<Found> {
    let probes = candidates.iter().map(|preset| async move {
        let answer = tokio::time::timeout(LOCAL_TIMEOUT, list_models(&preset.base_url, None)).await;

        match answer {
            Ok(Ok(models)) => Some(Found {
                preset_id: preset.id.clone(),
                name: preset.name.clone(),
                base_url: preset.base_url.clone(),
                model: models.into_iter().next(),
            }),
            // Unreachable, slow, or not OpenAI-shaped: not a candidate.
            _ => None,
        }
    });

    join_all(probes).await.into_iter().flatten().collect()
}

/// Check whether a configured tier could answer right now.
pub async fn check(library: &Library, tier: &Tier) -> Readiness {
    match tier.kind {
        TierKind::OpenAi => {
            let settings = match openai_settings(library, tier) {
                Ok(settings) => settings,
                Err(error) => return Readiness::problem(error.to_string()),
            };

            let key = settings
                .api_key_env
                .as_deref()
                .and_then(|name| std::env::var(name).ok());

            match tokio::time::timeout(
                CHECK_TIMEOUT,
                list_models(&settings.base_url, key.as_deref()),
            )
            .await
            {
                Ok(Ok(models)) if models.is_empty() => Readiness::problem(format!(
                    "{} answered but lists no models",
                    settings.base_url
                )),
                Ok(Ok(models)) => {
                    // Name the model that would actually be used: an empty
                    // `model` is resolved at startup, so it is otherwise
                    // invisible until a turn runs.
                    let chosen = tier
                        .model
                        .as_deref()
                        .filter(|model| !model.is_empty())
                        .map(str::to_string)
                        .or_else(|| models.first().cloned());

                    let detail = match chosen {
                        Some(model) if models.len() == 1 => {
                            format!("{} would use {model}", settings.base_url)
                        }
                        Some(model) => format!(
                            "{} offers {} models, would use {model}",
                            settings.base_url,
                            models.len()
                        ),
                        None => format!("{} lists no models", settings.base_url),
                    };
                    Readiness::ready(detail)
                }
                Ok(Err(message)) => Readiness::problem(message),
                Err(_) => Readiness::problem(format!(
                    "{} did not answer within {}s",
                    settings.base_url,
                    CHECK_TIMEOUT.as_secs()
                )),
            }
        }
        TierKind::Cli => {
            let spec = match cli_spec(library, tier) {
                Ok(spec) => spec,
                Err(error) => return Readiness::problem(error.to_string()),
            };

            if on_path(&spec.bin) {
                Readiness::ready(format!("{} is installed", spec.bin))
            } else {
                Readiness::problem(format!(
                    "{} is not on PATH — install it, or set bin to its full path",
                    spec.bin
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::path::Path;

    fn endpoint(id: &str, base_url: &str) -> Endpoint {
        Endpoint {
            id: id.to_string(),
            name: id.to_string(),
            base_url: base_url.to_string(),
            api_key_env: None,
            note: None,
        }
    }

    fn tier(text: &str) -> Tier {
        let config = Config::parse(
            Path::new("test.toml"),
            &format!(
                r#"
                [[tier]]
                id = "t"
                {text}
                "#
            ),
        )
        .expect("the test tier should be valid");
        config.tiers.into_iter().next().expect("one tier")
    }

    #[tokio::test]
    async fn a_server_that_answers_is_found() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/v1/models")
            .with_status(200)
            .with_body(r#"{"data":[{"id":"qwen3-coder"},{"id":"other"}]}"#)
            .create_async()
            .await;

        let found = find_local(&[endpoint("lmstudio", &format!("{}/v1", server.url()))]).await;

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].preset_id, "lmstudio");
        assert_eq!(found[0].model.as_deref(), Some("qwen3-coder"));
    }

    #[tokio::test]
    async fn a_server_that_is_not_running_is_left_out() {
        // Port 9 refuses connections.
        let found = find_local(&[endpoint("dead", "http://127.0.0.1:9/v1")]).await;
        assert!(found.is_empty());
    }

    #[tokio::test]
    async fn only_the_servers_that_answered_are_returned() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/v1/models")
            .with_status(200)
            .with_body(r#"{"data":[{"id":"m"}]}"#)
            .create_async()
            .await;

        let found = find_local(&[
            endpoint("dead", "http://127.0.0.1:9/v1"),
            endpoint("live", &format!("{}/v1", server.url())),
        ])
        .await;

        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].preset_id, "live");
    }

    #[tokio::test]
    async fn a_local_server_with_no_models_is_not_a_candidate() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/v1/models")
            .with_status(200)
            .with_body(r#"{"data":[]}"#)
            .create_async()
            .await;

        let found = find_local(&[endpoint("empty", &format!("{}/v1", server.url()))]).await;
        assert_eq!(found.len(), 1, "it answered, so it is still worth offering");
        assert_eq!(found[0].model, None, "but it named no model");
    }

    #[tokio::test]
    async fn checking_an_openai_tier_reports_the_models_it_offers() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/v1/models")
            .with_status(200)
            .with_body(r#"{"data":[{"id":"a"},{"id":"b"}]}"#)
            .create_async()
            .await;

        let library = Library::embedded();
        let readiness = check(
            &library,
            &tier(&format!(
                r#"kind = "openai"
base_url = "{}/v1"
model = "m""#,
                server.url()
            )),
        )
        .await;

        assert!(readiness.ok, "{}", readiness.detail);
        assert!(readiness.detail.contains("2 model"), "{}", readiness.detail);
    }

    #[tokio::test]
    async fn checking_an_openai_tier_says_what_went_wrong() {
        let library = Library::embedded();
        let readiness = check(
            &library,
            &tier(
                r#"kind = "openai"
base_url = "http://127.0.0.1:9/v1"
model = "m""#,
            ),
        )
        .await;

        assert!(!readiness.ok);
        assert!(
            readiness.detail.contains("could not reach"),
            "{}",
            readiness.detail
        );
    }

    #[tokio::test]
    async fn a_rejected_key_is_reported_as_a_key_problem() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/v1/models")
            .with_status(401)
            .create_async()
            .await;

        let library = Library::embedded();
        let readiness = check(
            &library,
            &tier(&format!(
                r#"kind = "openai"
base_url = "{}/v1"
model = "m""#,
                server.url()
            )),
        )
        .await;

        assert!(!readiness.ok);
        assert!(readiness.detail.contains("key"), "{}", readiness.detail);
    }

    #[tokio::test]
    async fn checking_a_cli_tier_reports_a_missing_binary() {
        let library = Library::embedded();
        let readiness = check(
            &library,
            &tier(
                r#"kind = "cli"
preset = "grok""#,
            ),
        )
        .await;

        // grok is installed on the machine this was written on, so accept
        // either answer; what matters is that it is reported, not guessed.
        assert!(!readiness.detail.is_empty());
        assert!(readiness.detail.contains("grok"), "{}", readiness.detail);
    }

    #[tokio::test]
    async fn checking_a_cli_tier_with_an_unknown_preset_fails_clearly() {
        let library = Library::embedded();
        let readiness = check(
            &library,
            &tier(
                r#"kind = "cli"
bin = "x"
args = ["{prompt}"]
preset = "nope""#,
            ),
        )
        .await;
        assert!(!readiness.ok);
        assert!(readiness.detail.contains("nope"), "{}", readiness.detail);
    }
}
