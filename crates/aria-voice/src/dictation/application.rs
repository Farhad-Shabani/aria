//! Which apps get prose. "and forward slash or" → `and/or` in Notes, and
//! `and /or` in a terminal.

use aria_kit::transcription::Writing;
use aria_platform::delivery::Application;

const PROSE_APPS: &[&str] = &[
    "com.tinyspeck.slackmacgap",
    "com.apple.MobileSMS",
    "com.hnc.Discord",
    "net.whatsapp.WhatsApp",
    "ru.keepcoder.Telegram",
    "com.microsoft.teams2",
    "com.apple.Notes",
    "com.apple.TextEdit",
    "com.apple.iWork.Pages",
    "com.microsoft.Word",
    "md.obsidian",
    "notion.id",
    "com.apple.Safari",
    "com.google.Chrome",
    "company.thebrowser.Browser",
    "org.mozilla.firefox",
];

/// Prose in a listed app, its bundle id matched in any case. Commands
/// anywhere else.
pub(super) fn writing(application: Option<&Application>) -> Writing {
    let bundle_id = application.and_then(|application| application.bundle_id.as_deref());
    match bundle_id {
        Some(id) if PROSE_APPS.iter().any(|app| app.eq_ignore_ascii_case(id)) => Writing::Prose,
        _ => Writing::Commands,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listed_app_takes_prose_and_any_other_commands() {
        for (bundle_id, expected) in [
            (Some("com.tinyspeck.slackmacgap"), Writing::Prose),
            (Some("com.apple.Notes"), Writing::Prose),
            (Some("com.apple.notes"), Writing::Prose),
            (Some("com.apple.Safari"), Writing::Prose),
            (Some("com.mitchellh.ghostty"), Writing::Commands),
            (Some("com.example.Unknown"), Writing::Commands),
            (None, Writing::Commands),
        ] {
            let application = Application {
                name: "App".into(),
                bundle_id: bundle_id.map(Into::into),
            };
            assert_eq!(writing(Some(&application)), expected, "{bundle_id:?}");
        }
        assert_eq!(writing(None), Writing::Commands);
    }
}
