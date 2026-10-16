//! Actions and the one table of key bindings.
//!
//! [`SHORTCUTS`] is the single source of truth: it is what gets bound at
//! startup and what the keyboard-shortcuts sheet lists. `secondary-` is cmd on
//! macOS and ctrl elsewhere.

use gpui::{App, KeyBinding, actions};

actions!(
    argus,
    [
        /// Select the next file in the diff.
        NextFile,
        /// Select the previous file in the diff.
        PreviousFile,
        /// Jump to the next hunk.
        NextHunk,
        /// Jump to the previous hunk.
        PreviousHunk,
        /// Soft-wrap long diff lines, or scroll them horizontally.
        ToggleSoftWrap,
        /// Start a comment on the selected line(s).
        Comment,
        /// Run the selected comment with Claude.
        Run,
        /// Accept the open proposal.
        Accept,
        /// Open the settings sheet.
        OpenSettings,
        /// Pick a repository to open.
        OpenRepository,
        /// Show or hide the keyboard-shortcuts sheet.
        ToggleKeyboardShortcuts,
        /// Show or hide the file tree dock.
        ToggleFileTree,
        /// Show or hide the run panel dock.
        ToggleRunPanel,
        /// Move the run panel between the right and the bottom.
        ToggleRunPanelPosition,
        /// Cycle the theme: system, dark, light.
        CycleTheme,
        /// Make the UI and code fonts larger.
        IncreaseFontSize,
        /// Make the UI and code fonts smaller.
        DecreaseFontSize,
        /// Reset the UI and code font sizes.
        ResetFontSize,
        /// Close the open sheet.
        Dismiss,
        /// Quit Argus.
        Quit,
    ]
);

/// Key context set on the workspace root.
pub const WORKSPACE_CONTEXT: &str = "Workspace";

/// Key context that text-entry views (comment editors, filters) must set so
/// that typing letters there does not trigger the single-key shortcuts.
pub const TEXT_INPUT_CONTEXT: &str = "TextInput";

/// Predicate for single-key navigation shortcuts.
const NAVIGATION: Option<&str> = Some("Workspace && !TextInput");

/// One entry of the keymap.
pub struct Shortcut {
    /// Keystrokes in GPUI syntax, e.g. `"secondary-o"`.
    pub keystrokes: &'static str,
    /// What the shortcut does, as shown in the help sheet.
    pub description: &'static str,
    /// Help-sheet section.
    pub group: &'static str,
    /// Key context predicate; `None` binds globally.
    pub context: Option<&'static str>,
    bind: fn(&'static str, Option<&'static str>) -> KeyBinding,
}

impl Shortcut {
    /// Builds the GPUI binding for this entry.
    pub fn binding(&self) -> KeyBinding {
        (self.bind)(self.keystrokes, self.context)
    }

    /// The keystrokes formatted for the current platform (e.g. `⌘,` on macOS,
    /// `ctrl-,` on Linux).
    pub fn display_keys(&self) -> String {
        self.binding()
            .keystrokes()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

macro_rules! shortcut {
    ($keys:literal, $action:expr, $context:expr, $group:literal, $description:literal) => {
        Shortcut {
            keystrokes: $keys,
            description: $description,
            group: $group,
            context: $context,
            bind: |keys, context| KeyBinding::new(keys, $action, context),
        }
    };
}

/// Every key binding in Argus.
pub const SHORTCUTS: &[Shortcut] = &[
    shortcut!("j", NextFile, NAVIGATION, "Navigation", "Next file"),
    shortcut!("k", PreviousFile, NAVIGATION, "Navigation", "Previous file"),
    shortcut!("n", NextHunk, NAVIGATION, "Navigation", "Next hunk"),
    shortcut!("p", PreviousHunk, NAVIGATION, "Navigation", "Previous hunk"),
    shortcut!(
        "alt-z",
        ToggleSoftWrap,
        NAVIGATION,
        "Layout",
        "Toggle soft wrap in the diff"
    ),
    shortcut!(
        "c",
        Comment,
        NAVIGATION,
        "Review",
        "Comment on the selected line"
    ),
    shortcut!(
        "r",
        Run,
        NAVIGATION,
        "Review",
        "Run the comment with Claude"
    ),
    shortcut!("a", Accept, NAVIGATION, "Review", "Accept the proposal"),
    shortcut!(
        "secondary-o",
        OpenRepository,
        None,
        "Application",
        "Open a repository"
    ),
    shortcut!("secondary-,", OpenSettings, None, "Application", "Settings"),
    shortcut!(
        "secondary-/",
        ToggleKeyboardShortcuts,
        None,
        "Application",
        "Keyboard shortcuts"
    ),
    shortcut!("secondary-q", Quit, None, "Application", "Quit"),
    shortcut!(
        "secondary-b",
        ToggleFileTree,
        None,
        "Layout",
        "Toggle the file tree"
    ),
    shortcut!(
        "secondary-j",
        ToggleRunPanel,
        None,
        "Layout",
        "Toggle the run panel"
    ),
    shortcut!(
        "secondary-shift-j",
        ToggleRunPanelPosition,
        None,
        "Layout",
        "Dock the run panel right / bottom"
    ),
    shortcut!(
        "secondary-k secondary-t",
        CycleTheme,
        None,
        "Appearance",
        "Cycle theme (system, dark, light)"
    ),
    shortcut!(
        "secondary-=",
        IncreaseFontSize,
        None,
        "Appearance",
        "Increase font size"
    ),
    shortcut!(
        "secondary--",
        DecreaseFontSize,
        None,
        "Appearance",
        "Decrease font size"
    ),
    shortcut!(
        "secondary-0",
        ResetFontSize,
        None,
        "Appearance",
        "Reset font size"
    ),
    shortcut!("escape", Dismiss, None, "Application", "Close the sheet"),
];

/// Help-sheet sections, in display order.
pub const SHORTCUT_GROUPS: &[&str] = &[
    "Navigation",
    "Review",
    "Layout",
    "Appearance",
    "Application",
];

/// Registers every binding in [`SHORTCUTS`].
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys(SHORTCUTS.iter().map(Shortcut::binding));
}
