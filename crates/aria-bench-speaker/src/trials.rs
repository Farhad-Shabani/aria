//! The trial list: which recordings are one voice, and which are two.
//!
//! One trial per line, whitespace separated:
//!
//! ```text
//! 1 ada/a.wav,ada/b.wav,ada/c.wav  ada/d.wav
//! 0 ada/a.wav,ada/b.wav,ada/c.wav  zoe/a.wav
//! 1 ada/a.wav                      ada/d.wav
//! ```
//!
//! The label is 1 when both sides are the same person. The enrollment side
//! is comma separated, so one path measures the model alone and several
//! measure what `aria speaker enroll` actually builds. Blank lines and `#`
//! comments are skipped. Relative paths resolve against the list's own
//! directory, so a corpus moves without editing.
//!
//! VoxCeleb's published trial lists are this format with a single enrollment
//! path, which makes their numbers directly comparable.

use std::path::{Path, PathBuf};

use super::VerifyError;

/// One question: is the voice in `test` the one `enrollment` recorded?
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trial {
    pub same_speaker: bool,
    pub enrollment: Vec<PathBuf>,
    pub test: PathBuf,
}

pub fn read_trials(path: &Path) -> Result<Vec<Trial>, VerifyError> {
    let text = std::fs::read_to_string(path).map_err(|source| VerifyError::TrialList {
        path: path.to_path_buf(),
        source,
    })?;
    let root = path.parent().unwrap_or(Path::new("."));
    parse(&text, root, path)
}

fn parse(text: &str, root: &Path, path: &Path) -> Result<Vec<Trial>, VerifyError> {
    let mut trials = Vec::new();
    for (offset, text) in text.lines().enumerate() {
        let line = offset + 1;
        let text = text.trim();
        if text.is_empty() || text.starts_with('#') {
            continue;
        }
        let mut fields = text.split_whitespace();
        let (Some(label), Some(enrollment), Some(test), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(VerifyError::Trial {
                path: path.to_path_buf(),
                line,
            });
        };
        let same_speaker = match label {
            "1" => true,
            "0" => false,
            _ => {
                return Err(VerifyError::Label {
                    path: path.to_path_buf(),
                    line,
                });
            }
        };
        trials.push(Trial {
            same_speaker,
            enrollment: enrollment.split(',').map(|side| root.join(side)).collect(),
            test: root.join(test),
        });
    }
    Ok(trials)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> Result<Vec<Trial>, VerifyError> {
        parse(text, Path::new("/corpus"), Path::new("/corpus/trials.txt"))
    }

    #[test]
    fn a_single_path_enrollment_reads_as_one_sample() {
        let trials = parsed("1 ada/a.wav ada/b.wav").expect("the line parses");

        assert_eq!(
            trials,
            vec![Trial {
                same_speaker: true,
                enrollment: vec![PathBuf::from("/corpus/ada/a.wav")],
                test: PathBuf::from("/corpus/ada/b.wav"),
            }]
        );
    }

    #[test]
    fn a_comma_separated_enrollment_reads_as_several() {
        let trials = parsed("0 ada/a.wav,ada/b.wav zoe/a.wav").expect("the line parses");

        assert!(!trials[0].same_speaker);
        assert_eq!(trials[0].enrollment.len(), 2);
        assert_eq!(trials[0].test, PathBuf::from("/corpus/zoe/a.wav"));
    }

    #[test]
    fn blank_lines_and_comments_are_not_trials() {
        let trials = parsed("# a corpus\n\n1 ada/a.wav ada/b.wav\n\n").expect("the list parses");

        assert_eq!(trials.len(), 1);
    }

    #[test]
    fn an_absolute_path_is_left_alone() {
        let trials = parsed("1 /elsewhere/a.wav ada/b.wav").expect("the line parses");

        assert_eq!(trials[0].enrollment[0], PathBuf::from("/elsewhere/a.wav"));
    }

    #[test]
    fn a_line_that_is_not_a_trial_names_its_line() {
        assert!(matches!(
            parsed("1 ada/a.wav"),
            Err(VerifyError::Trial { line: 1, .. })
        ));
        assert!(matches!(
            parsed("1 a.wav b.wav c.wav"),
            Err(VerifyError::Trial { line: 1, .. })
        ));
    }

    #[test]
    fn a_label_that_is_not_one_or_zero_is_refused() {
        assert!(matches!(
            parsed("\n2 ada/a.wav ada/b.wav"),
            Err(VerifyError::Label { line: 2, .. })
        ));
    }
}
