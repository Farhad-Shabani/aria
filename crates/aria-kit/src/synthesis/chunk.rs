//! Splitting speech text into sentence-sized chunks.
//!
//! Streaming wants small chunks — the first sentence plays while the second
//! synthesizes — and models want short ones: past their trained context the
//! output degrades. Sentences are the natural seam for both.

const SENTENCE_ENDERS: [char; 4] = ['.', '!', '?', '…'];

/// Characters per chunk, kept well under the model's token context since one
/// character phonemizes to at most a few tokens.
const MAX_CHUNK_CHARS: usize = 300;

/// The text in model-sized pieces, sentence boundaries preserved.
pub fn chunks(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for sentence in sentences(text) {
        // One for the space that joins them.
        if !current.is_empty()
            && current.chars().count() + 1 + sentence.chars().count() > MAX_CHUNK_CHARS
        {
            chunks.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(sentence);
        // One overlong sentence still has to split somewhere; mid-word beats
        // truncation.
        while let Some((cap, _)) = current.char_indices().nth(MAX_CHUNK_CHARS) {
            let split = split_point(&current, cap);
            let rest = current.split_off(split);
            chunks.push(std::mem::replace(
                &mut current,
                rest.trim_start().to_owned(),
            ));
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn sentences(text: &str) -> impl Iterator<Item = &str> {
    text.split_inclusive(SENTENCE_ENDERS)
        .map(str::trim)
        .filter(|sentence| !sentence.is_empty())
}

/// The last space before the byte index `cap`, or `cap` itself on one
/// unbroken run.
fn split_point(text: &str, cap: usize) -> usize {
    match text[..cap].rfind(' ') {
        Some(space) if space > 0 => space,
        _no_space => cap,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_stays_one_chunk() {
        assert_eq!(chunks("Hello there."), vec!["Hello there."]);
    }

    #[test]
    fn sentences_group_up_to_the_cap() {
        let text = "One. Two. Three.";
        assert_eq!(chunks(text), vec!["One. Two. Three."]);

        let sentence = format!("{}.", "word ".repeat(45).trim());
        let long = format!("{sentence} {sentence}");
        let split = chunks(&long);
        assert_eq!(split.len(), 2, "two long sentences split at their seam");
        assert_eq!(split[1], sentence);
    }

    #[test]
    fn an_unbroken_sentence_still_splits() {
        let long = "word ".repeat(120);
        for chunk in chunks(&long) {
            assert!(chunk.len() <= MAX_CHUNK_CHARS);
            assert!(!chunk.is_empty());
        }
    }

    #[test]
    fn multibyte_text_splits_on_character_boundaries() {
        let long = "wörd ".repeat(120);
        for chunk in chunks(&long) {
            assert!(chunk.chars().count() <= MAX_CHUNK_CHARS);
        }
    }

    #[test]
    fn the_cap_counts_characters_not_bytes() {
        for character in ["é", "語"] {
            let full = character.repeat(MAX_CHUNK_CHARS);
            assert_eq!(chunks(&full), vec![full.clone()]);
            let over = format!("{full}{character}");
            assert_eq!(chunks(&over), vec![full, character.to_owned()]);
        }

        let long = format!("{}.", "word ".repeat(40).trim_end());
        let short = format!("yyyy{}.", "ab cd ".repeat(16).trim_end());
        let pair = format!("{long} {short}");
        assert_eq!(
            long.chars().count() + short.chars().count(),
            MAX_CHUNK_CHARS
        );
        assert_eq!(chunks(&pair), vec![long, short], "the joining space counts");

        let offset = format!("a{}", "語".repeat(MAX_CHUNK_CHARS));
        let split = chunks(&offset);
        assert_eq!(split[0].chars().count(), MAX_CHUNK_CHARS);
        assert_eq!(split[1], "語");
    }
}
