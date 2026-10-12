use aria_stt::{prelude::*, whisper::Whisper};

const USAGE: &str =
    "usage: transcribe [--english-hesitations] MODEL_FILE AUDIO.wav [VOCABULARY_TERM ...]";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1).peekable();
    let english_hesitations = arguments
        .next_if(|argument| argument == "--english-hesitations")
        .is_some();
    let model = arguments.next().ok_or(USAGE)?;
    let audio = arguments.next().ok_or(USAGE)?;
    let terms = arguments
        .map(|term| {
            term.into_string()
                .map_err(|_| "vocabulary terms must be UTF-8")
        })
        .collect::<Result<Vec<_>, _>>()?;
    let vocabulary = Vocabulary::new(terms)?;
    let engine = Whisper::load(model)?;
    let transcription = Transcription::streaming(engine)
        .with(english_hesitations.then_some(RemoveHesitations))
        .with(vocabulary);

    println!("{}", transcription.transcribe_file(audio)?.as_str());
    Ok(())
}
