//! The brain's source of truth: a folder of markdown files.
//!
//! One memory is one `.md` note. The file's stem is its identity (its slug),
//! the text is the memory, `[[wikilinks]]` are deliberate edges in the brain
//! graph, and YAML frontmatter is tolerated but never embedded or injected.
//! SQLite holds only an index derived from these files — anything that
//! disagrees with the folder is the index's bug, never the folder's.

use std::path::{Path, PathBuf};

const BRAIN_DIR_NAME: &str = "brain";
/// A derived slug stays short enough to read as an id in the ledger.
const SLUG_MAX_CHARS: usize = 48;

#[derive(Debug, thiserror::Error)]
pub enum NotesError {
    #[error("could not read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not write {}: {source}", path.display())]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not create {}: {source}", path.display())]
    Create {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not locate a data directory for the brain folder")]
    NoDataDir,
    #[error("a note named `{0}` already exists")]
    Exists(String),
    #[error("no note named `{0}`")]
    NotFound(String),
    #[error("the note has no content")]
    EmptyNote,
}

/// One memory as its file holds it, ready for the index: frontmatter already
/// stripped, links already parsed, hash and token count already computed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteFile {
    /// The file stem, case preserved — what every surface shows.
    pub slug: String,
    /// The note without frontmatter, edge-trimmed. Never empty.
    pub content: String,
    /// `[[wikilink]]` targets, lowercased for resolution, in order, deduped.
    pub links: Vec<String>,
    /// FNV-1a of `content` — stable across runs, so the index only re-embeds
    /// what actually changed.
    pub hash: i64,
    /// chars/4 approximation of `content`.
    pub tokens: i64,
}

/// The brain folder: the configured path (`~` expanded), or `brain/` in the
/// platform data dir, next to the database.
pub fn brain_dir(configured: Option<&Path>) -> Result<PathBuf, NotesError> {
    if let Some(path) = configured {
        return Ok(expand_home(path));
    }
    let dirs = directories::ProjectDirs::from("", "", "odyn").ok_or(NotesError::NoDataDir)?;
    Ok(dirs.data_dir().join(BRAIN_DIR_NAME))
}

/// Every note in the folder, sorted by slug so callers see a stable order.
/// A folder that does not exist yet is an empty brain, not an error. Files
/// whose content is empty once frontmatter is stripped are not memories.
pub fn read_notes(dir: &Path) -> Result<Vec<NoteFile>, NotesError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(NotesError::Read {
                path: dir.to_path_buf(),
                source,
            })
        }
    };
    let mut notes = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|source| NotesError::Read {
                path: dir.to_path_buf(),
                source,
            })?
            .path();
        if !is_note(&path) {
            continue;
        }
        let Some(slug) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let raw = std::fs::read_to_string(&path).map_err(|source| NotesError::Read {
            path: path.clone(),
            source,
        })?;
        if let Some(note) = parse_note(slug, &raw) {
            notes.push(note);
        }
    }
    notes.sort_by(|a, b| a.slug.cmp(&b.slug));
    Ok(notes)
}

/// Writes a new note, deriving a slug from the content unless one is given.
/// A derived slug dodges collisions with a numeric suffix; an explicit name
/// that already exists is an error — overwriting is `update_note`'s job.
pub fn write_note(dir: &Path, name: Option<&str>, content: &str) -> Result<String, NotesError> {
    let content = content.trim();
    if content.is_empty() {
        return Err(NotesError::EmptyNote);
    }
    std::fs::create_dir_all(dir).map_err(|source| NotesError::Create {
        path: dir.to_path_buf(),
        source,
    })?;
    let slug = match name {
        Some(name) => {
            let slug = slugify(name);
            if slug.is_empty() {
                return Err(NotesError::EmptyNote);
            }
            if note_path(dir, &slug).exists() {
                return Err(NotesError::Exists(slug));
            }
            slug
        }
        None => free_slug(dir, &slugify_content(content)),
    };
    write(dir, &slug, content)?;
    Ok(slug)
}

/// Replaces a note's content, keeping its slug — and with it, its index id,
/// its hit history and its edges by name.
pub fn update_note(dir: &Path, slug: &str, content: &str) -> Result<(), NotesError> {
    let content = content.trim();
    if content.is_empty() {
        return Err(NotesError::EmptyNote);
    }
    if !note_path(dir, slug).exists() {
        return Err(NotesError::NotFound(slug.to_string()));
    }
    write(dir, slug, content)
}

pub fn delete_note(dir: &Path, slug: &str) -> Result<(), NotesError> {
    let path = note_path(dir, slug);
    std::fs::remove_file(&path).map_err(|source| match source.kind() {
        std::io::ErrorKind::NotFound => NotesError::NotFound(slug.to_string()),
        _ => NotesError::Write { path, source },
    })
}

pub fn note_path(dir: &Path, slug: &str) -> PathBuf {
    dir.join(format!("{slug}.md"))
}

fn write(dir: &Path, slug: &str, content: &str) -> Result<(), NotesError> {
    let path = note_path(dir, slug);
    // A trailing newline, as every well-behaved text tool leaves one.
    std::fs::write(&path, format!("{content}\n"))
        .map_err(|source| NotesError::Write { path, source })
}

fn is_note(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
}

fn parse_note(slug: &str, raw: &str) -> Option<NoteFile> {
    let content = strip_frontmatter(raw).trim();
    if content.is_empty() {
        return None;
    }
    Some(NoteFile {
        slug: slug.to_string(),
        links: parse_links(content),
        hash: fnv1a(content),
        tokens: approx_tokens(content),
        content: content.to_string(),
    })
}

/// YAML frontmatter — a leading `---` line closed by another — is metadata
/// for other tools; the memory is what follows.
fn strip_frontmatter(raw: &str) -> &str {
    let mut lines = raw.split_inclusive('\n');
    let Some(first) = lines.next() else {
        return raw;
    };
    if first.trim_end() != "---" {
        return raw;
    }
    let mut offset = first.len();
    for line in lines {
        offset += line.len();
        if line.trim_end() == "---" {
            return &raw[offset..];
        }
    }
    // An unclosed fence is not frontmatter, just a note starting with ---.
    raw
}

/// `[[target]]` and Obsidian's `[[target|alias]]` / `[[target#heading]]`,
/// lowercased for case-insensitive resolution, deduped in first-seen order.
fn parse_links(content: &str) -> Vec<String> {
    let mut links = Vec::new();
    let mut rest = content;
    while let Some(start) = rest.find("[[") {
        rest = &rest[start + 2..];
        let Some(end) = rest.find("]]") else {
            break;
        };
        let inner = &rest[..end];
        rest = &rest[end + 2..];
        let target = inner
            .split('|')
            .next()
            .unwrap_or_default()
            .split('#')
            .next()
            .unwrap_or_default()
            .trim()
            .to_lowercase();
        if !target.is_empty() && !links.contains(&target) {
            links.push(target);
        }
    }
    links
}

/// FNV-1a, 64-bit: deterministic across runs and toolchains forever, which is
/// exactly what a stored change-detection hash must be.
fn fnv1a(content: &str) -> i64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in content.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash as i64
}

pub(crate) fn approx_tokens(content: &str) -> i64 {
    content.chars().count().div_ceil(4) as i64
}

/// The first line's words, kebab-cased and capped — a readable id, not a title.
fn slugify_content(content: &str) -> String {
    let first_line = content.lines().next().unwrap_or_default();
    let slug = slugify(first_line);
    if slug.is_empty() {
        "note".to_string()
    } else {
        slug
    }
}

fn slugify(text: &str) -> String {
    let mut slug = String::new();
    let mut pending_dash = false;
    for ch in text.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !slug.is_empty() {
                if slug.chars().count() >= SLUG_MAX_CHARS {
                    break;
                }
                slug.push('-');
            }
            pending_dash = false;
            if slug.chars().count() >= SLUG_MAX_CHARS {
                break;
            }
            slug.push(ch);
        } else {
            pending_dash = true;
        }
    }
    slug
}

fn free_slug(dir: &Path, base: &str) -> String {
    if !note_path(dir, base).exists() {
        return base.to_string();
    }
    for suffix in 2.. {
        let candidate = format!("{base}-{suffix}");
        if !note_path(dir, &candidate).exists() {
            return candidate;
        }
    }
    unreachable!("the suffix loop is unbounded");
}

fn expand_home(path: &Path) -> PathBuf {
    let Some(rest) = path
        .to_str()
        .and_then(|path| path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")))
    else {
        return path.to_path_buf();
    };
    match std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        Some(home) => PathBuf::from(home).join(rest),
        None => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "odyn-notes-{}-{label}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_missing_folder_is_an_empty_brain() {
        let dir = TempDir::new("missing");
        let ghost = dir.0.join("never-created");
        assert_eq!(read_notes(&ghost).expect("read"), Vec::new());
    }

    #[test]
    fn notes_come_back_sorted_parsed_and_counted() {
        let dir = TempDir::new("read");
        std::fs::write(dir.0.join("zulu.md"), "last by name\n").expect("write");
        std::fs::write(
            dir.0.join("alpha.md"),
            "---\ntype: note\n---\nFirst by name, with a [[Zulu]] link.\n",
        )
        .expect("write");
        std::fs::write(dir.0.join("not-a-note.txt"), "ignored").expect("write");
        std::fs::write(dir.0.join("empty.md"), "---\na: b\n---\n   \n").expect("write");

        let notes = read_notes(&dir.0).expect("read");
        assert_eq!(notes.len(), 2, "txt and empty files are not memories");
        assert_eq!(notes[0].slug, "alpha");
        assert_eq!(notes[0].content, "First by name, with a [[Zulu]] link.");
        assert_eq!(notes[0].links, vec!["zulu".to_string()]);
        assert_eq!(notes[0].tokens, 9, "36 chars round up to 9");
        assert_eq!(notes[1].slug, "zulu");
        assert!(notes[1].links.is_empty());
        assert_ne!(notes[0].hash, notes[1].hash);
    }

    #[test]
    fn frontmatter_is_stripped_only_when_closed() {
        assert_eq!(strip_frontmatter("---\nkey: v\n---\nbody").trim(), "body");
        assert_eq!(
            strip_frontmatter("---\nnever closed\nbody"),
            "---\nnever closed\nbody"
        );
        assert_eq!(strip_frontmatter("no fence\n---\n"), "no fence\n---\n");
    }

    #[test]
    fn links_parse_aliases_headings_and_dedupe() {
        assert_eq!(
            parse_links("[[One]] then [[two|shown]] and [[Three#part]], [[one]] again, [[ ]]"),
            vec!["one".to_string(), "two".to_string(), "three".to_string()]
        );
        assert!(parse_links("no links [[unclosed").is_empty());
    }

    #[test]
    fn the_hash_is_stable_and_content_sensitive() {
        assert_eq!(fnv1a("same"), fnv1a("same"));
        assert_ne!(fnv1a("same"), fnv1a("Same"));
    }

    #[test]
    fn writing_derives_slugs_and_dodges_collisions() {
        let dir = TempDir::new("write");
        let slug = write_note(&dir.0, None, "Mitul prefers rustls, always.").expect("write");
        assert_eq!(slug, "mitul-prefers-rustls-always");
        let again = write_note(&dir.0, None, "Mitul prefers rustls, always?").expect("write");
        assert_eq!(again, "mitul-prefers-rustls-always-2");
        assert_eq!(
            std::fs::read_to_string(note_path(&dir.0, &slug)).expect("read back"),
            "Mitul prefers rustls, always.\n"
        );

        let named = write_note(&dir.0, Some("Coffee Order"), "flat white").expect("write named");
        assert_eq!(named, "coffee-order");
        assert!(matches!(
            write_note(&dir.0, Some("coffee-order"), "espresso"),
            Err(NotesError::Exists(slug)) if slug == "coffee-order"
        ));
        assert!(matches!(
            write_note(&dir.0, None, "   \n  "),
            Err(NotesError::EmptyNote)
        ));
    }

    #[test]
    fn update_replaces_and_delete_removes() {
        let dir = TempDir::new("update");
        let slug = write_note(&dir.0, None, "old text").expect("write");
        update_note(&dir.0, &slug, "new text").expect("update");
        assert_eq!(
            std::fs::read_to_string(note_path(&dir.0, &slug)).expect("read"),
            "new text\n"
        );
        assert!(matches!(
            update_note(&dir.0, "ghost", "text"),
            Err(NotesError::NotFound(_))
        ));
        delete_note(&dir.0, &slug).expect("delete");
        assert!(matches!(
            delete_note(&dir.0, &slug),
            Err(NotesError::NotFound(_))
        ));
        assert!(read_notes(&dir.0).expect("read").is_empty());
    }

    #[test]
    fn slugs_cap_their_length_at_a_word_boundary() {
        let slug =
            slugify("a very long first line that keeps going well past any sensible id length");
        assert!(slug.chars().count() <= SLUG_MAX_CHARS, "{slug}");
        assert!(!slug.ends_with('-'), "{slug}");
    }

    #[test]
    fn home_expansion_only_touches_a_leading_tilde() {
        let _env = crate::lock_env();
        std::env::set_var("HOME", "/home/test");
        assert_eq!(
            expand_home(Path::new("~/brain")),
            PathBuf::from("/home/test/brain")
        );
        assert_eq!(
            expand_home(Path::new("/abs/~path")),
            PathBuf::from("/abs/~path")
        );
    }
}
