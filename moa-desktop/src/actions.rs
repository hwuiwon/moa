//! Global GPUI actions for the MOA desktop app.
//!
//! Actions are dispatched through GPUI's keybinding system and handled by
//! listeners on the workspace, chat panel, or app root. Grouping them here
//! keeps the `actions!` macro invocations discoverable and lets other modules
//! register `cx.on_action` handlers in a single place.

use gpui::actions;

actions!(
    moa,
    [
        // Session management
        NewSession,
        CloseSession,
        NextSession,
        PreviousSession,
        // Navigation
        ToggleSidebar,
        ToggleDetailPanel,
        FocusPrompt,
        FocusSidebar,
        // Panels
        OpenCommandPalette,
        OpenSettings,
        OpenMemoryBrowser,
        OpenSkillManager,
        // Session control
        StopSession,
        ForceStopSession,
        // Approval (contextual, only when ApprovalCard holds focus)
        ApproveOnce,
        ApproveAlways,
        DenyApproval,
        // Memory
        SearchMemory,
        RefreshMemory,
        // Modal dismissal
        DismissModal,
        // Full-page Settings exit
        BackToApp,
        // Palette list navigation (scoped to CommandPalette)
        PaletteMoveUp,
        PaletteMoveDown,
        PaletteConfirm,
        // General
        Quit,
    ]
);
