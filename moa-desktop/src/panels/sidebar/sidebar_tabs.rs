//! Tab switcher for Sessions / Memory / Skills panels.

/// Top-level sidebar tab selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidebarTab {
    Sessions,
    Memory,
    Skills,
}

impl SidebarTab {
    pub const ALL: [SidebarTab; 3] = [SidebarTab::Sessions, SidebarTab::Memory, SidebarTab::Skills];

    pub fn label(self) -> &'static str {
        match self {
            SidebarTab::Sessions => "Sessions",
            SidebarTab::Memory => "Memory",
            SidebarTab::Skills => "Skills",
        }
    }
}
