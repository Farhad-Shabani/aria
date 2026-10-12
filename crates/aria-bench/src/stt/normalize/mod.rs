//! English text normalization, ported from Whisper's `EnglishTextNormalizer`.
//!
//! Matches the reference normalizer so scoring differences do not come from
//! different treatment of spelling, punctuation, or numbers.

mod lexicon;
mod numbers;
mod spelling;
mod symbols;

use std::sync::LazyLock;

use regex::Regex;

/// Symbols a number can be written with, kept until the number pass has read
/// them.
const NUMERIC_SYMBOLS: &str = ".%$¢€£";

/// Reduces text to the words a transcript and its reference can be compared
/// on: lowercase, unpunctuated, contractions expanded, numbers as digits.
fn normalize(text: &str) -> String {
    let text = text.to_lowercase();
    let text = ANNOTATIONS.strip(&text);
    let text = CONTRACTIONS.apply(&text);
    let text = strip_thousands_separators(&text);
    let text = symbols::strip(&text, NUMERIC_SYMBOLS);

    let text = numbers::standardize(&text);
    let text = spelling::americanize(&text);
    let text = release_lone_symbols(&text);
    collapse_whitespace(&text)
}

/// The words a transcript is scored on.
pub fn words(text: &str) -> Vec<String> {
    normalize(text)
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// Text a reader never spoke: bracketed annotations, and the filler sounds a
/// transcriber may or may not have written down.
struct Annotations {
    bracketed: Regex,
    parenthesized: Regex,
    fillers: Regex,
    space_before_apostrophe: Regex,
}

static ANNOTATIONS: LazyLock<Annotations> = LazyLock::new(|| Annotations {
    bracketed: pattern(r"[<\[][^>\]]*[>\]]"),
    parenthesized: pattern(r"\(([^)]+?)\)"),
    fillers: pattern(r"\b(hmm|mm|mhm|mmm|uh|um)\b"),
    space_before_apostrophe: pattern(r"\s+'"),
});

impl Annotations {
    fn strip(&self, text: &str) -> String {
        let text = self.bracketed.replace_all(text, "");
        let text = self.parenthesized.replace_all(&text, "");
        let text = self.fillers.replace_all(&text, "");
        self.space_before_apostrophe
            .replace_all(&text, "'")
            .into_owned()
    }
}

/// Contractions, abbreviated titles, and clipped tenses written out in full,
/// applied in order because the longer forms must win over the shorter ones:
/// `'d been` is `had been`, not `would been`.
struct Contractions {
    rules: Vec<(Regex, &'static str)>,
}

static CONTRACTIONS: LazyLock<Contractions> = LazyLock::new(|| Contractions {
    rules: EXPANSIONS
        .iter()
        .map(|&(contracted, expanded)| (pattern(contracted), expanded))
        .collect(),
});

impl Contractions {
    fn apply(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for (contracted, expanded) in &self.rules {
            text = contracted.replace_all(&text, *expanded).into_owned();
        }
        text
    }
}

const EXPANSIONS: &[(&str, &str)] = &[
    (r"\bwon't\b", "will not"),
    (r"\bcan't\b", "can not"),
    (r"\blet's\b", "let us"),
    (r"\bain't\b", "aint"),
    (r"\by'all\b", "you all"),
    (r"\bwanna\b", "want to"),
    (r"\bgotta\b", "got to"),
    (r"\bgonna\b", "going to"),
    (r"\bi'ma\b", "i am going to"),
    (r"\bimma\b", "i am going to"),
    (r"\bwoulda\b", "would have"),
    (r"\bcoulda\b", "could have"),
    (r"\bshoulda\b", "should have"),
    (r"\bma'am\b", "madam"),
    (r"\bmr\b", "mister "),
    (r"\bmrs\b", "missus "),
    (r"\bst\b", "saint "),
    (r"\bdr\b", "doctor "),
    (r"\bprof\b", "professor "),
    (r"\bcapt\b", "captain "),
    (r"\bgov\b", "governor "),
    (r"\bald\b", "alderman "),
    (r"\bgen\b", "general "),
    (r"\bsen\b", "senator "),
    (r"\brep\b", "representative "),
    (r"\bpres\b", "president "),
    (r"\brev\b", "reverend "),
    (r"\bhon\b", "honorable "),
    (r"\basst\b", "assistant "),
    (r"\bassoc\b", "associate "),
    (r"\blt\b", "lieutenant "),
    (r"\bcol\b", "colonel "),
    (r"\bjr\b", "junior "),
    (r"\bsr\b", "senior "),
    (r"\besq\b", "esquire "),
    (r"'d been\b", " had been"),
    (r"'s been\b", " has been"),
    (r"'d gone\b", " had gone"),
    (r"'s gone\b", " has gone"),
    (r"'d done\b", " had done"),
    (r"'s got\b", " has got"),
    (r"n't\b", " not"),
    (r"'re\b", " are"),
    (r"'s\b", " is"),
    (r"'d\b", " would"),
    (r"'ll\b", " will"),
    (r"'t\b", " not"),
    (r"'ve\b", " have"),
    (r"'m\b", " am"),
];

static THOUSANDS_SEPARATOR: LazyLock<Regex> = LazyLock::new(|| pattern(r"(\d),(\d)"));
static SENTENCE_PERIOD: LazyLock<Regex> = LazyLock::new(|| pattern(r"\.([^0-9]|$)"));

/// Commas inside a number join its digits; a period that is not a decimal
/// point ends a sentence and becomes a space.
fn strip_thousands_separators(text: &str) -> String {
    let joined = THOUSANDS_SEPARATOR.replace_all(text, "$1$2");
    SENTENCE_PERIOD.replace_all(&joined, " $1").into_owned()
}

static LONE_PREFIX: LazyLock<Regex> = LazyLock::new(|| pattern(r"[.$¢€£]([^0-9])"));
static LONE_PERCENT: LazyLock<Regex> = LazyLock::new(|| pattern(r"([^0-9])%"));

/// Numeric symbols that ended up next to no number. They were kept only for
/// the number pass, which is finished by now.
fn release_lone_symbols(text: &str) -> String {
    let released = LONE_PREFIX.replace_all(text, " $1");
    LONE_PERCENT.replace_all(&released, "$1 ").into_owned()
}

static WHITESPACE: LazyLock<Regex> = LazyLock::new(|| pattern(r"\s+"));

fn collapse_whitespace(text: &str) -> String {
    WHITESPACE.replace_all(text, " ").into_owned()
}

fn pattern(source: &str) -> Regex {
    Regex::new(source).expect("the normalizer's patterns should compile")
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    /// Cases and their expected output, produced by running Whisper's
    /// `EnglishTextNormalizer` over the inputs. Hand-picked cases cover each
    /// branch by name. The rest are sampled from ten thousand random word
    /// sequences run through both normalizers.
    const NORMALIZER_CASES: &str = include_str!("normalizer-parity.jsonl");

    #[derive(Deserialize)]
    struct NormalizerCase {
        input: String,
        expected: String,
    }

    #[test]
    fn case_and_punctuation_do_not_survive() {
        assert_eq!(words("The socket, closes."), words("THE SOCKET CLOSES"));
    }

    #[test]
    fn spoken_and_written_numbers_agree() {
        assert_eq!(words("twenty two"), words("22"));
        assert_eq!(words("nineteen sixty"), words("1960"));
    }

    #[test]
    fn fillers_and_annotations_are_dropped() {
        assert_eq!(words("um the [laughter] socket"), words("the socket"));
    }

    #[test]
    fn a_letter_spelled_two_ways_reads_the_same() {
        let composed = "se\u{f1}or";
        let decomposed = "sen\u{303}or";

        assert_eq!(words(decomposed), words(composed));
        assert_eq!(words(composed), vec!["senor".to_owned()]);
    }

    #[test]
    fn british_and_american_spellings_agree() {
        assert_eq!(words("the colour"), words("the color"));
    }

    /// A rate measured with a normalizer that disagrees with Whisper's is not
    /// comparable to a published one, so a difference here is a defect even
    /// when the Rust behavior looks more sensible.
    #[test]
    fn every_case_matches_the_reference_normalizer() {
        let mut differences = Vec::new();
        for case in NORMALIZER_CASES.lines() {
            let case: NormalizerCase = serde_json::from_str(case).expect("the case should parse");
            let normalized = normalize(&case.input);
            if normalized != case.expected {
                differences.push(format!(
                    "  {:?}\n    reference {:?}\n    ours      {:?}",
                    case.input, case.expected, normalized
                ));
            }
        }
        assert!(
            differences.is_empty(),
            "{} of {} cases differ from the reference normalizer:\n{}",
            differences.len(),
            NORMALIZER_CASES.lines().count(),
            differences.join("\n")
        );
    }
}
