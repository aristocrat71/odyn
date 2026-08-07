//! The endpoints most people connect, with everything but the key already
//! known, so connecting one is a paste rather than a form.
//!
//! Every entry serves open-weight models, the only kind Odyn talks to. An entry
//! is otherwise just the `[providers.*]` table a user could have written, and an
//! unlisted endpoint is still reachable the long way round.

use crate::config::ProviderConfig;
use crate::providers::openai_compat::is_free;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Provider {
    /// The name its table takes in `odyn.toml`, and what `--provider` accepts.
    pub id: &'static str,
    pub label: &'static str,
    pub kind: &'static str,
    pub base_url: &'static str,
    /// Key shapes unique to this endpoint, so a pasted key can name its own
    /// provider. Empty where they look like anyone else's.
    pub key_prefixes: &'static [&'static str],
    /// Substrings that pick a starting model out of the endpoint's listing, best
    /// first. A miss falls back to the listing, so these cannot go stale enough
    /// to break a connection.
    pub model_hints: &'static [&'static str],
    /// The model to start on when the endpoint lists none at all.
    pub fallback_model: Option<&'static str>,
    /// Where a key comes from.
    pub keys_url: &'static str,
}

impl Provider {
    pub fn needs_key(&self) -> bool {
        self.kind != "ollama"
    }
}

/// Ordered as the connect panel offers them: free tier first, then local last.
pub const PROVIDERS: &[Provider] = &[
    Provider {
        id: "openrouter",
        label: "OpenRouter",
        kind: "openai_compat",
        base_url: "https://openrouter.ai/api/v1",
        key_prefixes: &["sk-or-"],
        model_hints: &["gpt-oss", "qwen", "deepseek", "llama"],
        fallback_model: None,
        keys_url: "https://openrouter.ai/keys",
    },
    Provider {
        id: "groq",
        label: "Groq",
        kind: "openai_compat",
        base_url: "https://api.groq.com/openai/v1",
        key_prefixes: &["gsk_"],
        model_hints: &["llama-3.3", "gpt-oss", "qwen"],
        fallback_model: None,
        keys_url: "https://console.groq.com/keys",
    },
    Provider {
        id: "cerebras",
        label: "Cerebras",
        kind: "openai_compat",
        base_url: "https://api.cerebras.ai/v1",
        key_prefixes: &["csk-"],
        model_hints: &["gpt-oss", "qwen", "llama"],
        fallback_model: None,
        keys_url: "https://cloud.cerebras.ai/platform",
    },
    Provider {
        id: "deepseek",
        label: "DeepSeek",
        kind: "openai_compat",
        base_url: "https://api.deepseek.com/v1",
        key_prefixes: &[],
        model_hints: &["deepseek-chat"],
        fallback_model: Some("deepseek-chat"),
        keys_url: "https://platform.deepseek.com/api_keys",
    },
    Provider {
        id: "together",
        label: "Together AI",
        kind: "openai_compat",
        base_url: "https://api.together.xyz/v1",
        key_prefixes: &[],
        model_hints: &["gpt-oss", "Qwen", "Llama"],
        fallback_model: None,
        keys_url: "https://api.together.ai/settings/api-keys",
    },
    Provider {
        id: "mistral",
        label: "Mistral",
        kind: "openai_compat",
        base_url: "https://api.mistral.ai/v1",
        key_prefixes: &[],
        model_hints: &["mistral-small", "magistral", "ministral"],
        fallback_model: None,
        keys_url: "https://console.mistral.ai/api-keys",
    },
    Provider {
        id: "zen",
        label: "OpenCode Zen",
        kind: "openai_compat",
        base_url: "https://opencode.ai/zen/v1",
        key_prefixes: &[],
        model_hints: &[],
        fallback_model: None,
        keys_url: "https://opencode.ai/auth",
    },
    Provider {
        id: "ollama",
        label: "Ollama",
        kind: "ollama",
        base_url: "http://localhost:11434",
        key_prefixes: &[],
        model_hints: &[],
        fallback_model: None,
        keys_url: "https://ollama.com/download",
    },
];

pub fn find(id: &str) -> Option<&'static Provider> {
    PROVIDERS.iter().find(|provider| provider.id == id)
}

/// The provider a pasted key belongs to, by longest matching prefix: `sk-or-`
/// is OpenRouter's before it is anyone's `sk-`.
pub fn detect(key: &str) -> Option<&'static Provider> {
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    PROVIDERS
        .iter()
        .filter_map(|provider| {
            provider
                .key_prefixes
                .iter()
                .filter(|prefix| key.starts_with(**prefix))
                .map(|prefix| (prefix.len(), provider))
                .max_by_key(|(length, _)| *length)
        })
        .max_by_key(|(length, _)| *length)
        .map(|(_, provider)| provider)
}

/// The table a connection writes. Anything the connection has no opinion about
/// is carried across from `existing`, so reconnecting is a new key rather than a
/// reset of a hand-edited table.
pub fn connected(
    provider: &Provider,
    api_key: &str,
    models: &[String],
    existing: Option<&ProviderConfig>,
) -> ProviderConfig {
    if !provider.needs_key() {
        return ProviderConfig::Ollama {
            base_url: provider.base_url.to_string(),
            keep_alive: match existing {
                Some(ProviderConfig::Ollama { keep_alive, .. }) => keep_alive.clone(),
                _ => None,
            },
        };
    }
    let (kept_env, kept_model) = match existing {
        Some(ProviderConfig::OpenAiCompat {
            api_key_env,
            default_model,
            ..
        }) => (api_key_env.clone(), default_model.clone()),
        _ => (None, None),
    };
    ProviderConfig::OpenAiCompat {
        base_url: provider.base_url.to_string(),
        api_key: Some(api_key.to_string()),
        // Harmless beside a literal key, which wins; deleting a line the user
        // wrote is no part of connecting one.
        api_key_env: kept_env,
        default_model: starting_model(provider, models).or(kept_model),
    }
}

/// The model a fresh connection starts on: the first hint any listed model
/// carries, else the first model listed, else the entry's fallback. A free model
/// wins even on a later hint.
pub fn starting_model(provider: &Provider, models: &[String]) -> Option<String> {
    let hinted = |free_only: bool| {
        provider.model_hints.iter().find_map(|hint| {
            models
                .iter()
                .find(|model| model.contains(hint) && (!free_only || is_free(model)))
                .cloned()
        })
    };
    hinted(true)
        .or_else(|| hinted(false))
        // Listings arrive free-first, so this lands on one too.
        .or_else(|| models.first().cloned())
        .or_else(|| provider.fallback_model.map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_id_is_a_name_the_config_and_the_cli_accept() {
        let mut seen = Vec::new();
        for provider in PROVIDERS {
            assert!(
                provider
                    .id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "{}",
                provider.id
            );
            assert!(!seen.contains(&provider.id), "duplicate id {}", provider.id);
            seen.push(provider.id);
            assert!(!provider.base_url.is_empty(), "{}", provider.id);
            assert!(
                !provider.base_url.ends_with('/'),
                "{} would double its slash",
                provider.id
            );
            assert_eq!(find(provider.id), Some(provider));
        }
    }

    #[test]
    fn a_pasted_key_names_its_own_provider() {
        let named = |key: &str| detect(key).map(|provider| provider.id);

        assert_eq!(named("sk-or-v1-0123456789"), Some("openrouter"));
        assert_eq!(named("  gsk_0123456789  "), Some("groq"));
        assert_eq!(named("csk-0123"), Some("cerebras"));

        // A shape half the industry issues belongs to nobody.
        assert_eq!(named("sk-0123456789"), None);
        assert_eq!(named(""), None);
        assert_eq!(named("   "), None);
    }

    #[test]
    fn the_starting_model_prefers_a_hint_then_the_listing() {
        let groq = find("groq").expect("groq");
        let listed = [
            "gpt-oss-120b".to_string(),
            "llama-3.3-70b-versatile".to_string(),
        ];
        // Hints win in their own order, not the listing's.
        assert_eq!(
            starting_model(groq, &listed),
            Some("llama-3.3-70b-versatile".to_string())
        );

        let unfamiliar = ["something-else".to_string()];
        assert_eq!(
            starting_model(groq, &unfamiliar),
            Some("something-else".to_string())
        );

        // Nothing listed: only an entry with a fallback still has an answer.
        assert_eq!(starting_model(groq, &[]), None);
        let deepseek = find("deepseek").expect("deepseek");
        assert_eq!(
            starting_model(deepseek, &[]),
            Some("deepseek-chat".to_string())
        );
    }

    #[test]
    fn a_free_model_beats_a_paid_one_on_an_earlier_hint() {
        let openrouter = find("openrouter").expect("openrouter");
        // Hints are gpt-oss, qwen, deepseek, llama — and only qwen is free.
        let listed = [
            "qwen/qwen3-next:free".to_string(),
            "openai/gpt-oss-120b".to_string(),
        ];
        assert_eq!(
            starting_model(openrouter, &listed),
            Some("qwen/qwen3-next:free".to_string())
        );

        let paid = [
            "qwen/qwen3-next".to_string(),
            "openai/gpt-oss-120b".to_string(),
        ];
        assert_eq!(
            starting_model(openrouter, &paid),
            Some("openai/gpt-oss-120b".to_string())
        );
    }

    #[test]
    fn only_remote_endpoints_want_a_key() {
        let ollama = find("ollama").expect("ollama");
        assert!(!ollama.needs_key());
        assert_eq!(ollama.kind, "ollama");
        for provider in PROVIDERS.iter().filter(|p| p.id != "ollama") {
            assert!(provider.needs_key(), "{}", provider.id);
            assert_eq!(provider.kind, "openai_compat", "{}", provider.id);
        }
    }

    #[test]
    fn connecting_writes_the_key_and_keeps_the_rest_of_a_hand_edited_table() {
        let groq = find("groq").expect("groq");
        let listed = ["llama-3.3-70b-versatile".to_string()];

        let fresh = connected(groq, "gsk_new", &listed, None);
        assert_eq!(
            fresh,
            ProviderConfig::OpenAiCompat {
                base_url: groq.base_url.to_string(),
                api_key: Some("gsk_new".to_string()),
                api_key_env: None,
                default_model: Some("llama-3.3-70b-versatile".to_string()),
            }
        );

        // Reconnecting with an empty listing must leave the model alone.
        let hand_edited = ProviderConfig::OpenAiCompat {
            base_url: groq.base_url.to_string(),
            api_key: Some("gsk_old".to_string()),
            api_key_env: Some("GROQ_API_KEY".to_string()),
            default_model: Some("a-model-i-chose".to_string()),
        };
        assert_eq!(
            connected(groq, "gsk_new", &[], Some(&hand_edited)),
            ProviderConfig::OpenAiCompat {
                base_url: groq.base_url.to_string(),
                api_key: Some("gsk_new".to_string()),
                api_key_env: Some("GROQ_API_KEY".to_string()),
                default_model: Some("a-model-i-chose".to_string()),
            }
        );
        assert_eq!(
            connected(groq, "gsk_new", &listed, Some(&hand_edited)),
            ProviderConfig::OpenAiCompat {
                base_url: groq.base_url.to_string(),
                api_key: Some("gsk_new".to_string()),
                api_key_env: Some("GROQ_API_KEY".to_string()),
                default_model: Some("llama-3.3-70b-versatile".to_string()),
            }
        );
    }

    #[test]
    fn connecting_a_local_endpoint_keeps_how_long_it_holds_a_model() {
        let ollama = find("ollama").expect("ollama");
        assert_eq!(
            connected(ollama, "", &[], None),
            ProviderConfig::Ollama {
                base_url: ollama.base_url.to_string(),
                keep_alive: None,
            }
        );

        let tuned = ProviderConfig::Ollama {
            base_url: ollama.base_url.to_string(),
            keep_alive: Some("30m".to_string()),
        };
        assert_eq!(connected(ollama, "", &[], Some(&tuned)), tuned);
    }

    /// Odyn talks to open-weight models only — CLAUDE.md §3.
    #[test]
    fn no_closed_weight_endpoint_is_offered() {
        for provider in PROVIDERS {
            let url = provider.base_url.to_ascii_lowercase();
            for closed in [
                "api.openai.com",
                "api.anthropic.com",
                "generativelanguage.googleapis.com",
                "aiplatform.googleapis.com",
                "api.x.ai",
                "azure.com",
            ] {
                assert!(!url.contains(closed), "{} is closed-weight", provider.id);
            }
        }
    }
}
