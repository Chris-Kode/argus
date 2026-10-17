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

/// Key context of the multi-line comment editor (set together with
/// [`TEXT_INPUT_CONTEXT`]).
pub const COMMENT_EDITOR_CONTEXT: &str = "CommentEditor";

/// Predicate for the comment editor's keys.
const COMMENT_EDITOR: Option<&str> = Some("CommentEditor");

/// Predicate for single-key navigation shortcuts.
const NAVIGATION: Option<&str> = Some("Workspace && !TextInput");

/// Predicate for the file tree's own keys (the filter box handles the arrows
/// itself).
const FILE_TREE: Option<&str> = Some("FileTree && !TextInput");

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
    shortcut!("a", Accept, NAVIGATION, "Review", "Accept the proposal"),
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
    "File tree",
    "Layout",
    "Review",
    "Appearance",
    "Application",
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
