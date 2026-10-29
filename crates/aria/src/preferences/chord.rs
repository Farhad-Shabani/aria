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

/// A key held with at least one modifier: a bare key would be taken away
/// from every application.
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
}
