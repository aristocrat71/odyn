//! The brain's source of truth: a folder of markdown files.
//!
//! One memory is one `.md` note, its file stem the slug, `[[wikilinks]]` its
//! edges; YAML frontmatter is tolerated but never embedded. The folder is the
//! truth — anything in the SQLite index that disagrees is the index's bug.

use std::path::{Path, PathBuf};

const BRAIN_DIR_NAME: &str = "brain";
/// Model-trashed notes live here; a subfolder is invisible to `is_note`.
const TRASH_DIR_NAME: &str = ".trash";
/// A derived slug stays short enough to read as an id in the ledger.
const SLUG_MAX_CHARS: usize = 48;
/// The line `link_note` keeps a note's added links on.
const SEE_ALSO: &str = "See also ";

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
    #[error("`{0}` cannot link to itself")]
    SelfLink(String),
    #[error("the note has no content")]
    EmptyNote,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteFile {
    /// The file stem, case preserved.
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

/// The configured path (`~` expanded), or `brain/` in the platform data dir.
pub fn brain_dir(configured: Option<&Path>) -> Result<PathBuf, NotesError> {
    if let Some(path) = configured {
        return Ok(expand_home(path));
    }
    let dirs = directories::ProjectDirs::from("", "", "odyn").ok_or(NotesError::NoDataDir)?;
    Ok(dirs.data_dir().join(BRAIN_DIR_NAME))
}

/// Every note in the folder, sorted by slug. A missing folder is an empty brain,
/// not an error; a file empty once frontmatter is stripped is not a memory.
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

/// The slugs in the folder, sorted. A missing folder is an empty brain.
pub fn list_slugs(dir: &Path) -> Result<Vec<String>, NotesError> {
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
    let mut slugs = Vec::new();
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
        if let Some(slug) = path.file_stem().and_then(|stem| stem.to_str()) {
            slugs.push(slug.to_string());
        }
    }
    slugs.sort();
    Ok(slugs)
}

/// Writes a new note, deriving a slug from the content unless one is given. A
/// derived slug dodges collisions with a numeric suffix; an explicit name that
/// already exists is an error — overwriting is `update_note`'s job.
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

/// Replaces a note's content, keeping its slug and so its index id, hit history
/// and edges by name.
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

/// Writes a `[[to]]` wikilink into `from`, leaving the rest of the note — and
/// its frontmatter — byte for byte. Idempotent: `Ok(false)` means the link was
/// already there. Both notes must exist, so a slug a small model invented comes
/// back as an error it can read rather than a dangling edge.
pub fn link_note(dir: &Path, from: &str, to: &str) -> Result<bool, NotesError> {
    if from.eq_ignore_ascii_case(to) {
        return Err(NotesError::SelfLink(from.to_string()));
    }
    let path = note_path(dir, from);
    if !path.exists() {
        return Err(NotesError::NotFound(from.to_string()));
    }
    if !note_path(dir, to).exists() {
        return Err(NotesError::NotFound(to.to_string()));
    }
    let raw = std::fs::read_to_string(&path).map_err(|source| NotesError::Read {
        path: path.clone(),
        source,
    })?;
    if parse_links(strip_frontmatter(&raw)).contains(&to.to_lowercase()) {
        return Ok(false);
    }
    let body = raw.trim_end();
    let link = format!("[[{to}]]");
    // A note collects its links on one trailing line instead of growing a
    // paragraph per link — these files are read by hand, not only by Odyn.
    let text = match body.rsplit_once('\n') {
        Some((head, last)) if last.starts_with(SEE_ALSO) => {
            format!("{head}\n{}, {link}.\n", last.trim_end_matches('.'))
        }
        _ if body.starts_with(SEE_ALSO) => format!("{}, {link}.\n", body.trim_end_matches('.')),
        _ => format!("{body}\n\n{SEE_ALSO}{link}.\n"),
    };
    std::fs::write(&path, text).map_err(|source| NotesError::Write { path, source })?;
    Ok(true)
}

/// Removes the `[[to]]` edge from `from`, keeping the note readable. On the
/// `See also` line the entry goes, and the line with it once nothing is left;
/// anywhere else the brackets are unwrapped so the sentence still reads.
/// Idempotent: `Ok(false)` means there was no such link. `to` need not exist —
/// a link left dangling by a delete is exactly the kind worth clearing.
pub fn unlink_note(dir: &Path, from: &str, to: &str) -> Result<bool, NotesError> {
    let path = note_path(dir, from);
    if !path.exists() {
        return Err(NotesError::NotFound(from.to_string()));
    }
    let raw = std::fs::read_to_string(&path).map_err(|source| NotesError::Read {
        path: path.clone(),
        source,
    })?;
    let target = link_target(to);
    if !parse_links(strip_frontmatter(&raw)).contains(&target) {
        return Ok(false);
    }
    // Frontmatter belongs to other tools; only the memory below it is edited.
    let (head, body) = raw.split_at(raw.len() - strip_frontmatter(&raw).len());
    let kept: Vec<String> = body
        .lines()
        .filter_map(|line| unlinked_line(line, &target))
        .collect();
    let body = kept.join("\n");
    let body = body.trim();
    if body.is_empty() {
        return Err(NotesError::EmptyNote);
    }
    std::fs::write(&path, format!("{head}{body}\n"))
        .map_err(|source| NotesError::Write { path, source })?;
    Ok(true)
}

/// `None` drops the line: it was Odyn's `See also` line and held nothing else.
fn unlinked_line(line: &str, target: &str) -> Option<String> {
    let Some(links) = see_also_links(line) else {
        return Some(unwrap_links(line, target));
    };
    let kept: Vec<&str> = links
        .into_iter()
        .filter(|inner| link_target(inner) != target)
        .collect();
    if kept.is_empty() {
        return None;
    }
    let names: Vec<String> = kept.iter().map(|inner| format!("[[{inner}]]")).collect();
    Some(format!("{SEE_ALSO}{}.", names.join(", ")))
}

/// The insides of `See also [[a]], [[b]].` and nothing else — a line a human
/// wrote around its links is prose, and is never rebuilt.
fn see_also_links(line: &str) -> Option<Vec<&str>> {
    let inner = line.strip_prefix(SEE_ALSO)?.strip_suffix('.')?;
    inner
        .split(", ")
        .map(|part| part.strip_prefix("[[")?.strip_suffix("]]"))
        .collect()
}

/// `[[anna]]` becomes `anna` and `[[anna|Anna]]` becomes `Anna`: the edge goes,
/// the sentence stays.
fn unwrap_links(line: &str, target: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = rest.find("[[") {
        let Some(end) = rest[start + 2..].find("]]") else {
            break;
        };
        let inner = &rest[start + 2..start + 2 + end];
        out.push_str(&rest[..start]);
        if link_target(inner) == target {
            out.push_str(display_text(inner));
        } else {
            out.push_str(&rest[start..start + 2 + end + 2]);
        }
        rest = &rest[start + 2 + end + 2..];
    }
    out.push_str(rest);
    out
}

pub fn delete_note(dir: &Path, slug: &str) -> Result<(), NotesError> {
    let path = note_path(dir, slug);
    std::fs::remove_file(&path).map_err(|source| match source.kind() {
        std::io::ErrorKind::NotFound => NotesError::NotFound(slug.to_string()),
        _ => NotesError::Write { path, source },
    })
}

/// Model-driven deletion: the note moves to `.trash/` in the brain folder
/// rather than vanishing, so a wrong slug from a small model stays
/// recoverable. Human paths (`mem rm`, the brain view) delete outright.
pub fn trash_note(dir: &Path, slug: &str) -> Result<(), NotesError> {
    let path = note_path(dir, slug);
    if !path.exists() {
        return Err(NotesError::NotFound(slug.to_string()));
    }
    let trash = dir.join(TRASH_DIR_NAME);
    std::fs::create_dir_all(&trash).map_err(|source| NotesError::Create {
        path: trash.clone(),
        source,
    })?;
    // A re-trashed slug replaces the older copy: the latest deletion wins.
    std::fs::rename(&path, trash.join(format!("{slug}.md")))
        .map_err(|source| NotesError::Write { path, source })
}

pub fn note_path(dir: &Path, slug: &str) -> PathBuf {
    dir.join(format!("{slug}.md"))
}

fn write(dir: &Path, slug: &str, content: &str) -> Result<(), NotesError> {
    let path = note_path(dir, slug);
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

/// YAML frontmatter (a leading `---` line closed by another) is metadata for
/// other tools; the memory is what follows.
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
        let target = link_target(inner);
        if !target.is_empty() && !links.contains(&target) {
            links.push(target);
        }
    }
    links
}

/// What a `[[wikilink]]`'s insides resolve to: the file stem, lowercased.
fn link_target(inner: &str) -> String {
    inner
        .split('|')
        .next()
        .unwrap_or_default()
        .split('#')
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase()
}

/// What a `[[wikilink]]` reads as on the page: its alias, or its target.
fn display_text(inner: &str) -> &str {
    match inner.split_once('|') {
        Some((_, alias)) => alias.trim(),
        None => inner.split('#').next().unwrap_or_default().trim(),
    }
}

/// FNV-1a, 64-bit: deterministic across runs and toolchains, as a stored
/// change-detection hash must be.
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

/// The first line's words, kebab-cased and capped: a readable id, not a title.
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
    fn linking_appends_one_see_also_line_and_repeats_collect_on_it() {
        let dir = TempDir::new("link");
        write_note(&dir.0, Some("mitul"), "---\ntype: person\n---\nThe user.").expect("write");
        write_note(&dir.0, Some("football"), "He plays on Sundays.").expect("write");
        write_note(&dir.0, Some("anna"), "His sister.").expect("write");

        assert!(link_note(&dir.0, "football", "mitul").expect("link"));
        assert_eq!(
            std::fs::read_to_string(note_path(&dir.0, "football")).expect("read"),
            "He plays on Sundays.\n\nSee also [[mitul]].\n"
        );
        assert!(link_note(&dir.0, "football", "anna").expect("second link"));
        assert_eq!(
            std::fs::read_to_string(note_path(&dir.0, "football")).expect("read"),
            "He plays on Sundays.\n\nSee also [[mitul]], [[anna]].\n"
        );
        assert_eq!(
            read_notes(&dir.0).expect("read")[1].links,
            vec!["mitul".to_string(), "anna".to_string()]
        );

        // Idempotent, whatever the case the model names the target in.
        assert!(!link_note(&dir.0, "football", "Mitul").expect("again"));

        // Frontmatter survives a link written into the note below it.
        assert!(link_note(&dir.0, "mitul", "football").expect("link back"));
        assert_eq!(
            std::fs::read_to_string(note_path(&dir.0, "mitul")).expect("read"),
            "---\ntype: person\n---\nThe user.\n\nSee also [[football]].\n"
        );

        assert!(matches!(
            link_note(&dir.0, "football", "ghost"),
            Err(NotesError::NotFound(slug)) if slug == "ghost"
        ));
        assert!(matches!(
            link_note(&dir.0, "ghost", "mitul"),
            Err(NotesError::NotFound(slug)) if slug == "ghost"
        ));
        assert!(matches!(
            link_note(&dir.0, "mitul", "mitul"),
            Err(NotesError::SelfLink(_))
        ));
    }

    #[test]
    fn unlinking_clears_the_see_also_entry_and_unwraps_links_in_prose() {
        let dir = TempDir::new("unlink");
        write_note(&dir.0, Some("mitul"), "The user.").expect("write");
        write_note(&dir.0, Some("anna"), "His sister.").expect("write");
        write_note(&dir.0, Some("football"), "He plays on Sundays.").expect("write");
        link_note(&dir.0, "football", "mitul").expect("link");
        link_note(&dir.0, "football", "anna").expect("link");

        assert!(unlink_note(&dir.0, "football", "mitul").expect("unlink"));
        assert_eq!(
            std::fs::read_to_string(note_path(&dir.0, "football")).expect("read"),
            "He plays on Sundays.\n\nSee also [[anna]].\n"
        );
        // The last entry takes the line with it, blank separator included.
        assert!(unlink_note(&dir.0, "football", "ANNA").expect("unlink"));
        assert_eq!(
            std::fs::read_to_string(note_path(&dir.0, "football")).expect("read"),
            "He plays on Sundays.\n"
        );
        assert!(!unlink_note(&dir.0, "football", "anna").expect("again"));
        assert!(matches!(
            unlink_note(&dir.0, "ghost", "anna"),
            Err(NotesError::NotFound(slug)) if slug == "ghost"
        ));
    }

    #[test]
    fn unlinking_in_prose_keeps_the_sentence_and_the_other_links() {
        let dir = TempDir::new("unwrap");
        std::fs::write(
            dir.0.join("keys.md"),
            "---\ntype: note\n---\n[[Anna|Anna]] left the [[car-keys#spare]] with [[mitul]].\n\
             \nShe told [[anna]] twice.\n",
        )
        .expect("write");

        assert!(unlink_note(&dir.0, "keys", "anna").expect("unlink"));
        assert_eq!(
            std::fs::read_to_string(note_path(&dir.0, "keys")).expect("read"),
            "---\ntype: note\n---\nAnna left the [[car-keys#spare]] with [[mitul]].\n\
             \nShe told anna twice.\n"
        );
        assert_eq!(
            read_notes(&dir.0).expect("read")[0].links,
            vec!["car-keys".to_string(), "mitul".to_string()]
        );

        // A hand-written `See also` line is prose, so it is unwrapped, not rebuilt.
        std::fs::write(
            dir.0.join("spare.md"),
            "See also [[anna]] — she has the spare.\n",
        )
        .expect("write");
        assert!(unlink_note(&dir.0, "spare", "anna").expect("unlink"));
        assert_eq!(
            std::fs::read_to_string(note_path(&dir.0, "spare")).expect("read"),
            "See also anna — she has the spare.\n"
        );
    }

    /// Unlinking the whole of a note would leave a file that is no longer a
    /// memory; it errors instead, and the model reads why.
    #[test]
    fn unlinking_cannot_empty_a_note() {
        let dir = TempDir::new("unlink-empty");
        std::fs::write(dir.0.join("stub.md"), "See also [[anna]].\n").expect("write");
        assert!(matches!(
            unlink_note(&dir.0, "stub", "anna"),
            Err(NotesError::EmptyNote)
        ));
        assert_eq!(
            std::fs::read_to_string(note_path(&dir.0, "stub")).expect("read"),
            "See also [[anna]].\n"
        );
    }

    #[test]
    fn trash_moves_the_note_out_of_the_brain_but_keeps_the_file() {
        let dir = TempDir::new("trash");
        let slug = write_note(&dir.0, Some("car-keys"), "on the desk").expect("write");
        trash_note(&dir.0, &slug).expect("trash");
        assert!(read_notes(&dir.0).expect("read").is_empty());
        let kept = dir.0.join(TRASH_DIR_NAME).join("car-keys.md");
        assert_eq!(
            std::fs::read_to_string(&kept).expect("kept"),
            "on the desk\n"
        );
        // A recreated then re-trashed slug replaces the older copy.
        write_note(&dir.0, Some("car-keys"), "on the fridge").expect("rewrite");
        trash_note(&dir.0, "car-keys").expect("retrash");
        assert_eq!(
            std::fs::read_to_string(&kept).expect("kept"),
            "on the fridge\n"
        );
        assert!(matches!(
            trash_note(&dir.0, "ghost"),
            Err(NotesError::NotFound(_))
        ));
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
