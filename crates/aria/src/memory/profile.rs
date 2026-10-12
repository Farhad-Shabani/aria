//! The profile block: every active fact rendered once to a file, so an ask
//! pays one small read to show the agent what it knows.

use std::io::Write;
use std::path::Path;

use aria_kit::primitives::private_fs;

use super::PROFILE_FILE;
use super::store::{Fact, KINDS, Store, StoreError};

const HEADERS: [(&str, &str); 3] = [
    ("identity", "Identity"),
    ("preference", "Preferences"),
    ("correction", "Corrections the user has made"),
];

/// Markdown grouped by kind, empty when there are no facts. The hedge in
/// the header is what lets the agent ignore a memory that does not apply;
/// over-personalization is how memory features go wrong.
pub(crate) fn render_profile(facts: &[Fact]) -> String {
    if facts.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "## About the user (from local memory)".to_owned(),
        "Facts the user stated in past sessions. Use when relevant; ignore freely if not."
            .to_owned(),
    ];
    let mut unknown: Vec<&str> = facts
        .iter()
        .map(|fact| fact.kind.as_str())
        .filter(|kind| !KINDS.contains(kind))
        .collect();
    unknown.sort_unstable();
    unknown.dedup();
    for kind in KINDS.iter().copied().chain(unknown) {
        let texts: Vec<&str> = facts
            .iter()
            .filter(|fact| fact.kind == kind)
            .map(|fact| fact.text.as_str())
            .collect();
        if texts.is_empty() {
            continue;
        }
        lines.push(format!("\n### {}", header(kind)));
        lines.extend(texts.into_iter().map(|text| format!("- {text}")));
    }
    lines.join("\n")
}

fn header(kind: &str) -> String {
    if let Some((_, header)) = HEADERS.iter().find(|(known, _)| *known == kind) {
        return (*header).to_owned();
    }
    kind.split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        })
        .collect::<Vec<String>>()
        .join(" ")
}

/// Rewrites the block from the active facts; removes it when none are
/// left, so a stale block never outlives its facts.
pub fn write_profile(store: &Store, dir: &Path) -> Result<(), StoreError> {
    let block = render_profile(&store.active_facts()?);
    let path = dir.join(PROFILE_FILE);
    if block.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            result => result.map_err(StoreError::Profile),
        };
    }
    private_fs::prepare_directory(dir).map_err(StoreError::Directory)?;
    let mut file = tempfile::NamedTempFile::new_in(dir).map_err(StoreError::Profile)?;
    file.write_all(block.as_bytes())
        .map_err(StoreError::Profile)?;
    file.persist(&path)
        .map_err(|error| StoreError::Profile(error.error))?;
    Ok(())
}

/// The block as written, or `None` when there is none or it cannot be
/// read: memory never fails an ask.
pub(crate) fn load_profile_block(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(PROFILE_FILE)).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::store::STATUS_ACTIVE;

    fn fact(kind: &str, text: &str) -> Fact {
        Fact {
            id: 0,
            text: text.to_owned(),
            kind: kind.to_owned(),
            status: STATUS_ACTIVE.to_owned(),
            confidence: 0.9,
            source_quote: String::new(),
            source_session: String::new(),
            created_at: String::new(),
        }
    }

    #[test]
    fn facts_group_by_kind_in_a_fixed_order() {
        let facts = [
            fact("preference", "Likes tea"),
            fact("identity", "Is Sam"),
            fact("preference", "Hates meetings"),
        ];
        assert_eq!(
            render_profile(&facts),
            "## About the user (from local memory)\n\
             Facts the user stated in past sessions. Use when relevant; ignore freely if not.\n\
             \n### Identity\n- Is Sam\n\
             \n### Preferences\n- Likes tea\n- Hates meetings"
        );
    }

    #[test]
    fn no_facts_render_nothing() {
        assert_eq!(render_profile(&[]), "");
    }

    #[test]
    fn an_unknown_kind_is_kept_under_its_own_header() {
        let facts = [fact("work_style", "Prefers async")];
        assert!(render_profile(&facts).contains("### Work Style\n- Prefers async"));
    }

    #[test]
    fn the_block_is_written_and_removed_with_its_facts() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Store::open(&dir.path().join("memory.db")).expect("opens");
        assert_eq!(load_profile_block(dir.path()), None);

        let id = store
            .add_fact("Is Sam", "identity", STATUS_ACTIVE, 0.9, "", "")
            .expect("adds");
        write_profile(&store, dir.path()).expect("writes");
        let block = load_profile_block(dir.path()).expect("block");
        assert!(block.ends_with("### Identity\n- Is Sam"));

        store.forget(id).expect("forgets");
        write_profile(&store, dir.path()).expect("writes");
        assert!(!dir.path().join(PROFILE_FILE).exists());
        assert_eq!(load_profile_block(dir.path()), None);
    }
}
