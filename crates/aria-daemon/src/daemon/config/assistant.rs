//! What the assistant reads from the settings.

use std::path::Path;

use aria_assistant::chat::ChatConfig;
use aria_assistant::memory::MemoryConfig;
use aria_settings::config::memory_directory;
use aria_settings::{CheckedSettings, Memory};

const DEFAULT_MEMORY_EXTRACT_MODEL: &str = "sonnet";

const DEFAULT_MEMORY_MAX_FACTS: u32 = 10;

pub fn load_chat(settings: &CheckedSettings) -> ChatConfig {
    let choices = settings.choices();
    ChatConfig {
        workdir: settings.assistant.workdir.clone(),
        project_roots: settings.assistant.project_roots.clone().unwrap_or_default(),
        timeout: choices.assistant_timeout,
        delivery_timeout: choices.timeouts.delivery,
    }
}

pub fn load_memory(memory: &Memory, home: &Path) -> MemoryConfig {
    MemoryConfig {
        enabled: memory.enabled.unwrap_or(false),
        extract_model: memory
            .extract_model
            .clone()
            .unwrap_or_else(|| DEFAULT_MEMORY_EXTRACT_MODEL.to_owned()),
        max_facts_per_session: memory
            .max_facts_per_session
            .unwrap_or(DEFAULT_MEMORY_MAX_FACTS),
        dir: memory_directory(memory, home),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aria_settings::Loaded;
    use aria_settings::testing::{defaults_text, file, test_home};
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn chat_defaults_to_the_caller_directory_and_default_timeouts() {
        let chat = load_chat(&file(""));

        assert_eq!(
            chat,
            ChatConfig {
                workdir: None,
                project_roots: Vec::new(),
                timeout: aria_settings::config::DEFAULT_ASSISTANT_TIMEOUT,
                delivery_timeout: aria_settings::config::DEFAULT_DELIVERY_TIMEOUT,
            }
        );
    }

    #[test]
    fn chat_resolves_its_workdir_and_timeouts() {
        let file = file(
            "[assistant]\nworkdir = \"~/src\"\nproject_roots = [\"~\", \"/srv/code\"]\ntimeout_secs = 30.0\n             [delivery]\ntimeout_secs = 2.0\n",
        );
        let home_dir = test_home();

        let chat = load_chat(&file);

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
        let home = Path::new("/home/user");
        let defaults = Loaded::parse(&defaults_text(), home)
            .expect("every default is valid")
            .into_settings();
        assert_eq!(load_chat(&defaults), load_chat(&file("")));
        assert_eq!(
            load_memory(&defaults.memory, home),
            load_memory(&Memory::default(), home)
        );
    }
}
