//! `odyn.toml`: which providers exist, how much of the brain to inject, and how
//! Spotlight behaves.
//!
//! API keys never live in the file — `api_key_env` names an environment
//! variable, and it is read only when that provider is actually built.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::brevity::Brevity;
use crate::chat::ChatProvider;
use crate::providers::ollama::OllamaProvider;
use crate::providers::openai_compat::{OpenAiCompatProvider, ProviderInitError};

const CONFIG_FILE_NAME: &str = "odyn.toml";
const CONFIG_PATH_ENV: &str = "ODYN_CONFIG";
const DEFAULT_OLLAMA_BASE_URL: &str = "http://localhost:11434";

/// Written on first run and then parsed as the loaded configuration, so the
/// file on disk and the running configuration can never disagree.
const DEFAULT_CONFIG: &str = r#"# Odyn configuration.
#
# Keys are never stored here: `api_key_env` names an environment variable that
# holds the key, read only when that provider is used.

default_provider = "ollama"

# Local models, no key required.
[providers.ollama]
kind = "ollama"
base_url = "http://localhost:11434"
keep_alive = "5m"  # how long a model stays in RAM; "0" unloads it immediately

# Any number of OpenAI-compatible endpoints can be added. Uncomment one, export
# its key in your shell, and point `default_provider` at it.
#
# [providers.deepseek]
# kind = "openai_compat"
# base_url = "https://api.deepseek.com"
# api_key_env = "DEEPSEEK_API_KEY"
# default_model = "deepseek-chat"
#
# [providers.zen]
# kind = "openai_compat"
# base_url = "https://api.opencode.ai/zen/v1"
# api_key_env = "OPENCODE_API_KEY"

[memory]
core_budget_tokens = 500
episodic_top_k = 6
episodic_cap_tokens = 900
similarity_edge_threshold = 0.78

[style]
brevity = "off"        # off | lite | full | ultra — default for new conversations

[spotlight]
hotkey = "CmdOrCtrl+Shift+Space"
brevity = "full"       # spotlight answers should be terse
# provider = "ollama"    # falls back to default_provider when unset
# model = "llama3.3:8b"
"#;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not create {}: {source}", path.display())]
    Directory {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not write {}: {source}", path.display())]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not locate a config directory; set {CONFIG_PATH_ENV} to a config file path")]
    NoConfigDir,
    /// toml's own message, which names the offending key and quotes its line.
    #[error("invalid config: {0}")]
    Parse(String),
    #[error("invalid config: {key}: {message}")]
    Invalid { key: String, message: String },
    #[error("no such key: {0}")]
    UnknownKey(String),
    #[error("no provider named `{0}` is configured")]
    UnknownProvider(String),
    #[error("providers.{name}.api_key_env: environment variable `{var}` is empty or not set")]
    MissingApiKey { name: String, var: String },
    #[error("providers.{name}: {source}")]
    Provider {
        name: String,
        source: ProviderInitError,
    },
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub default_provider: String,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub style: StyleConfig,
    #[serde(default)]
    pub spotlight: SpotlightConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum ProviderConfig {
    #[serde(rename = "openai_compat")]
    OpenAiCompat {
        base_url: String,
        api_key_env: Option<String>,
        default_model: Option<String>,
    },
    #[serde(rename = "ollama")]
    Ollama {
        #[serde(default = "default_ollama_base_url")]
        base_url: String,
        keep_alive: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MemoryConfig {
    pub core_budget_tokens: u32,
    pub episodic_top_k: u32,
    pub episodic_cap_tokens: u32,
    pub similarity_edge_threshold: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StyleConfig {
    /// The brevity every conversation starts with, unless it chose its own.
    pub brevity: Brevity,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SpotlightConfig {
    pub hotkey: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub brevity: Brevity,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            core_budget_tokens: 500,
            episodic_top_k: 6,
            episodic_cap_tokens: 900,
            similarity_edge_threshold: 0.78,
        }
    }
}

impl Default for SpotlightConfig {
    fn default() -> Self {
        Self {
            hotkey: "CmdOrCtrl+Shift+Space".to_string(),
            provider: None,
            model: None,
            brevity: Brevity::Full,
        }
    }
}

impl ProviderConfig {
    /// The `kind` value from the file, so callers can label a provider without
    /// matching on this enum.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::OpenAiCompat { .. } => "openai_compat",
            Self::Ollama { .. } => "ollama",
        }
    }

    fn base_url(&self) -> &str {
        match self {
            Self::OpenAiCompat { base_url, .. } | Self::Ollama { base_url, .. } => base_url,
        }
    }
}

impl Config {
    /// Loads `ODYN_CONFIG`, or `odyn.toml` in the platform config directory.
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from(config_path()?)
    }

    pub fn load_from(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        Self::parse(&read_or_create(path.as_ref())?)
    }

    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let config: Self =
            toml::from_str(text).map_err(|err| ConfigError::Parse(err.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    /// The model a new conversation starts on for `provider`. Only
    /// `openai_compat` entries declare one; Ollama has no such notion, so the
    /// answer there is `None` until a model is chosen.
    pub fn default_model(&self, provider: &str) -> Option<&str> {
        match self.providers.get(provider) {
            Some(ProviderConfig::OpenAiCompat { default_model, .. }) => default_model.as_deref(),
            _ => None,
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if !self.providers.contains_key(&self.default_provider) {
            return Err(unconfigured("default_provider", &self.default_provider));
        }
        if let Some(name) = &self.spotlight.provider {
            if !self.providers.contains_key(name) {
                return Err(unconfigured("spotlight.provider", name));
            }
        }
        for (name, provider) in &self.providers {
            if provider.base_url().trim().is_empty() {
                return Err(invalid(
                    format!("providers.{name}.base_url"),
                    "must not be empty",
                ));
            }
            if let ProviderConfig::OpenAiCompat {
                api_key_env: Some(var),
                ..
            } = provider
            {
                if !is_env_var_name(var) {
                    return Err(invalid(
                        format!("providers.{name}.api_key_env"),
                        "must name an environment variable (letters, digits, underscores)",
                    ));
                }
            }
        }
        if self.memory.episodic_top_k == 0 {
            return Err(invalid("memory.episodic_top_k", "must be at least 1"));
        }
        let threshold = self.memory.similarity_edge_threshold;
        if !(threshold > 0.0 && threshold <= 1.0) {
            return Err(invalid(
                "memory.similarity_edge_threshold",
                "must be greater than 0 and at most 1",
            ));
        }
        Ok(())
    }
}

/// Providers are built on demand: an unset `DEEPSEEK_API_KEY` must not stop
/// Odyn from talking to a local Ollama.
#[derive(Debug, Clone)]
pub struct ProviderRegistry {
    default_provider: String,
    providers: BTreeMap<String, ProviderConfig>,
}

impl ProviderRegistry {
    pub fn from_config(config: &Config) -> Result<Self, ConfigError> {
        config.validate()?;
        Ok(Self {
            default_provider: config.default_provider.clone(),
            providers: config.providers.clone(),
        })
    }

    pub fn default_provider_name(&self) -> &str {
        &self.default_provider
    }

    /// Sorted, so menus and `--help` output never reshuffle.
    pub fn names(&self) -> Vec<&str> {
        self.providers.keys().map(String::as_str).collect()
    }

    pub fn kind(&self, name: &str) -> Option<&'static str> {
        self.providers.get(name).map(ProviderConfig::kind)
    }

    pub fn provider(&self, name: &str) -> Result<Box<dyn ChatProvider>, ConfigError> {
        let config = self
            .providers
            .get(name)
            .ok_or_else(|| ConfigError::UnknownProvider(name.to_string()))?;
        let built = |source| ConfigError::Provider {
            name: name.to_string(),
            source,
        };
        match config {
            ProviderConfig::OpenAiCompat {
                base_url,
                api_key_env,
                ..
            } => {
                // No `api_key_env` means no auth header, which is what keyless
                // endpoints expect.
                let api_key = api_key_env
                    .as_deref()
                    .map(|var| read_api_key(name, var))
                    .transpose()?;
                let provider =
                    OpenAiCompatProvider::new(base_url, api_key, Vec::new()).map_err(built)?;
                Ok(Box::new(provider))
            }
            ProviderConfig::Ollama {
                base_url,
                keep_alive,
            } => {
                let provider = OllamaProvider::new(base_url, keep_alive.clone()).map_err(built)?;
                Ok(Box::new(provider))
            }
        }
    }
}

fn read_api_key(name: &str, var: &str) -> Result<String, ConfigError> {
    std::env::var(var)
        .ok()
        .filter(|key| !key.trim().is_empty())
        .ok_or_else(|| ConfigError::MissingApiKey {
            name: name.to_string(),
            var: var.to_string(),
        })
}

/// A missing file is created from the default template rather than being an
/// error: the first run should just work.
pub(crate) fn read_or_create(path: &Path) -> Result<String, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            write_default(path)?;
            Ok(DEFAULT_CONFIG.to_string())
        }
        Err(source) => Err(ConfigError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn write_default(path: &Path) -> Result<(), ConfigError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|source| ConfigError::Directory {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(path, DEFAULT_CONFIG).map_err(|source| ConfigError::Write {
        path: path.to_path_buf(),
        source,
    })
}

/// `ODYN_CONFIG`, or `odyn.toml` in the platform config directory.
pub fn config_path() -> Result<PathBuf, ConfigError> {
    if let Some(path) = std::env::var_os(CONFIG_PATH_ENV).filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let dirs = directories::ProjectDirs::from("", "", "odyn").ok_or(ConfigError::NoConfigDir)?;
    Ok(dirs.config_dir().join(CONFIG_FILE_NAME))
}

/// The file's text and the path it came from. Read on demand rather than kept
/// beside the parsed `Config`: the GUI shows the file as it is now, edits and
/// all, not as it was when the app started.
pub fn read_config_file() -> Result<(PathBuf, String), ConfigError> {
    let path = config_path()?;
    let text = read_or_create(&path)?;
    Ok((path, text))
}

fn default_ollama_base_url() -> String {
    DEFAULT_OLLAMA_BASE_URL.to_string()
}

fn is_env_var_name(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with(|c: char| c.is_ascii_digit())
        && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub(crate) fn invalid(key: impl Into<String>, message: &str) -> ConfigError {
    ConfigError::Invalid {
        key: key.into(),
        message: message.to_string(),
    }
}

fn unconfigured(key: &str, name: &str) -> ConfigError {
    invalid(key, &format!("no provider named `{name}` is configured"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{ChatError, ChatRequest, Message, Role};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// The sample from IMPLEMENTATION_PLAN.md, comments included.
    const SAMPLE: &str = r#"
default_provider = "deepseek"
[providers.deepseek]        # any number of OpenAI-compatible entries
kind = "openai_compat"
base_url = "https://api.deepseek.com"
api_key_env = "DEEPSEEK_API_KEY"   # v1: keys via env var reference only
default_model = "deepseek-chat"
[providers.zen]
kind = "openai_compat"
base_url = "https://api.opencode.ai/zen/v1"
api_key_env = "OPENCODE_API_KEY"
[providers.ollama]
kind = "ollama"
base_url = "http://localhost:11434"
keep_alive = "5m"
[memory]
core_budget_tokens = 500
episodic_top_k = 6
episodic_cap_tokens = 900
similarity_edge_threshold = 0.78
[spotlight]
hotkey = "CmdOrCtrl+Shift+Space"
provider = "ollama"
model = "llama3.3:8b"
"#;

    const MINIMAL: &str = r#"
default_provider = "ollama"
[providers.ollama]
kind = "ollama"
"#;

    /// A unique directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            Self(
                std::env::temp_dir()
                    .join(format!("odyn-test-{}-{label}-{unique}", std::process::id())),
            )
        }

        fn config(&self) -> PathBuf {
            self.0.join(CONFIG_FILE_NAME)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// `Box<dyn ChatProvider>` is not `Debug`, so `expect_err` is unavailable.
    fn provider_error(registry: &ProviderRegistry, name: &str) -> String {
        match registry.provider(name) {
            Err(err) => err.to_string(),
            Ok(_) => panic!("expected building `{name}` to fail"),
        }
    }

    #[test]
    fn parses_the_documented_sample() {
        let config = Config::parse(SAMPLE).expect("parse sample");

        assert_eq!(config.default_provider, "deepseek");
        assert_eq!(
            config.providers["deepseek"],
            ProviderConfig::OpenAiCompat {
                base_url: "https://api.deepseek.com".to_string(),
                api_key_env: Some("DEEPSEEK_API_KEY".to_string()),
                default_model: Some("deepseek-chat".to_string()),
            }
        );
        assert_eq!(
            config.providers["zen"],
            ProviderConfig::OpenAiCompat {
                base_url: "https://api.opencode.ai/zen/v1".to_string(),
                api_key_env: Some("OPENCODE_API_KEY".to_string()),
                default_model: None,
            }
        );
        assert_eq!(
            config.providers["ollama"],
            ProviderConfig::Ollama {
                base_url: DEFAULT_OLLAMA_BASE_URL.to_string(),
                keep_alive: Some("5m".to_string()),
            }
        );
        assert_eq!(config.memory, MemoryConfig::default());
        assert_eq!(
            config.spotlight,
            SpotlightConfig {
                hotkey: "CmdOrCtrl+Shift+Space".to_string(),
                provider: Some("ollama".to_string()),
                model: Some("llama3.3:8b".to_string()),
                ..SpotlightConfig::default()
            }
        );
    }

    #[test]
    fn missing_sections_and_fields_fall_back_to_defaults() {
        let config = Config::parse(MINIMAL).expect("parse minimal");

        assert_eq!(
            config.providers["ollama"],
            ProviderConfig::Ollama {
                base_url: DEFAULT_OLLAMA_BASE_URL.to_string(),
                keep_alive: None,
            }
        );
        assert_eq!(config.memory.core_budget_tokens, 500);
        assert_eq!(config.memory.episodic_top_k, 6);
        assert_eq!(config.memory.episodic_cap_tokens, 900);
        assert_eq!(config.memory.similarity_edge_threshold, 0.78);
        assert_eq!(config.spotlight, SpotlightConfig::default());

        let partial = format!("{MINIMAL}[memory]\nepisodic_top_k = 3\n");
        let config = Config::parse(&partial).expect("parse partial memory section");
        assert_eq!(config.memory.episodic_top_k, 3);
        assert_eq!(config.memory.core_budget_tokens, 500);
    }

    #[test]
    fn unknown_keys_are_rejected_by_name() {
        let cases = [
            (format!("smell = \"fishy\"\n{MINIMAL}"), "smell"),
            (
                format!("{MINIMAL}[providers.other]\nkind = \"ollama\"\napi_key_env = \"NOPE\"\n"),
                "api_key_env",
            ),
            (format!("{MINIMAL}[memory]\nbudget = 5\n"), "budget"),
            (
                format!("{MINIMAL}[spotlight]\nshortcut = \"F1\"\n"),
                "shortcut",
            ),
        ];
        for (text, key) in cases {
            let err = Config::parse(&text)
                .expect_err("unknown key must be rejected")
                .to_string();
            assert!(err.contains(key), "{err}");
        }
    }

    #[test]
    fn invalid_values_name_their_key() {
        let cases = [
            (
                "default_provider = \"zen\"\n[providers.ollama]\nkind = \"ollama\"\n".to_string(),
                "default_provider",
            ),
            (
                format!("{MINIMAL}[spotlight]\nprovider = \"zen\"\n"),
                "spotlight.provider",
            ),
            (
                "default_provider = \"zen\"\n[providers.zen]\nkind = \"openai_compat\"\nbase_url = \"\"\n".to_string(),
                "providers.zen.base_url",
            ),
            (
                "default_provider = \"zen\"\n[providers.zen]\nkind = \"openai_compat\"\nbase_url = \"https://example.test\"\napi_key_env = \"not a var\"\n".to_string(),
                "providers.zen.api_key_env",
            ),
            (
                format!("{MINIMAL}[memory]\nepisodic_top_k = 0\n"),
                "memory.episodic_top_k",
            ),
            (
                format!("{MINIMAL}[memory]\nsimilarity_edge_threshold = 1.5\n"),
                "memory.similarity_edge_threshold",
            ),
            (
                format!("{MINIMAL}[memory]\nsimilarity_edge_threshold = 0.0\n"),
                "memory.similarity_edge_threshold",
            ),
        ];
        for (text, key) in cases {
            let err = Config::parse(&text)
                .expect_err("invalid value must be rejected")
                .to_string();
            assert!(err.contains(key), "{err}");
        }
    }

    #[test]
    fn a_missing_file_is_created_from_the_template() {
        let _env = crate::lock_env();
        let dir = TempDir::new("config");
        let path = dir.config();
        let previous = std::env::var_os(CONFIG_PATH_ENV);
        std::env::set_var(CONFIG_PATH_ENV, &path);

        let created = Config::load();
        let reloaded = Config::load();

        match previous {
            Some(value) => std::env::set_var(CONFIG_PATH_ENV, value),
            None => std::env::remove_var(CONFIG_PATH_ENV),
        }

        let created = created.expect("create and parse the template");
        assert_eq!(created, reloaded.expect("reload the written file"));
        assert_eq!(created.default_provider, "ollama");
        assert_eq!(created.memory, MemoryConfig::default());
        assert_eq!(created.spotlight, SpotlightConfig::default());

        let written = std::fs::read_to_string(&path).expect("template was written");
        assert_eq!(written, DEFAULT_CONFIG);
        assert!(!written.contains("sk-"), "{written}");
        for line in written.lines().filter(|line| line.contains("api_key")) {
            assert!(line.trim_start().starts_with('#'), "{line}");
        }
    }

    #[test]
    fn default_model_is_declared_only_by_openai_compat_providers() {
        let config = Config::parse(SAMPLE).expect("parse sample");

        assert_eq!(config.default_model("deepseek"), Some("deepseek-chat"));
        assert_eq!(config.default_model("zen"), None);
        assert_eq!(config.default_model("ollama"), None);
        assert_eq!(config.default_model("nope"), None);
    }

    #[test]
    fn registry_exposes_the_configured_providers() {
        let config = Config::parse(SAMPLE).expect("parse sample");
        let registry = ProviderRegistry::from_config(&config).expect("build registry");

        assert_eq!(registry.default_provider_name(), "deepseek");
        assert_eq!(registry.names(), vec!["deepseek", "ollama", "zen"]);
        assert_eq!(registry.kind("zen"), Some("openai_compat"));
        assert_eq!(registry.kind("ollama"), Some("ollama"));
        assert_eq!(registry.kind("nope"), None);

        // A registry built from a config whose keys are unset still serves the
        // providers that need none.
        assert!(registry.provider("ollama").is_ok());
        let err = provider_error(&registry, "nope");
        assert!(err.contains("nope"), "{err}");
    }

    #[test]
    fn openai_compat_keys_are_read_from_the_environment_on_demand() {
        let _env = crate::lock_env();
        const VAR: &str = "ODYN_TEST_PROVIDER_TOKEN";
        let config = Config::parse(&format!(
            "default_provider = \"keyed\"\n\
             [providers.keyed]\n\
             kind = \"openai_compat\"\n\
             base_url = \"https://example.test/v1\"\n\
             api_key_env = \"{VAR}\"\n\
             [providers.keyless]\n\
             kind = \"openai_compat\"\n\
             base_url = \"https://example.test/local\"\n"
        ))
        .expect("parse");
        let registry = ProviderRegistry::from_config(&config).expect("build registry");

        assert!(registry.provider("keyless").is_ok());

        std::env::remove_var(VAR);
        let unset = provider_error(&registry, "keyed");
        std::env::set_var(VAR, "");
        let empty = provider_error(&registry, "keyed");
        std::env::set_var(VAR, "test-token-value");
        let built = registry.provider("keyed").is_ok();
        std::env::remove_var(VAR);

        for message in [unset, empty] {
            assert!(message.contains("providers.keyed.api_key_env"), "{message}");
            assert!(message.contains(VAR), "{message}");
        }
        assert!(built, "a set key must build the provider");
    }

    #[tokio::test]
    async fn built_providers_use_their_configured_base_url() {
        // Bind then drop, so nothing is listening on a port we know is free.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("address");
        drop(listener);

        let config = Config::parse(&format!(
            "default_provider = \"ollama\"\n\
             [providers.ollama]\n\
             kind = \"ollama\"\n\
             base_url = \"http://{addr}\"\n"
        ))
        .expect("parse");
        let registry = ProviderRegistry::from_config(&config).expect("build registry");
        let provider = registry.provider("ollama").expect("build ollama provider");

        let messages = vec![Message::new(Role::User, "hi")];
        let err = provider
            .chat_collect(ChatRequest::new(&messages, "llama3.2:3b"))
            .await
            .expect_err("nothing is listening");
        match err {
            ChatError::Network(message) => assert!(
                message.contains(&format!("ollama not reachable at http://{addr}")),
                "{message}"
            ),
            other => panic!("expected a network error, got {other:?}"),
        }
    }
    #[test]
    fn brevity_levels_parse_and_bad_ones_name_the_key() {
        let config = Config::parse(
            "default_provider = \"x\"\n[providers.x]\nkind = \"ollama\"\n\
             [style]\nbrevity = \"ultra\"\n[spotlight]\nbrevity = \"lite\"\n",
        )
        .expect("parse");
        assert_eq!(config.style.brevity, crate::brevity::Brevity::Ultra);
        assert_eq!(config.spotlight.brevity, crate::brevity::Brevity::Lite);

        // Absent sections fall back: off for chat, full for spotlight.
        let config = Config::parse("default_provider = \"x\"\n[providers.x]\nkind = \"ollama\"\n")
            .expect("parse defaults");
        assert_eq!(config.style.brevity, crate::brevity::Brevity::Off);
        assert_eq!(config.spotlight.brevity, crate::brevity::Brevity::Full);

        let error = Config::parse(
            "default_provider = \"x\"\n[providers.x]\nkind = \"ollama\"\n\
             [style]\nbrevity = \"caveman\"\n",
        )
        .expect_err("a made-up level must fail");
        let message = error.to_string();
        assert!(message.contains("brevity"), "{message}");
        for level in ["off", "lite", "full", "ultra"] {
            assert!(message.contains(level), "{message} must offer {level}");
        }
    }
}
