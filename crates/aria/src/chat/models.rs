use std::collections::HashSet;
use std::process::Command;
use std::time::Duration;

use aria_kit::cancel::CancelToken;
use serde::{Deserialize, Serialize};

use super::{AssistantError, run_cli};

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);
const CLAUDE_REQUEST_ID: &str = "aria-model-list";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantModel {
    pub id: String,
    pub name: String,
}

impl AssistantModel {
    pub(crate) fn is_valid(&self) -> bool {
        !self.id.is_empty()
            && is_valid_model_id(&self.id)
            && !self.name.is_empty()
            && self.name.len() <= 200
            && !self.name.chars().any(char::is_control)
    }
}

pub(crate) fn is_valid_model_id(model_id: &str) -> bool {
    const MAX_MODEL_ID_BYTES: usize = 200;
    model_id.len() <= MAX_MODEL_ID_BYTES
        && !model_id.starts_with('-')
        && !model_id
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
}

pub(super) fn discover(
    mut command: Command,
    input: &str,
    parse: fn(&str) -> Result<Vec<AssistantModel>, AssistantError>,
    cancel: &CancelToken,
) -> Result<Vec<AssistantModel>, AssistantError> {
    let directory = tempfile::tempdir().map_err(AssistantError::RunDirectory)?;
    command.current_dir(directory.path());
    let output = run_cli(&mut command, input, |_| {}, cancel, DISCOVERY_TIMEOUT)?;
    if !output.status.success() {
        return output.reply(Err(AssistantError::Malformed(
            "model discovery did not finish".to_owned(),
        )));
    }
    let models = parse(&output.stdout)?;
    let mut ids = HashSet::new();
    for model in &models {
        if !ids.insert(&model.id) || !model.is_valid() {
            return Err(AssistantError::Malformed(
                "the model list contains an invalid entry".to_owned(),
            ));
        }
    }
    Ok(models)
}

#[derive(Deserialize)]
struct CodexModels {
    models: Vec<CodexModel>,
}

#[derive(Deserialize)]
struct CodexModel {
    slug: String,
    display_name: String,
    visibility: String,
    priority: i64,
}

pub(super) fn parse_codex(stdout: &str) -> Result<Vec<AssistantModel>, AssistantError> {
    let mut catalog: CodexModels = serde_json::from_str(stdout)
        .map_err(|_| AssistantError::Malformed("the model list could not be read".to_owned()))?;
    catalog.models.retain(|model| model.visibility == "list");
    catalog.models.sort_by_key(|model| model.priority);
    Ok(catalog
        .models
        .into_iter()
        .map(|model| AssistantModel {
            id: model.slug,
            name: model.display_name,
        })
        .collect())
}

#[derive(Deserialize)]
struct ClaudeModels {
    models: Vec<ClaudeModel>,
}

#[derive(Deserialize)]
struct ClaudeModel {
    value: String,
    #[serde(rename = "displayName")]
    display_name: String,
}

pub(super) fn parse_claude(stdout: &str) -> Result<Vec<AssistantModel>, AssistantError> {
    for (index, line) in stdout.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event: serde_json::Value = serde_json::from_str(line).map_err(|_| {
            AssistantError::Malformed(format!(
                "model discovery line {} is not valid JSON",
                index + 1
            ))
        })?;
        if event["type"] != "control_response"
            || event["response"]["request_id"] != CLAUDE_REQUEST_ID
        {
            continue;
        }
        if event["response"]["subtype"] != "success" {
            return Err(AssistantError::Failed(
                "model discovery was refused".to_owned(),
            ));
        }
        let catalog: ClaudeModels = serde_json::from_value(event["response"]["response"].clone())
            .map_err(|_| {
            AssistantError::Malformed("the model list could not be read".to_owned())
        })?;
        return Ok(catalog
            .models
            .into_iter()
            .filter(|model| model.value != "default")
            .map(|model| AssistantModel {
                id: model.value,
                name: model.display_name,
            })
            .collect());
    }
    Err(AssistantError::Malformed(
        "model discovery returned no initialization response".to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_discovered_model_ids_are_refused() {
        let result = discover(
            Command::new("true"),
            "",
            |_| {
                Ok(vec![
                    AssistantModel {
                        id: "same".into(),
                        name: "First".into(),
                    },
                    AssistantModel {
                        id: "same".into(),
                        name: "Second".into(),
                    },
                ])
            },
            &CancelToken::never(),
        );
        assert!(matches!(result, Err(AssistantError::Malformed(_))));
    }

    #[test]
    fn codex_models_hide_non_picker_entries_and_keep_catalog_order() {
        let models = parse_codex(
            r#"{"models":[
                {"slug":"later","display_name":"Later","visibility":"list","priority":2,"base_instructions":"ignored"},
                {"slug":"hidden","display_name":"Hidden","visibility":"hidden","priority":0},
                {"slug":"first","display_name":"First","visibility":"list","priority":1}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            models,
            vec![
                AssistantModel {
                    id: "first".into(),
                    name: "First".into()
                },
                AssistantModel {
                    id: "later".into(),
                    name: "Later".into()
                },
            ]
        );
        assert!(parse_codex("not JSON").is_err());
        assert!(parse_codex(r#"{"models":[{"slug":"missing-fields"}]}"#).is_err());
    }

    #[test]
    fn claude_initialization_keeps_models_and_ignores_other_events_and_account_data() {
        let models = parse_claude(
            concat!(
                "{\"type\":\"system\",\"subtype\":\"init\"}\n",
                "{\"type\":\"control_response\",\"response\":{\"request_id\":\"another-request\",\"subtype\":\"error\"}}\n",
                "{\"type\":\"control_response\",\"response\":{\"request_id\":\"aria-model-list\",\"subtype\":\"success\",\"response\":{\"account\":\"private\",\"models\":[{\"value\":\"default\",\"displayName\":\"Default\"},{\"value\":\"opus\",\"displayName\":\"Opus\",\"resolvedModel\":\"ignored\"}]}}}\n",
            ),
        )
        .unwrap();
        assert_eq!(
            models,
            vec![AssistantModel {
                id: "opus".into(),
                name: "Opus".into()
            }]
        );
    }

    #[test]
    fn claude_discovery_requires_a_successful_initialization_response() {
        for stdout in [
            "not JSON",
            r#"{"type":"system"}"#,
            r#"{"type":"control_response","response":{"request_id":"aria-model-list","subtype":"error","error":"private"}}"#,
            r#"{"type":"control_response","response":{"request_id":"aria-model-list","subtype":"success","response":{}}}"#,
            r#"{"type":"control_response","response":{"request_id":"aria-model-list","subtype":"success","response":"private"}}"#,
        ] {
            let error = parse_claude(stdout).unwrap_err().to_string();
            assert!(!error.contains("private"));
        }
    }
}
