use std::borrow::Borrow;
use std::collections::BTreeMap;

use crate::delivery::OutputMode;
use crate::dictation::DeliveryMode;

const MAX_APPLICATIONS: usize = 64;
const MAX_ID_BYTES: usize = 255;

/// An application as the platform names it: a bundle ID on macOS.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ApplicationId(String);

impl ApplicationId {
    /// Apple allows letters, digits, '.' and '-'; some shipping apps also use '_'.
    pub fn new(id: impl Into<String>) -> Result<Self, OverrideError> {
        let id = id.into();
        if id.is_empty() || id.len() > MAX_ID_BYTES {
            return Err(OverrideError::IdLength);
        }
        if !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        {
            return Err(OverrideError::IdCharacters);
        }
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Lets the map be searched by the front application's raw ID.
impl Borrow<str> for ApplicationId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

/// Dictation settings that replace shared settings. Each `None` inherits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DictationOverrides {
    pub output_mode: Option<OutputMode>,
    pub delivery_mode: Option<DeliveryMode>,
}

/// What one application changes about Aria while it is in front.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ApplicationOverride {
    /// Aria starts no recording and delivers nothing. `overrides` are kept
    /// for when the app is switched back on.
    pub excluded: bool,
    pub overrides: DictationOverrides,
}

/// Overrides keyed by the application that must be in front for them to apply.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ApplicationOverrides(BTreeMap<ApplicationId, ApplicationOverride>);

impl ApplicationOverrides {
    pub fn new(
        entries: impl IntoIterator<Item = (ApplicationId, ApplicationOverride)>,
    ) -> Result<Self, OverrideError> {
        let mut map = BTreeMap::new();
        for (id, overrides) in entries {
            if map.insert(id, overrides).is_some() {
                return Err(OverrideError::DuplicateApplication);
            }
            if map.len() > MAX_APPLICATIONS {
                return Err(OverrideError::TooManyApplications);
            }
        }
        Ok(Self(map))
    }

    pub fn get(&self, id: &str) -> Option<ApplicationOverride> {
        self.0.get(id).copied()
    }

    pub fn excluded_ids(&self) -> Vec<String> {
        self.0
            .iter()
            .filter(|(_, entry)| entry.excluded)
            .map(|(id, _)| id.as_str().to_owned())
            .collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&ApplicationId, &ApplicationOverride)> {
        self.0.iter()
    }

    /// Drops choices that change nothing. A value equal to the shared
    /// setting goes, so the app follows that setting when it changes.
    /// An app's own Copy drops its delivery choice, because Copy cannot
    /// stream. A shared Copy keeps it for when that setting changes.
    pub(crate) fn without_inert(self, output: OutputMode, delivery: DeliveryMode) -> Self {
        let map = self
            .0
            .into_iter()
            .map(|(id, entry)| {
                let DictationOverrides {
                    output_mode,
                    delivery_mode,
                } = entry.overrides;
                let output_mode = output_mode.filter(|mode| *mode != output);
                let copies = output_mode == Some(OutputMode::Copy);
                let overrides = DictationOverrides {
                    output_mode,
                    delivery_mode: delivery_mode.filter(|mode| !copies && *mode != delivery),
                };
                (id, ApplicationOverride { overrides, ..entry })
            })
            .collect();
        Self(map)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OverrideError {
    #[error("an application ID must be 1 to {MAX_ID_BYTES} bytes")]
    IdLength,
    #[error("an application ID may hold only letters, digits, '.', '-' and '_'")]
    IdCharacters,
    #[error("an application has overrides more than once")]
    DuplicateApplication,
    #[error("at most {MAX_APPLICATIONS} applications may have overrides")]
    TooManyApplications,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(raw: &str) -> ApplicationId {
        ApplicationId::new(raw).expect("a valid application ID")
    }

    fn copy() -> ApplicationOverride {
        ApplicationOverride {
            excluded: false,
            overrides: DictationOverrides {
                output_mode: Some(OutputMode::Copy),
                delivery_mode: None,
            },
        }
    }

    #[test]
    fn application_ids_are_bundle_id_shaped() {
        assert_eq!(
            id("com.tinyspeck.slackmacgap").as_str(),
            "com.tinyspeck.slackmacgap"
        );
        assert_eq!(id("com.example.my_app").as_str(), "com.example.my_app");
        assert_eq!(ApplicationId::new(""), Err(OverrideError::IdLength));
        assert_eq!(
            ApplicationId::new("a".repeat(256)),
            Err(OverrideError::IdLength)
        );
        assert_eq!(
            ApplicationId::new("com.app/x"),
            Err(OverrideError::IdCharacters)
        );
        assert_eq!(
            ApplicationId::new("com.app x"),
            Err(OverrideError::IdCharacters)
        );
    }

    #[test]
    fn each_application_appears_once() {
        assert_eq!(
            ApplicationOverrides::new([(id("com.app"), copy()), (id("com.app"), copy())]),
            Err(OverrideError::DuplicateApplication)
        );
    }

    #[test]
    fn the_map_is_bounded() {
        let entries = |count: usize| (0..count).map(|index| (id(&format!("app.{index}")), copy()));

        assert!(ApplicationOverrides::new(entries(MAX_APPLICATIONS)).is_ok());
        assert_eq!(
            ApplicationOverrides::new(entries(MAX_APPLICATIONS + 1)),
            Err(OverrideError::TooManyApplications)
        );
    }

    #[test]
    fn dropping_shared_values_keeps_an_exclusion() {
        let excluded = ApplicationOverride {
            excluded: true,
            ..copy()
        };
        let overrides = ApplicationOverrides::new([(id("com.app"), excluded)]).expect("valid");

        let normalized = overrides.without_inert(OutputMode::Copy, DeliveryMode::Complete);

        assert_eq!(
            normalized.get("com.app"),
            Some(ApplicationOverride {
                excluded: true,
                overrides: DictationOverrides::default(),
            })
        );
    }

    #[test]
    fn only_an_apps_own_copy_drops_its_delivery_choice() {
        let streaming = ApplicationOverride {
            excluded: false,
            overrides: DictationOverrides {
                output_mode: None,
                delivery_mode: Some(DeliveryMode::Streaming),
            },
        };
        let copy_streaming = ApplicationOverride {
            overrides: DictationOverrides {
                delivery_mode: Some(DeliveryMode::Streaming),
                ..copy().overrides
            },
            ..copy()
        };
        let overrides = ApplicationOverrides::new([
            (id("com.copy"), copy_streaming),
            (id("com.follows"), streaming),
        ])
        .expect("valid");

        let normalized = overrides.without_inert(OutputMode::Insert, DeliveryMode::Complete);
        assert_eq!(normalized.get("com.copy"), Some(copy()));
        assert_eq!(normalized.get("com.follows"), Some(streaming));

        let under_shared_copy = ApplicationOverrides::new([(id("com.follows"), streaming)])
            .expect("valid")
            .without_inert(OutputMode::Copy, DeliveryMode::Complete);
        assert_eq!(under_shared_copy.get("com.follows"), Some(streaming));
    }
}
