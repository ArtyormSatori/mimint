//! Onboarding: detect local backends and SUGGEST configuration.
//!
//! When no backend is configured anywhere, the CLI probes loopback for
//! Ollama (`127.0.0.1:11434`) and LM Studio (`127.0.0.1:1234`) and
//! prints a suggestion. It never writes or changes any configuration:
//! the user copies the suggestion into their config file.
//!
//! Probes are read-only `GET`s to loopback metadata endpoints, through
//! the guarded HTTP client (so they obey the egress policy), with a
//! short timeout. A probe that fails for any reason means "not
//! detected", never an error.

use crate::egress::{EgressPolicy, GuardedHttpClient};
use std::time::Duration;

/// Probe timeout: local servers answer fast; do not keep the user waiting.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// What the loopback probes found. `None` means "not detected"; `Some`
/// holds the model names the server reported (possibly empty).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalProbe {
    /// Models reported by Ollama `/api/tags`, when Ollama answered.
    pub ollama_models: Option<Vec<String>>,
    /// Models reported by LM Studio `/v1/models`, when it answered.
    pub lm_studio_models: Option<Vec<String>>,
}

/// Probe both loopback servers. Never fails: any error means not detected.
pub async fn probe_local_backends(policy: EgressPolicy) -> LocalProbe {
    let Ok(http) = GuardedHttpClient::new(policy, PROBE_TIMEOUT) else {
        return LocalProbe::default();
    };
    let ollama_models = probe_ollama(&http).await;
    let lm_studio_models = probe_lm_studio(&http).await;
    LocalProbe {
        ollama_models,
        lm_studio_models,
    }
}

async fn probe_ollama(http: &GuardedHttpClient) -> Option<Vec<String>> {
    let response = http.get("http://127.0.0.1:11434/api/tags").await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: serde_json::Value = response.json().await.ok()?;
    let models = body
        .get("models")
        .and_then(serde_json::Value::as_array)?
        .iter()
        .filter_map(|entry| entry.get("name")?.as_str().map(str::to_string))
        .collect();
    Some(models)
}

async fn probe_lm_studio(http: &GuardedHttpClient) -> Option<Vec<String>> {
    let response = http.get("http://127.0.0.1:1234/v1/models").await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: serde_json::Value = response.json().await.ok()?;
    let models = body
        .get("data")
        .and_then(serde_json::Value::as_array)?
        .iter()
        .filter_map(|entry| entry.get("id")?.as_str().map(str::to_string))
        .collect();
    Some(models)
}

/// Render the onboarding suggestion for a probe result. Pure text: the
/// caller prints it. The text always says no configuration was changed.
pub fn suggestion_text(probe: &LocalProbe, config_path: &str) -> String {
    let mut out = String::new();
    out.push_str("No backend is configured, and no configuration was changed.\n");
    match (&probe.ollama_models, &probe.lm_studio_models) {
        (Some(models), _) => {
            out.push_str("Ollama was detected at http://127.0.0.1:11434");
            if models.is_empty() {
                out.push_str(" (no models installed; install one with `ollama pull <model>`).\n");
            } else {
                out.push_str(&format!(" with models: {}.\n", models.join(", ")));
            }
            let model = models
                .first()
                .cloned()
                .unwrap_or_else(|| "<model-from-ollama-list>".to_string());
            out.push_str(&format!(
                "Suggested config (edit the model if needed) in {config_path}:\n\n\
                 default_backend = \"local\"\n\n\
                 [[backend]]\nname = \"local\"\nkind = \"ollama\"\nmodel = \"{model}\"\n"
            ));
        }
        (None, Some(models)) => {
            out.push_str("LM Studio was detected at http://127.0.0.1:1234");
            if models.is_empty() {
                out.push_str(" (no models loaded).\n");
            } else {
                out.push_str(&format!(" with models: {}.\n", models.join(", ")));
            }
            let model = models
                .first()
                .cloned()
                .unwrap_or_else(|| "<loaded-model-id>".to_string());
            out.push_str(&format!(
                "Suggested config (edit the model if needed) in {config_path}:\n\n\
                 default_backend = \"local\"\n\n\
                 [[backend]]\nname = \"local\"\nkind = \"lmstudio\"\nmodel = \"{model}\"\n"
            ));
        }
        (None, None) => {
            out.push_str(
                "No local backend was detected (Ollama on 127.0.0.1:11434 and LM Studio on \
                 127.0.0.1:1234 did not answer).\n\
                 Start Ollama or LM Studio and run again, or configure a backend in ",
            );
            out.push_str(config_path);
            out.push_str(
                " (see the example in the config docs), or pass --base-url and --model \
                 explicitly.\n",
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestion_names_detected_model_and_says_nothing_changed() {
        let probe = LocalProbe {
            ollama_models: Some(vec!["qwen2.5:3b".to_string(), "other:1b".to_string()]),
            lm_studio_models: None,
        };
        let text = suggestion_text(&probe, "/home/u/.config/mimint/config.toml");
        assert!(text.contains("no configuration was changed"), "{text}");
        assert!(text.contains("Ollama was detected"), "{text}");
        assert!(text.contains("qwen2.5:3b"), "{text}");
        assert!(text.contains("kind = \"ollama\""), "{text}");
        assert!(
            text.contains("/home/u/.config/mimint/config.toml"),
            "{text}"
        );
    }

    #[test]
    fn suggestion_without_detection_points_at_both_servers() {
        let probe = LocalProbe::default();
        let text = suggestion_text(&probe, "config.toml");
        assert!(text.contains("No local backend was detected"), "{text}");
        assert!(text.contains("127.0.0.1:11434"), "{text}");
        assert!(text.contains("127.0.0.1:1234"), "{text}");
    }

    #[test]
    fn suggestion_prefers_ollama_when_both_answer() {
        let probe = LocalProbe {
            ollama_models: Some(vec!["a".to_string()]),
            lm_studio_models: Some(vec!["b".to_string()]),
        };
        let text = suggestion_text(&probe, "config.toml");
        assert!(text.contains("Ollama was detected"), "{text}");
        assert!(text.contains("kind = \"ollama\""), "{text}");
    }
}
