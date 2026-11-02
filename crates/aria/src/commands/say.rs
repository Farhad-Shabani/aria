use crate::synthesis::{SpeakingRate, SpeechText, Voice};
use anyhow::Context;
use aria_daemon::{DaemonSession, SpeechOutcome, SpeechRequest};
use aria_platform::runtime::PreparedRuntimePaths;

use super::logging::init_daemon_logging;

pub(super) fn run(
    text: Option<String>,
    voice: Option<String>,
    rate: Option<f32>,
    stop: bool,
) -> anyhow::Result<()> {
    let paths = PreparedRuntimePaths::for_current_user()
        .context("failed to prepare daemon runtime paths")?;
    init_daemon_logging(paths.log_path())?;
    let session = DaemonSession::open_at(&paths)?;

    if stop {
        let was_speaking = session.stop_speaking()?;
        if !was_speaking {
            println!("nothing was playing");
        }
        return Ok(());
    }

    let text = match text {
        Some(text) => text,
        None => {
            let mut text = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
                .context("failed to read the text from stdin")?;
            text
        }
    };
    let request = SpeechRequest {
        text: SpeechText::new(text)?,
        voice: voice.map(Voice::new).transpose()?,
        rate: rate.map(SpeakingRate::new).transpose()?,
    };
    if session.speak(request)? == SpeechOutcome::Stopped {
        println!("stopped before the end");
    }
    Ok(())
}
