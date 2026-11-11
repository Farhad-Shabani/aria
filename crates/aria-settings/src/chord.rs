/// The modifiers held with a chord's key, named rather than masked: the mask
/// constants belong to one platform and this is stored on all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    pub control: bool,
    pub option: bool,
    pub shift: bool,
    pub command: bool,
}

impl Modifiers {
    pub fn is_empty(self) -> bool {
        !(self.control || self.option || self.shift || self.command)
    }
}

/// A key held with Control, Option, or Shift, and maybe Command. A bare key
/// would be taken from every application, and Command alone from the app in
/// front: ⌘Q, ⌘W, ⌘S are its own, and a registered chord wins over them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chord {
    key_code: u16,
    modifiers: Modifiers,
}

impl Chord {
    pub fn new(key_code: u16, modifiers: Modifiers) -> Result<Self, ChordError> {
        if modifiers.is_empty() {
            return Err(ChordError::NoModifier);
        }
        if !(modifiers.control || modifiers.option || modifiers.shift) {
            return Err(ChordError::CommandAlone);
        }
        Ok(Self {
            key_code,
            modifiers,
        })
    }

    pub fn key_code(self) -> u16 {
        self.key_code
    }

    pub fn modifiers(self) -> Modifiers {
        self.modifiers
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ChordError {
    #[error("a chord needs at least one modifier")]
    NoModifier,

    #[error("a chord needs Control, Option, or Shift; Command alone belongs to the app in front")]
    CommandAlone,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_alone_is_refused_and_command_with_another_modifier_is_kept() {
        let command = Modifiers {
            command: true,
            ..Modifiers::default()
        };
        assert!(matches!(
            Chord::new(12, command),
            Err(ChordError::CommandAlone)
        ));
        let shift_command = Modifiers {
            shift: true,
            ..command
        };
        assert_eq!(
            Chord::new(12, shift_command).unwrap().modifiers(),
            shift_command
        );
    }
}
