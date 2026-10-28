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
        /// Switch the diff between unified and side-by-side.
        ToggleDiffMode,
        /// Start a comment on the selected line(s).
        Comment,
        /// Run the selected comment with Claude.
        Run,
        /// Have Claude explain the selected lines, or the whole diff.
        Explain,
        /// Accept the open proposal.
        Accept,
        /// Reject the open proposal (asks first; press again to confirm).
        RejectProposal,
        /// Reply to the open proposal, starting a follow-up.
        ReplyToProposal,
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
        /// Hide both docks to read the diff alone, or bring them back.
        ToggleFocusMode,
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
        /// Look for a newer Argus release now.
        CheckForUpdates,
        /// Move the file-tree selection down one row.
        SelectNextEntry,
        /// Move the file-tree selection up one row.
        SelectPreviousEntry,
        /// Collapse the selected directory, or select the parent directory.
        CollapseEntry,
        /// Expand the selected directory, or select its first child.
        ExpandEntry,
        /// Open the selected file in the diff view (or toggle a directory).
        OpenEntry,
        /// Focus the file-tree filter box.
        FocusFileFilter,
        /// Switch the file tree between tree and flat list.
        ToggleFileListMode,
        /// Scroll to the next comment thread.
        NextComment,
        /// Scroll to the previous comment thread.
        PreviousComment,
        /// Collapse every comment thread, or expand them all again.
        ToggleAllComments,
        /// Switch the left dock between the file tree and the comment list.
        ToggleCommentList,
        /// Save the comment being written.
        SubmitComment,
        /// Debug builds: move the focused comment to its next status, to try
        /// the status chips before runs exist.
        AdvanceCommentStatus,
        /// Welcome screen: open the first recent repository.
        OpenRecent1,
        /// Welcome screen: open the second recent repository.
        OpenRecent2,
        /// Welcome screen: open the third recent repository.
        OpenRecent3,
        /// Welcome screen: open the fourth recent repository.
        OpenRecent4,
        /// Welcome screen: open the fifth recent repository.
        OpenRecent5,
        /// Welcome screen: open the sixth recent repository.
        OpenRecent6,
        /// Welcome screen: open the seventh recent repository.
        OpenRecent7,
        /// Welcome screen: open the eighth recent repository.
        OpenRecent8,
        /// Welcome screen: open the ninth recent repository.
        OpenRecent9,
    ]
);

actions!(
    argus_editor,
    [
        /// Delete the character before the caret (or the selection).
        Backspace,
        /// Delete the character after the caret (or the selection).
        Delete,
        /// Insert a line break.
        Newline,
        /// Move the caret one character left.
        MoveLeft,
        /// Move the caret one character right.
        MoveRight,
        /// Move the caret one line up.
        MoveUp,
        /// Move the caret one line down.
        MoveDown,
        /// Move the caret one word left.
        MoveWordLeft,
        /// Move the caret one word right.
        MoveWordRight,
        /// Move the caret to the start of the line.
        MoveToLineStart,
        /// Move the caret to the end of the line.
        MoveToLineEnd,
        /// Extend the selection one character left.
        SelectLeft,
        /// Extend the selection one character right.
        SelectRight,
        /// Extend the selection one line up.
        SelectUp,
        /// Extend the selection one line down.
        SelectDown,
        /// Extend the selection to the start of the line.
        SelectToLineStart,
        /// Extend the selection to the end of the line.
        SelectToLineEnd,
        /// Select the whole text.
        SelectAll,
        /// Copy the selection.
        Copy,
        /// Cut the selection.
        Cut,
        /// Paste the clipboard.
        Paste,
    ]
);

/// Key context set on the workspace root.
pub const WORKSPACE_CONTEXT: &str = "Workspace";

/// Key context that text-entry views (comment editors, filters) must set so
/// that typing letters there does not trigger the single-key shortcuts.
pub const TEXT_INPUT_CONTEXT: &str = "TextInput";

/// Key context set on the file tree dock.
pub const FILE_TREE_CONTEXT: &str = "FileTree";

/// Key context set on a proposal tab.
pub const PROPOSAL_CONTEXT: &str = "Proposal";

/// Key context the workspace root adds while the welcome screen (no
/// repository open, no sheet) is shown.
pub const WELCOME_CONTEXT: &str = "Welcome";

/// Key context of the multi-line comment editor (set together with
/// [`TEXT_INPUT_CONTEXT`]).
pub const COMMENT_EDITOR_CONTEXT: &str = "CommentEditor";

/// Predicate for the comment editor's keys.
const COMMENT_EDITOR: Option<&str> = Some("CommentEditor");

/// Predicate for single-key navigation shortcuts.
const NAVIGATION: Option<&str> = Some("Workspace && !TextInput");

/// Predicate for a proposal tab's keys; deeper than [`NAVIGATION`], so `r`
/// replies there instead of running.
const PROPOSAL: Option<&str> = Some("Proposal && !TextInput");

/// Predicate for the file tree's own keys (the filter box handles the arrows
/// itself).
const FILE_TREE: Option<&str> = Some("FileTree && !TextInput");

/// Predicate for the welcome screen's number keys.
const WELCOME: Option<&str> = Some("Welcome && !TextInput");

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
    /// How the help sheet lists the entry, so a family of bindings (the
    /// welcome screen's `1`-`9`) reads as one row.
    sheet: SheetRow,
    bind: fn(&'static str, Option<&'static str>) -> KeyBinding,
}

/// How an entry appears in the keyboard-shortcuts sheet.
#[derive(Clone, Copy)]
enum SheetRow {
    /// Listed with its formatted keystrokes.
    Keys,
    /// Listed with these keys instead (e.g. `1 … 9`).
    Label(&'static str),
    /// Covered by another entry's row.
    Hidden,
}

impl Shortcut {
    /// The keys to show for this entry in the keyboard-shortcuts sheet, or
    /// `None` when another entry's row already covers it.
    pub fn sheet_keys(&self) -> Option<String> {
        match self.sheet {
            SheetRow::Keys => Some(self.display_keys()),
            SheetRow::Label(keys) => Some(keys.to_owned()),
            SheetRow::Hidden => None,
        }
    }

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
        shortcut!(
            $keys,
            $action,
            $context,
            $group,
            $description,
            SheetRow::Keys
        )
    };
    (
        $keys:literal,
        $action:expr,
        $context:expr,
        $group:literal,
        $description:literal,
        $sheet:expr
    ) => {
        Shortcut {
            keystrokes: $keys,
            description: $description,
            group: $group,
            context: $context,
            sheet: $sheet,
            bind: |keys, context| KeyBinding::new(keys, $action, context),
        }
    };
}

/// One of the welcome screen's `2`-`9` keys; the `1` entry lists them all.
macro_rules! open_recent {
    ($keys:literal, $action:expr) => {
        shortcut!(
            $keys,
            $action,
            WELCOME,
            "Welcome",
            "Open a recent repository",
            SheetRow::Hidden
        )
    };
}

/// Every key binding in Argus.
pub const SHORTCUTS: &[Shortcut] = &[
    shortcut!("j", NextFile, NAVIGATION, "Navigation", "Next file"),
    shortcut!("k", PreviousFile, NAVIGATION, "Navigation", "Previous file"),
    shortcut!("n", NextHunk, NAVIGATION, "Navigation", "Next hunk"),
    shortcut!(
        "/",
        FocusFileFilter,
        NAVIGATION,
        "File tree",
        "Filter the files"
    ),
    shortcut!(
        "down",
        SelectNextEntry,
        FILE_TREE,
        "File tree",
        "Select the next row"
    ),
    shortcut!(
        "up",
        SelectPreviousEntry,
        FILE_TREE,
        "File tree",
        "Select the previous row"
    ),
    shortcut!(
        "left",
        CollapseEntry,
        FILE_TREE,
        "File tree",
        "Collapse the directory"
    ),
    shortcut!(
        "right",
        ExpandEntry,
        FILE_TREE,
        "File tree",
        "Expand the directory"
    ),
    shortcut!(
        "enter",
        OpenEntry,
        FILE_TREE,
        "File tree",
        "Open the file in the diff"
    ),
    shortcut!(
        "secondary-shift-l",
        ToggleFileListMode,
        None,
        "File tree",
        "Toggle tree / flat list"
    ),
    shortcut!("p", PreviousHunk, NAVIGATION, "Navigation", "Previous hunk"),
    shortcut!(
        "alt-z",
        ToggleSoftWrap,
        NAVIGATION,
        "Layout",
        "Toggle soft wrap in the diff"
    ),
    shortcut!(
        "secondary-shift-d",
        ToggleDiffMode,
        None,
        "Layout",
        "Toggle unified / side-by-side diff"
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
    shortcut!(
        "e",
        Explain,
        NAVIGATION,
        "Review",
        "Explain the selected lines (or the diff) with Claude"
    ),
    shortcut!("a", Accept, NAVIGATION, "Review", "Accept the proposal"),
    shortcut!(
        "x",
        RejectProposal,
        PROPOSAL,
        "Review",
        "Reject the proposal"
    ),
    shortcut!(
        "r",
        ReplyToProposal,
        PROPOSAL,
        "Review",
        "Reply to the proposal"
    ),
    shortcut!(
        "secondary-enter",
        SubmitComment,
        COMMENT_EDITOR,
        "Review",
        "Save the comment (Esc cancels)"
    ),
    shortcut!("] c", NextComment, NAVIGATION, "Review", "Next comment"),
    shortcut!(
        "[ c",
        PreviousComment,
        NAVIGATION,
        "Review",
        "Previous comment"
    ),
    shortcut!(
        "shift-c",
        ToggleAllComments,
        NAVIGATION,
        "Review",
        "Collapse / expand all comments"
    ),
    shortcut!(
        "secondary-shift-c",
        ToggleCommentList,
        None,
        "Review",
        "Show the files / the comment list"
    ),
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
        "secondary-.",
        ToggleFocusMode,
        None,
        "Layout",
        "Focus on the diff (hide both docks)"
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
    shortcut!(
        "1",
        OpenRecent1,
        WELCOME,
        "Welcome",
        "Open a recent repository",
        SheetRow::Label("1 … 9")
    ),
    open_recent!("2", OpenRecent2),
    open_recent!("3", OpenRecent3),
    open_recent!("4", OpenRecent4),
    open_recent!("5", OpenRecent5),
    open_recent!("6", OpenRecent6),
    open_recent!("7", OpenRecent7),
    open_recent!("8", OpenRecent8),
    open_recent!("9", OpenRecent9),
];

/// Help-sheet sections, in display order.
pub const SHORTCUT_GROUPS: &[&str] = &[
    "Navigation",
    "File tree",
    "Layout",
    "Review",
    "Appearance",
    "Application",
    "Welcome",
];

/// The text-editing keys of the comment editor. They are the usual ones of
/// each platform, so the shortcuts sheet does not list them.
pub fn editor_bindings() -> Vec<KeyBinding> {
    let context = COMMENT_EDITOR;
    let mut bindings = vec![
        KeyBinding::new("backspace", Backspace, context),
        KeyBinding::new("shift-backspace", Backspace, context),
        KeyBinding::new("delete", Delete, context),
        KeyBinding::new("enter", Newline, context),
        KeyBinding::new("shift-enter", Newline, context),
        KeyBinding::new("left", MoveLeft, context),
        KeyBinding::new("right", MoveRight, context),
        KeyBinding::new("up", MoveUp, context),
        KeyBinding::new("down", MoveDown, context),
        KeyBinding::new("home", MoveToLineStart, context),
        KeyBinding::new("end", MoveToLineEnd, context),
        KeyBinding::new("shift-left", SelectLeft, context),
        KeyBinding::new("shift-right", SelectRight, context),
        KeyBinding::new("shift-up", SelectUp, context),
        KeyBinding::new("shift-down", SelectDown, context),
        KeyBinding::new("shift-home", SelectToLineStart, context),
        KeyBinding::new("shift-end", SelectToLineEnd, context),
        KeyBinding::new("secondary-a", SelectAll, context),
        KeyBinding::new("secondary-c", Copy, context),
        KeyBinding::new("secondary-x", Cut, context),
        KeyBinding::new("secondary-v", Paste, context),
    ];
    if cfg!(target_os = "macos") {
        bindings.extend([
            KeyBinding::new("alt-left", MoveWordLeft, context),
            KeyBinding::new("alt-right", MoveWordRight, context),
            KeyBinding::new("cmd-left", MoveToLineStart, context),
            KeyBinding::new("cmd-right", MoveToLineEnd, context),
            KeyBinding::new("cmd-shift-left", SelectToLineStart, context),
            KeyBinding::new("cmd-shift-right", SelectToLineEnd, context),
            KeyBinding::new("ctrl-a", MoveToLineStart, context),
            KeyBinding::new("ctrl-e", MoveToLineEnd, context),
        ]);
    } else {
        bindings.extend([
            KeyBinding::new("ctrl-left", MoveWordLeft, context),
            KeyBinding::new("ctrl-right", MoveWordRight, context),
        ]);
    }
    bindings
}

/// Registers every binding in [`SHORTCUTS`], plus the comment editor's
/// [`editor_bindings`].
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys(SHORTCUTS.iter().map(Shortcut::binding));
    cx.bind_keys(editor_bindings());
}
