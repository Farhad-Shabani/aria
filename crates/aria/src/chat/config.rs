//! Where and how long `aria ask` runs the assistant.

use std::path::PathBuf;
use std::time::Duration;

use aria_settings::config::{ConfigError, delivery_timeout, timeout};
use aria_settings::settings::SettingsFile;

/// Where and how long `aria ask` runs the assistant. Loaded per ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChatConfig {
    /// Pins the assistant's working directory; otherwise the caller's.
    pub(crate) workdir: Option<PathBuf>,
    /// Directories whose git repos an ask may name; empty turns routing off.
    pub(crate) project_roots: Vec<PathBuf>,
    pub(crate) timeout: Duration,
    /// A reply delivered like a transcript gets dictation's delivery timeout.
    pub(crate) delivery_timeout: Duration,
}

/// Long enough for an assistant to read a repo and think.
const DEFAULT_CHAT_TIMEOUT: Duration = Duration::from_secs(300);

impl ChatConfig {
    pub(crate) fn load(settings: &SettingsFile) -> Result<Self, ConfigError> {
        let timeout = timeout(
            settings.assistant.timeout_secs,
            "assistant.timeout_secs",
            DEFAULT_CHAT_TIMEOUT,
        )?;
        Ok(Self {
            workdir: settings.assistant.workdir.clone(),
            project_roots: settings.assistant.project_roots.clone().unwrap_or_default(),
            timeout,
            delivery_timeout: delivery_timeout(&settings.delivery)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aria_settings::settings::Loaded;
    use std::path::Path;

    fn test_home() -> PathBuf {
        PathBuf::from("/home/user")
    }

    fn file(text: &str) -> SettingsFile {
        Loaded::parse(text, &test_home())
            .expect("the file is valid")
            .settings
    }

    #[test]
    fn chat_defaults_to_the_caller_directory_and_default_timeouts() {
        let chat = ChatConfig::load(&SettingsFile::default()).expect("defaults resolve");

        assert_eq!(
            chat,
            ChatConfig {
                workdir: None,
                project_roots: Vec::new(),
                timeout: DEFAULT_CHAT_TIMEOUT,
                delivery_timeout: aria_settings::config::DEFAULT_DELIVERY_TIMEOUT,
            }
        );
    }

    #[test]
    fn chat_resolves_its_workdir_and_timeouts() {
        let file = file(
            "[assistant]\nworkdir = \"~/src\"\nproject_roots = [\"~\", \"/srv/code\"]\ntimeout_secs = 30.0\n\
             [delivery]\ntimeout_secs = 2.0\n",
        );
        let home_dir = test_home();

        let chat = ChatConfig::load(&file).expect("the assistant table resolves");

        assert_eq!(chat.workdir, Some(home_dir.join("src")));
        assert_eq!(
            chat.project_roots,
            vec![home_dir, PathBuf::from("/srv/code")]
        );
        assert_eq!(chat.timeout, Duration::from_secs(30));
        assert_eq!(chat.delivery_timeout, Duration::from_secs(2));
    }

    #[test]
    fn the_defaults_match_what_code_does_without_them() {
        let defaults = Loaded::parse(
            &aria_settings::settings::testing::defaults_text(),
            Path::new("/home/user"),
        )
        .expect("every default is valid")
        .settings;
        assert_eq!(
            ChatConfig::load(&defaults).unwrap(),
            ChatConfig::load(&SettingsFile::default()).unwrap()
        );
    }
}
