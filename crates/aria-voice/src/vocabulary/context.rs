use aria_settings::Directory;

use super::Completeness;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Context {
    #[default]
    Default,
    Directory(Directory),
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Project vocabulary is off, or no folder is chosen.
    Disabled,
    Folder {
        /// The list dictation uses now. Absent before the first scan, and
        /// after eviction or a forget.
        list: Option<List>,
        activity: Activity,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct List {
    /// The catalog generation it came from. `None` when the catalog didn't
    /// keep the scan.
    pub catalog_revision: Option<u64>,
    pub terms: usize,
    /// Names that passed the keep rules, before the cap left `terms`.
    pub found: usize,
    /// `None` for a list loaded from the catalog, which doesn't keep it.
    pub completeness: Option<Completeness>,
    /// Personal rules changed after it was compiled.
    pub is_stale: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    Idle,
    Refreshing,
    Removing,
    Failed(Failure),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The last refresh exceeded a resource limit.
    Limit,
    Refresh,
    /// The list was dropped to free memory.
    Evicted,
    /// A forget couldn't remove the folder's stored names.
    Removal,
}
