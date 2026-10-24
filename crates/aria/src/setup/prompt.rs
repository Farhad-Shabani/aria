//! Questions on stdin, answers as plain text: numbered lists and y/n. In
//! tests a `Cursor` plays the terminal.

use std::io::{BufRead, Write};

use super::SetupError;
use super::backends::{Availability, Choice};

pub(super) struct Prompts<'a, R, W> {
    input: &'a mut R,
    output: &'a mut W,
    yes: bool,
    /// Whether an answer can be asked for. A piped stdin reads EOF, which
    /// would take every default silently, model downloads included.
    interactive: bool,
    style: Style,
}

/// ANSI styling, or nothing when no terminal is watching.
struct Style {
    bold: &'static str,
    dim: &'static str,
    reset: &'static str,
}

impl Style {
    fn for_terminal(color: bool) -> Self {
        if color {
            Self {
                bold: "\x1b[1m",
                dim: "\x1b[2m",
                reset: "\x1b[0m",
            }
        } else {
            Self {
                bold: "",
                dim: "",
                reset: "",
            }
        }
    }
}

impl<'a, R: BufRead, W: Write> Prompts<'a, R, W> {
    /// `color` is about the stream being written to, which is not the one
    /// answers arrive on: `aria setup > log` still reads a terminal.
    pub(super) fn new(
        input: &'a mut R,
        output: &'a mut W,
        yes: bool,
        interactive: bool,
        color: bool,
    ) -> Self {
        Self {
            input,
            output,
            yes,
            interactive,
            style: Style::for_terminal(color),
        }
    }

    pub(super) fn heading(&mut self, text: &str) -> Result<(), SetupError> {
        let Style { bold, reset, .. } = self.style;
        writeln!(self.output, "\n{bold}{text}{reset}").map_err(SetupError::Prompt)
    }

    pub(super) fn note(&mut self, text: &str) -> Result<(), SetupError> {
        let Style { dim, reset, .. } = self.style;
        writeln!(self.output, "{dim}{text}{reset}").map_err(SetupError::Prompt)
    }

    pub(super) fn confirm(&mut self, question: &str, default: bool) -> Result<bool, SetupError> {
        if self.yes {
            return Ok(default);
        }
        let hint = if default { "Y/n" } else { "y/N" };
        let answer = self.ask(&format!("{question} [{hint}] "))?;
        Ok(match answer.as_str() {
            "" => default,
            answer => answer.eq_ignore_ascii_case("y"),
        })
    }

    /// A free-text answer. Empty means the question went unanswered — end of
    /// input reads the same way, so nothing here can wait forever.
    pub(super) fn line(&mut self, question: &str) -> Result<String, SetupError> {
        self.ask(&format!("{question} "))
    }

    /// One of `choices`. The default is the one marked so. An option this
    /// host cannot run at all is listed with the reason and refused; one
    /// setup cannot make work by itself is accepted with its fix echoed.
    pub(super) fn pick<'c>(
        &mut self,
        title: &str,
        choices: &'c [Choice],
    ) -> Result<&'c Choice, SetupError> {
        let default = choices
            .iter()
            .position(|choice| choice.default)
            .unwrap_or(0);
        if self.yes {
            return Ok(&choices[default]);
        }

        self.heading(title)?;
        for (index, choice) in choices.iter().enumerate() {
            let hint = match &choice.availability {
                Availability::Ready => String::new(),
                Availability::Missing { fix } => format!("  — {fix}"),
                Availability::Unsupported { why } => format!("  — {why}"),
            };
            let Style { dim, reset, .. } = self.style;
            writeln!(
                self.output,
                "  {}) {}{dim}{hint}{reset}",
                index + 1,
                choice.label
            )
            .map_err(SetupError::Prompt)?;
        }
        loop {
            let answer = self.ask(&format!("Choice [{}]: ", default + 1))?;
            let index = if answer.is_empty() {
                default
            } else {
                match answer.parse::<usize>() {
                    Ok(number) if (1..=choices.len()).contains(&number) => number - 1,
                    _ => {
                        self.note(&format!("enter a number from 1 to {}", choices.len()))?;
                        continue;
                    }
                }
            };
            let choice = &choices[index];
            match &choice.availability {
                Availability::Unsupported { why } => {
                    self.note(&format!("not available here: {why}"))?;
                }
                Availability::Missing { fix } => {
                    self.note(&format!("chosen; before it works, {fix}"))?;
                    return Ok(choice);
                }
                Availability::Ready => return Ok(choice),
            }
        }
    }

    /// One trimmed line of input; end of input reads as an empty answer.
    fn ask(&mut self, prompt: &str) -> Result<String, SetupError> {
        if !self.interactive {
            return Err(SetupError::NotATerminal);
        }
        write!(self.output, "{prompt}")
            .and_then(|()| self.output.flush())
            .map_err(SetupError::Prompt)?;
        let mut answer = String::new();
        self.input
            .read_line(&mut answer)
            .map_err(SetupError::Prompt)?;
        Ok(answer.trim().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::setup::backends::{Backend, Host, Stt, Tts, choices};

    fn prompts<'a>(
        input: &'a mut Cursor<String>,
        output: &'a mut Vec<u8>,
        yes: bool,
    ) -> Prompts<'a, Cursor<String>, Vec<u8>> {
        Prompts::new(input, output, yes, true, true)
    }

    fn answer(text: &str, default: bool, yes: bool) -> bool {
        let mut input = Cursor::new(text.to_owned());
        let mut output = Vec::new();
        prompts(&mut input, &mut output, yes)
            .confirm("Download?", default)
            .unwrap()
    }

    #[test]
    fn confirm_reads_the_answer_or_takes_the_default() {
        assert!(answer("\n", true, false));
        assert!(!answer("\n", false, false));
        assert!(answer("y\n", false, false));
        assert!(answer("Y\n", false, false));
        assert!(!answer("n\n", true, false));
        assert!(!answer("maybe\n", true, false));
        assert!(answer("", true, false), "end of input takes the default");
    }

    #[test]
    fn yes_skips_the_prompt() {
        assert!(answer("n\n", true, true));
        assert!(!answer("y\n", false, true));
    }

    #[test]
    fn a_pipe_refuses_to_answer_for_the_user() {
        let mut input = Cursor::new(String::new());
        let mut output = Vec::new();
        let mut prompts = Prompts::new(&mut input, &mut output, false, false, false);

        let error = prompts.confirm("Download?", true).unwrap_err();

        assert!(matches!(error, SetupError::NotATerminal), "{error}");
    }

    #[test]
    fn a_line_comes_back_trimmed_and_an_empty_one_stays_empty() {
        let mut input = Cursor::new("  Ada Lovelace  \n".to_owned());
        let mut output = Vec::new();
        assert_eq!(
            prompts(&mut input, &mut output, false)
                .line("Name?")
                .unwrap(),
            "Ada Lovelace"
        );

        let mut input = Cursor::new("\n".to_owned());
        let mut output = Vec::new();
        assert!(
            prompts(&mut input, &mut output, false)
                .line("Name?")
                .unwrap()
                .is_empty()
        );
    }

    /// A Mac with a system voice.
    fn bare_host() -> Host {
        Host {
            macos: true,
            system_voice: Availability::Ready,
        }
    }

    fn pick<B: Backend>(text: &str, current: Option<&str>, yes: bool) -> (&'static str, String) {
        let mut input = Cursor::new(text.to_owned());
        let mut output = Vec::new();
        let choices = choices::<B>(current, &bare_host());
        let picked = prompts(&mut input, &mut output, yes)
            .pick(B::TITLE, &choices)
            .unwrap()
            .key;
        (picked, String::from_utf8(output).unwrap())
    }

    #[test]
    fn pick_takes_a_number_or_the_default() {
        assert_eq!(pick::<Tts>("2\n", None, false).0, "kokoro");
        assert_eq!(pick::<Tts>("\n", None, false).0, "system");
        assert_eq!(pick::<Tts>("\n", Some("kokoro"), false).0, "kokoro");
    }

    /// A missing prerequisite is a fix to print, not a reason to switch.
    #[test]
    fn pick_keeps_a_configured_choice_that_is_missing() {
        let (picked, shown) = pick::<Stt>("\n", Some("parakeet-mlx"), false);

        assert_eq!(picked, "parakeet-mlx");
        assert!(shown.contains("Choice [6]"), "{shown}");
    }

    #[test]
    fn pick_falls_back_only_when_nothing_is_configured() {
        let (picked, shown) = pick::<Tts>("\n", Some("not-a-backend"), false);

        assert_eq!(picked, "system");
        assert!(shown.contains("Choice [1]"), "{shown}");
    }

    #[test]
    fn pick_asks_again_after_a_bad_number() {
        let (picked, shown) = pick::<Tts>("9\nx\n2\n", None, false);

        assert_eq!(picked, "kokoro");
        assert_eq!(shown.matches("Choice [").count(), 3, "{shown}");
    }

    #[test]
    fn pick_allows_a_backend_that_is_missing() {
        let (picked, shown) = pick::<Stt>("6\n", None, false);

        assert_eq!(picked, "parakeet-mlx");
        assert!(shown.contains("before it works"), "{shown}");
    }

    #[test]
    fn yes_picks_the_default_without_printing() {
        let (picked, shown) = pick::<Tts>("2\n", None, true);

        assert_eq!(picked, "system");
        assert!(shown.is_empty());
    }

    #[test]
    fn a_missing_system_voice_changes_only_an_unconfigured_default() {
        let host = Host {
            system_voice: Availability::Missing {
                fix: "install the system voice".to_owned(),
            },
            ..bare_host()
        };
        let default = |current| {
            choices::<Tts>(current, &host)
                .into_iter()
                .find(|choice| choice.default)
                .unwrap()
                .key
        };

        assert_eq!(default(None), "kokoro");
        assert_eq!(default(Some("system")), "system");
    }
}
