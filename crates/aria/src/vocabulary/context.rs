use crate::settings::Directory;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Context {
    #[default]
    Default,
    Directory(Directory),
    Disabled,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum State {
    Ready,
    Refreshing,
    Stale,
    #[default]
    Unavailable,
    TooLarge,
    Disabled,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub state: State,
    pub catalog_revision: u64,
    pub runtime_revision: u64,
    pub terms: usize,
    pub error: String,
}
