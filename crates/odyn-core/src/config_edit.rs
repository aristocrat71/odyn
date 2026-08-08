//! Reading and rewriting `odyn.toml`: single keys, and whole provider tables.
//!
//! Every write goes through `toml_edit`, so comments, blank lines and spacing
//! the user left behind survive an edit to some other key.

use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Table, TableLike, Value};

use crate::config::{invalid, read_or_create, Config, ConfigError, ProviderConfig};

/// The value at a dotted key: strings raw, everything else as TOML.
pub fn get(path: &Path, key: &str) -> Result<String, ConfigError> {
    let doc = parse(&read_or_create(path)?)?;
    let mut item = doc.as_item();
    for segment in key.split('.') {
        item = item
            .as_table_like()
            .and_then(|table| table.get(segment))
            .ok_or_else(|| ConfigError::UnknownKey(key.to_string()))?;
    }
    Ok(render(key, item))
}

/// The edited document is validated whole before it reaches disk, so a rejected
/// value leaves the file exactly as it was.
pub fn set(path: &Path, key: &str, value: &str) -> Result<(), ConfigError> {
    let mut doc = parse(&read_or_create(path)?)?;
    assign(&mut doc, key, value)?;
    let edited = doc.to_string();
    Config::parse(&edited)?;
    write_atomically(path, &edited)
}

/// Writes `[providers.{name}]` wholesale, new or replacing. That table becomes
/// machine-shaped; the rest of the file keeps its hand.
pub fn upsert_provider(
    path: &Path,
    name: &str,
    provider: &ProviderConfig,
) -> Result<(), ConfigError> {
    check_name(name)?;
    let mut doc = parse(&read_or_create(path)?)?;
    let providers = descend(doc.as_table_mut(), &["providers"], "providers")?;
    providers.insert(name, Item::Table(provider_table(provider)));
    let edited = doc.to_string();
    Config::parse(&edited)?;
    write_atomically(path, &edited)
}

/// Validation runs on the document with the table gone, so removing the
/// default provider is rejected and the file left untouched.
pub fn remove_provider(path: &Path, name: &str) -> Result<(), ConfigError> {
    let mut doc = parse(&read_or_create(path)?)?;
    doc.get_mut("providers")
        .and_then(Item::as_table_like_mut)
        .and_then(|providers| providers.remove(name))
        .ok_or_else(|| ConfigError::UnknownProvider(name.to_string()))?;
    let edited = doc.to_string();
    Config::parse(&edited)?;
    write_atomically(path, &edited)
}

fn provider_table(provider: &ProviderConfig) -> Table {
    let mut table = Table::new();
    table.insert("kind", toml_edit::value(provider.kind()));
    match provider {
        ProviderConfig::OpenAiCompat {
            base_url,
            api_key,
            api_key_env,
            default_model,
        } => {
            table.insert("base_url", toml_edit::value(base_url));
            for (key, value) in [
                ("api_key", api_key),
                ("api_key_env", api_key_env),
                ("default_model", default_model),
            ] {
                if let Some(value) = value {
                    table.insert(key, toml_edit::value(value));
                }
            }
        }
        ProviderConfig::Ollama {
            base_url,
            keep_alive,
        } => {
            table.insert("base_url", toml_edit::value(base_url));
            if let Some(keep_alive) = keep_alive {
                table.insert("keep_alive", toml_edit::value(keep_alive));
            }
        }
    }
    table
}

/// Names stay addressable by the CLI's dotted keys, so no dots and no spaces.
fn check_name(name: &str) -> Result<(), ConfigError> {
    let plain = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if plain {
        Ok(())
    } else {
        Err(invalid(
            format!("providers.{name}"),
            "provider names use letters, digits, `_` and `-`",
        ))
    }
}

fn parse(text: &str) -> Result<DocumentMut, ConfigError> {
    text.parse()
        .map_err(|err: toml_edit::TomlError| ConfigError::Parse(err.to_string()))
}

fn assign(doc: &mut DocumentMut, key: &str, raw: &str) -> Result<(), ConfigError> {
    let mut segments: Vec<&str> = key.split('.').collect();
    let leaf = match segments.pop() {
        Some(leaf) => leaf,
        None => return Err(ConfigError::UnknownKey(key.to_string())),
    };
    let table = descend(doc.as_table_mut(), &segments, key)?;
    match table.get_mut(leaf) {
        Some(Item::Value(old)) => {
            // The line keeps its own shape: same spacing, same trailing comment.
            let mut new = scalar(raw);
            *new.decor_mut() = old.decor().clone();
            *old = new;
        }
        Some(_) => return Err(invalid(key, "is a table, not a value")),
        None => {
            table.insert(leaf, Item::Value(scalar(raw)));
        }
    }
    Ok(())
}

fn descend<'a>(
    table: &'a mut dyn TableLike,
    segments: &[&str],
    key: &str,
) -> Result<&'a mut dyn TableLike, ConfigError> {
    let Some((head, rest)) = segments.split_first() else {
        return Ok(table);
    };
    let entry = table.entry(head).or_insert_with(|| {
        let mut created = Table::new();
        created.set_implicit(true);
        Item::Table(created)
    });
    let next = entry
        .as_table_like_mut()
        .ok_or_else(|| invalid(key, &format!("`{head}` is a value, not a table")))?;
    descend(next, rest, key)
}

/// A value that is already a TOML scalar is taken as written, so `set … 6`
/// stores a number and `set … '"0"'` a string. Everything else is a string.
fn scalar(raw: &str) -> Value {
    match raw.parse::<Value>() {
        Ok(
            value @ (Value::String(_) | Value::Integer(_) | Value::Float(_) | Value::Boolean(_)),
        ) => value,
        _ => Value::from(raw),
    }
}

fn render(key: &str, item: &Item) -> String {
    match item {
        Item::Value(Value::String(text)) => text.value().clone(),
        Item::Value(value) => value.clone().decorated("", "").to_string(),
        // A table is reprinted under its full header, so it can be pasted
        // straight back into the file.
        _ => {
            let mut segments: Vec<&str> = key.split('.').collect();
            let mut nested = item.clone();
            if let Item::Table(table) = &mut nested {
                // Whatever preceded the header in the file is not this key.
                table.decor_mut().set_prefix("");
            }
            let mut doc = DocumentMut::new();
            while let Some(name) = segments.pop() {
                if segments.is_empty() {
                    doc.insert(name, nested);
                    break;
                }
                let mut parent = Table::new();
                parent.set_implicit(true);
                parent.insert(name, nested);
                nested = Item::Table(parent);
            }
            doc.to_string().trim().to_string()
        }
    }
}

/// Temp file plus rename: an interrupted write can never leave a truncated
/// config behind.
fn write_atomically(path: &Path, text: &str) -> Result<(), ConfigError> {
    let path = &resolve(path);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let failed = |path: &Path| {
        let path = path.to_path_buf();
        move |source| ConfigError::Write { path, source }
    };
    std::fs::write(&temp, text).map_err(failed(&temp))?;
    std::fs::rename(&temp, path).map_err(|source| {
        let _ = std::fs::remove_file(&temp);
        failed(path)(source)
    })
}

/// The file a path really names, links followed: renaming onto a symlink would
/// replace the link with a regular file, and a config in a dotfiles directory
/// would quietly stop being the one Odyn reads.
fn resolve(path: &Path) -> PathBuf {
    if let Ok(target) = std::fs::canonicalize(path) {
        return target;
    }
    // A dangling link still says where its file belongs, relative to the link.
    let target = match std::fs::read_link(path) {
        Ok(link) => path.parent().unwrap_or(Path::new("")).join(link),
        Err(_) => path.to_path_buf(),
    };
    match (
        target.parent().map(std::fs::canonicalize),
        target.file_name(),
    ) {
        (Some(Ok(parent)), Some(name)) => parent.join(name),
        _ => target,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Hand-written on purpose: comments on a header and a value, and spacing
    /// nobody would generate.
    const FIXTURE: &str = r#"# odyn, hand-edited.

default_provider   =    "ollama"

[providers.ollama]  # local only
kind = "ollama"
base_url = "http://localhost:11434"

# the brain's share of the prompt
[memory]
episodic_top_k = 6      # keep it small
similarity_edge_threshold = 0.78
"#;

    const EDITED: &str = r#"# odyn, hand-edited.

default_provider   =    "ollama"

[providers.ollama]  # local only
kind = "ollama"
base_url = "http://localhost:11434"
keep_alive = "10m"

# the brain's share of the prompt
[memory]
episodic_top_k = 4      # keep it small
similarity_edge_threshold = 0.5
"#;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("odyn-edit-{}-{label}-{unique}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            let path = dir.join("odyn.toml");
            std::fs::write(&path, FIXTURE).expect("write fixture");
            Self(path)
        }

        fn text(&self) -> String {
            std::fs::read_to_string(&self.0).expect("read config")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Some(dir) = self.0.parent() {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }

    #[test]
    fn editing_a_key_leaves_the_rest_of_the_file_byte_for_byte() {
        let fixture = Fixture::new("roundtrip");

        set(&fixture.0, "memory.episodic_top_k", "4").expect("set an existing integer");
        set(&fixture.0, "memory.similarity_edge_threshold", "0.5").expect("set an existing float");
        set(&fixture.0, "providers.ollama.keep_alive", "10m").expect("set a new key");

        assert_eq!(fixture.text(), EDITED);
        for (key, value) in [
            ("memory.episodic_top_k", "4"),
            ("memory.similarity_edge_threshold", "0.5"),
            ("providers.ollama.keep_alive", "10m"),
        ] {
            assert_eq!(get(&fixture.0, key).expect("get back"), value);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_config_is_written_through_to_its_target() {
        let real = Fixture::new("dotfiles");
        let linked = Fixture::new("through-a-link");
        link(&real.0, &linked);

        set(&linked.0, "brain.top_k", "4").expect("set through the link");

        assert!(is_symlink(&linked.0), "the link was replaced by a file");
        assert!(real.text().contains("top_k = 4"), "{}", real.text());
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_is_written_through_to_where_it_points() {
        let real = Fixture::new("dotfiles-empty");
        let linked = Fixture::new("through-a-dangling-link");
        std::fs::remove_file(&real.0).expect("remove the target");
        link(&real.0, &linked);

        set(&linked.0, "brain.top_k", "4").expect("set through the link");

        assert!(is_symlink(&linked.0), "the link was replaced by a file");
        assert!(real.text().contains("top_k = 4"), "{}", real.text());
    }

    #[cfg(unix)]
    fn link(target: &Path, at: &Fixture) {
        std::fs::remove_file(&at.0).expect("clear the link's place");
        std::os::unix::fs::symlink(target, &at.0).expect("link the config");
    }

    #[cfg(unix)]
    fn is_symlink(path: &Path) -> bool {
        std::fs::symlink_metadata(path)
            .expect("stat the link")
            .file_type()
            .is_symlink()
    }

    #[test]
    fn a_value_that_fails_validation_is_never_written() {
        let fixture = Fixture::new("rejected");
        let cases = [("default_provider", "zen"), ("brain.top_k", "0")];
        for (key, value) in cases {
            let err = set(&fixture.0, key, value)
                .expect_err("validation must reject this")
                .to_string();
            assert!(err.contains(key), "{err}");
            assert_eq!(fixture.text(), FIXTURE);
        }
    }

    /// The escape hatch for values that would otherwise read as numbers, such as
    /// ollama's `keep_alive = "0"`.
    #[test]
    fn a_quoted_value_is_stored_as_a_string() {
        let fixture = Fixture::new("quoted");
        set(&fixture.0, "providers.ollama.keep_alive", "\"0\"").expect("set a quoted string");
        assert_eq!(
            get(&fixture.0, "providers.ollama.keep_alive").expect("get"),
            "0"
        );
    }

    #[test]
    fn a_missing_key_is_named_in_the_error() {
        let fixture = Fixture::new("missing");
        for key in ["nope", "memory.nope", "providers.ollama.nope"] {
            let err = get(&fixture.0, key)
                .expect_err("missing key must fail")
                .to_string();
            assert!(err.contains(key), "{err}");
        }
    }

    #[test]
    fn scalars_print_raw_and_tables_print_as_toml() {
        let fixture = Fixture::new("shapes");
        let value = |key| get(&fixture.0, key).expect("get");

        assert_eq!(value("default_provider"), "ollama");
        assert_eq!(value("memory.episodic_top_k"), "6");
        assert_eq!(value("memory.similarity_edge_threshold"), "0.78");
        assert_eq!(
            value("memory"),
            "[memory]\nepisodic_top_k = 6      # keep it small\nsimilarity_edge_threshold = 0.78"
        );
        assert_eq!(
            value("providers.ollama"),
            "[providers.ollama]  # local only\nkind = \"ollama\"\nbase_url = \"http://localhost:11434\""
        );
    }

    #[test]
    fn a_missing_file_is_created_from_the_template() {
        let fixture = Fixture::new("template");
        std::fs::remove_file(&fixture.0).expect("remove the fixture");

        assert_eq!(get(&fixture.0, "default_provider").expect("get"), "ollama");
        assert!(fixture.text().starts_with("# Odyn configuration."));
    }

    #[test]
    fn an_upserted_provider_lands_beside_the_hand_written_ones() {
        let fixture = Fixture::new("upsert");
        let zen = ProviderConfig::OpenAiCompat {
            base_url: "https://opencode.ai/zen/v1".to_string(),
            api_key: Some("sk-test".to_string()),
            api_key_env: None,
            default_model: Some("kimi-k3".to_string()),
        };

        upsert_provider(&fixture.0, "zen", &zen).expect("add zen");

        let text = fixture.text();
        assert!(text.starts_with("# odyn, hand-edited."), "{text}");
        assert!(text.contains("[providers.ollama]  # local only"), "{text}");
        assert_eq!(
            get(&fixture.0, "providers.zen.api_key").expect("get"),
            "sk-test"
        );
        assert_eq!(
            Config::load_from(&fixture.0).expect("parse").providers["zen"],
            zen
        );

        // Replacing rewrites the whole table: dropped fields stay dropped.
        let keyless = ProviderConfig::OpenAiCompat {
            base_url: "https://opencode.ai/zen/v1".to_string(),
            api_key: None,
            api_key_env: Some("OPENCODE_API_KEY".to_string()),
            default_model: None,
        };
        upsert_provider(&fixture.0, "zen", &keyless).expect("replace zen");
        assert_eq!(
            Config::load_from(&fixture.0).expect("parse").providers["zen"],
            keyless
        );
        assert!(!fixture.text().contains("sk-test"), "{}", fixture.text());
    }

    #[test]
    fn removing_a_provider_spares_the_default_and_the_unknown() {
        let fixture = Fixture::new("remove");
        let extra = ProviderConfig::Ollama {
            base_url: "http://localhost:11435".to_string(),
            keep_alive: None,
        };
        upsert_provider(&fixture.0, "spare", &extra).expect("add a second provider");

        remove_provider(&fixture.0, "spare").expect("remove it again");
        assert!(!fixture.text().contains("spare"), "{}", fixture.text());

        let err = remove_provider(&fixture.0, "ollama")
            .expect_err("the default provider must survive")
            .to_string();
        assert!(err.contains("default_provider"), "{err}");
        assert!(fixture.text().contains("[providers.ollama]"));

        let err = remove_provider(&fixture.0, "nope")
            .expect_err("an unknown name must fail")
            .to_string();
        assert!(err.contains("nope"), "{err}");
    }

    #[test]
    fn a_provider_name_the_cli_cannot_address_is_rejected() {
        let fixture = Fixture::new("bad-name");
        let provider = ProviderConfig::Ollama {
            base_url: "http://localhost:11434".to_string(),
            keep_alive: None,
        };
        for name in ["", "with space", "with.dot", "wíth-áccents"] {
            let err = upsert_provider(&fixture.0, name, &provider)
                .expect_err("bad names must be rejected")
                .to_string();
            assert!(err.contains("letters, digits"), "{err}");
            assert_eq!(fixture.text(), FIXTURE);
        }
    }
}
