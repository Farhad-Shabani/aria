use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::ops::Range;

use aria_kit::cancel::CancelToken;

use super::{MAX_TEXT_BYTES, SourceKind, select};

pub(crate) struct TextInput<'a> {
    pub kind: SourceKind,
    pub text: &'a str,
}

pub(crate) trait Extractor: Send + 'static {
    type Error: std::error::Error + Send + Sync + 'static;

    fn cache_key(&self) -> &str;
    fn extract(
        &mut self,
        input: TextInput<'_>,
        limit: usize,
        cancel: &CancelToken,
    ) -> Result<Vec<Range<usize>>, Self::Error>;
}

#[derive(Default)]
pub(crate) struct LocalExtractor;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("vocabulary extraction was cancelled")]
    Cancelled,
    #[error("vocabulary extraction input exceeds 64 KiB")]
    TooLarge,
}

impl Extractor for LocalExtractor {
    type Error = Error;

    fn cache_key(&self) -> &str {
        "local-unicode-1"
    }

    fn extract(
        &mut self,
        input: TextInput<'_>,
        limit: usize,
        cancel: &CancelToken,
    ) -> Result<Vec<Range<usize>>, Error> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if input.text.len() > MAX_TEXT_BYTES {
            return Err(Error::TooLarge);
        }
        if limit == 0 {
            return Ok(Vec::new());
        }
        if input.kind != SourceKind::Text {
            return Ok(if select::eligible(input.kind, input.text) {
                std::iter::once(0..input.text.len()).collect()
            } else {
                Vec::new()
            });
        }
        let mut occurrences: HashMap<&str, (usize, usize)> = HashMap::new();
        let mut characters = input.text.char_indices().peekable();
        while let Some((start, character)) = characters.next() {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            if !character.is_alphanumeric() {
                continue;
            }
            let mut end = start + character.len_utf8();
            let mut suffix = false;
            while let Some(&(offset, character)) = characters.peek() {
                if cancel.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                if !suffix && character.is_alphanumeric() {
                    end = offset + character.len_utf8();
                    characters.next();
                } else if !suffix && matches!(character, '.' | '-' | '_') {
                    characters.next();
                    if characters
                        .peek()
                        .is_some_and(|(_, next)| next.is_alphanumeric())
                    {
                        end = offset + character.len_utf8();
                    } else {
                        break;
                    }
                } else if matches!(character, '+' | '#') {
                    suffix = true;
                    end = offset + character.len_utf8();
                    characters.next();
                } else {
                    break;
                }
            }
            let canonical = &input.text[start..end];
            if select::eligible(input.kind, canonical) {
                let occurrence = occurrences.entry(canonical).or_insert((0, start));
                occurrence.0 += 1;
            }
        }
        let mut ranked = BinaryHeap::new();
        for (canonical, (count, start)) in occurrences {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            ranked.push((count, Reverse(start), canonical.len()));
        }
        let mut spans = Vec::new();
        while spans.len() < limit {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let Some((_, Reverse(start), length)) = ranked.pop() else {
                break;
            };
            spans.push(start..start + length);
        }
        Ok(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(text: &str) -> Vec<String> {
        LocalExtractor
            .extract(
                TextInput {
                    kind: SourceKind::Text,
                    text,
                },
                128,
                &CancelToken::never(),
            )
            .unwrap()
            .into_iter()
            .map(|span| text[span].to_owned())
            .collect()
    }

    #[test]
    fn exact_unicode_spans_frequency_and_lexical_boundaries() {
        let text = "(écho-stt), URLSession vex16 Cargo.toml C++ C# écho-stt. foo..bar baz_";
        assert_eq!(
            extract(text),
            ["écho-stt", "URLSession", "vex16", "Cargo.toml", "C++", "C#"]
        );
        let spans = LocalExtractor
            .extract(
                TextInput {
                    kind: SourceKind::Text,
                    text: "雪9 雪9",
                },
                128,
                &CancelToken::never(),
            )
            .unwrap();
        assert_eq!(spans, vec![0..4]);
    }

    #[test]
    fn prose_cannot_starve_structured_candidates() {
        let mut text = String::new();
        for first in b'a'..=b'z' {
            for second in b'a'..=b'z' {
                text.push(first as char);
                text.push(second as char);
                text.push(' ');
            }
        }
        text.push_str("aria-kit");
        assert_eq!(extract(&text), ["aria-kit"]);
    }

    #[test]
    fn basenames_stay_whole_and_cancellation_is_observed() {
        let text = "ordinary report.pdf";
        assert_eq!(
            LocalExtractor
                .extract(
                    TextInput {
                        kind: SourceKind::FileName,
                        text
                    },
                    128,
                    &CancelToken::never()
                )
                .unwrap(),
            vec![0..text.len()]
        );
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(matches!(
            LocalExtractor.extract(
                TextInput {
                    kind: SourceKind::Text,
                    text
                },
                128,
                &cancel
            ),
            Err(Error::Cancelled)
        ));
        let checks = std::sync::atomic::AtomicUsize::new(0);
        let cancel = CancelToken::from_fn(move || {
            checks.fetch_add(1, std::sync::atomic::Ordering::Relaxed) > 500
        });
        assert!(matches!(
            LocalExtractor.extract(
                TextInput {
                    kind: SourceKind::Text,
                    text: &"x".repeat(8192)
                },
                128,
                &cancel
            ),
            Err(Error::Cancelled)
        ));
    }
}
