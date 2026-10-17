//! Highlight kinds: the Zed-style capture names a theme colors.

/// What a highlighted span of code is, named like Zed's theme syntax keys
/// (`keyword`, `string`, `comment`, `function`, `type`, …).
///
/// Grammar queries use many more capture names (`function.method.call`,
/// `markup.heading.1`, nvim-style `conditional`, …); each one resolves to
/// the kind with the most specific matching name (see
/// [`RECOGNIZED_NAMES`]). Captures that match nothing stay plain text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum HighlightKind {
    /// `attribute`: annotations, attributes, HTML attributes.
    Attribute,
    /// `boolean`: `true`, `false`.
    Boolean,
    /// `comment`.
    Comment,
    /// `comment.doc`: documentation comments.
    CommentDoc,
    /// `constant`: constants and builtin constants (`null`, `None`).
    Constant,
    /// `constructor`: constructors and enum-variant-like calls.
    Constructor,
    /// `embedded`: code embedded in another language (`${}` contents).
    Embedded,
    /// `emphasis`: Markdown italics.
    Emphasis,
    /// `emphasis.strong`: Markdown bold.
    EmphasisStrong,
    /// `enum`.
    Enum,
    /// `function`: function and method names.
    Function,
    /// `keyword`.
    Keyword,
    /// `label`: loop labels, lifetimes.
    Label,
    /// `link_text`: Markdown link text.
    LinkText,
    /// `link_uri`: URLs.
    LinkUri,
    /// `number`.
    Number,
    /// `operator`.
    Operator,
    /// `preproc`: preprocessor directives.
    Preproc,
    /// `property`: fields, object keys.
    Property,
    /// `punctuation`.
    Punctuation,
    /// `punctuation.bracket`.
    PunctuationBracket,
    /// `punctuation.delimiter`: `,`, `.`, `;`.
    PunctuationDelimiter,
    /// `punctuation.list_marker`: Markdown list bullets.
    PunctuationListMarker,
    /// `punctuation.special`: interpolation delimiters and the like.
    PunctuationSpecial,
    /// `string`.
    String,
    /// `string.escape`.
    StringEscape,
    /// `string.regex`.
    StringRegex,
    /// `string.special`: keys, paths, other special strings.
    StringSpecial,
    /// `string.special.symbol`: Ruby symbols and the like.
    StringSpecialSymbol,
    /// `tag`: HTML/JSX tags.
    Tag,
    /// `text.literal`: Markdown code.
    TextLiteral,
    /// `title`: Markdown headings.
    Title,
    /// `type`.
    Type,
    /// `variable`.
    Variable,
    /// `variable.parameter`.
    VariableParameter,
    /// `variable.special`: `self`, `this`, builtin variables.
    VariableSpecial,
    /// `variant`: enum variants.
    Variant,
}

impl HighlightKind {
    /// Number of kinds; [`HighlightKind::index`] is below it.
    pub const COUNT: usize = HighlightKind::Variant as usize + 1;

    /// Every kind, in [`HighlightKind::index`] order.
    pub const ALL: [HighlightKind; HighlightKind::COUNT] = [
        HighlightKind::Attribute,
        HighlightKind::Boolean,
        HighlightKind::Comment,
        HighlightKind::CommentDoc,
        HighlightKind::Constant,
        HighlightKind::Constructor,
        HighlightKind::Embedded,
        HighlightKind::Emphasis,
        HighlightKind::EmphasisStrong,
        HighlightKind::Enum,
        HighlightKind::Function,
        HighlightKind::Keyword,
        HighlightKind::Label,
        HighlightKind::LinkText,
        HighlightKind::LinkUri,
        HighlightKind::Number,
        HighlightKind::Operator,
        HighlightKind::Preproc,
        HighlightKind::Property,
        HighlightKind::Punctuation,
        HighlightKind::PunctuationBracket,
        HighlightKind::PunctuationDelimiter,
        HighlightKind::PunctuationListMarker,
        HighlightKind::PunctuationSpecial,
        HighlightKind::String,
        HighlightKind::StringEscape,
        HighlightKind::StringRegex,
        HighlightKind::StringSpecial,
        HighlightKind::StringSpecialSymbol,
        HighlightKind::Tag,
        HighlightKind::TextLiteral,
        HighlightKind::Title,
        HighlightKind::Type,
        HighlightKind::Variable,
        HighlightKind::VariableParameter,
        HighlightKind::VariableSpecial,
        HighlightKind::Variant,
    ];

    /// Dense index, for per-kind tables such as theme colors.
    pub fn index(self) -> usize {
        self as usize
    }

    /// The Zed theme key of this kind (`"string.escape"`).
    pub fn name(self) -> &'static str {
        match self {
            HighlightKind::Attribute => "attribute",
            HighlightKind::Boolean => "boolean",
            HighlightKind::Comment => "comment",
            HighlightKind::CommentDoc => "comment.doc",
            HighlightKind::Constant => "constant",
            HighlightKind::Constructor => "constructor",
            HighlightKind::Embedded => "embedded",
            HighlightKind::Emphasis => "emphasis",
            HighlightKind::EmphasisStrong => "emphasis.strong",
            HighlightKind::Enum => "enum",
            HighlightKind::Function => "function",
            HighlightKind::Keyword => "keyword",
            HighlightKind::Label => "label",
            HighlightKind::LinkText => "link_text",
            HighlightKind::LinkUri => "link_uri",
            HighlightKind::Number => "number",
            HighlightKind::Operator => "operator",
            HighlightKind::Preproc => "preproc",
            HighlightKind::Property => "property",
            HighlightKind::Punctuation => "punctuation",
            HighlightKind::PunctuationBracket => "punctuation.bracket",
            HighlightKind::PunctuationDelimiter => "punctuation.delimiter",
            HighlightKind::PunctuationListMarker => "punctuation.list_marker",
            HighlightKind::PunctuationSpecial => "punctuation.special",
            HighlightKind::String => "string",
            HighlightKind::StringEscape => "string.escape",
            HighlightKind::StringRegex => "string.regex",
            HighlightKind::StringSpecial => "string.special",
            HighlightKind::StringSpecialSymbol => "string.special.symbol",
            HighlightKind::Tag => "tag",
            HighlightKind::TextLiteral => "text.literal",
            HighlightKind::Title => "title",
            HighlightKind::Type => "type",
            HighlightKind::Variable => "variable",
            HighlightKind::VariableParameter => "variable.parameter",
            HighlightKind::VariableSpecial => "variable.special",
            HighlightKind::Variant => "variant",
        }
    }
}

/// Capture names the highlighter recognizes, with the kind each maps to.
///
/// A query capture resolves to the entry whose dot-separated parts all
/// appear in it, preferring the most parts; on a tie the earlier entry wins
/// (so `keyword.function` is a keyword, not a function). Besides Zed's own
/// names this lists the aliases the bundled grammar queries use
/// (tree-sitter, nvim-treesitter and Helix conventions).
pub const RECOGNIZED_NAMES: &[(&str, HighlightKind)] = &[
    // Keywords first: they win ties such as `keyword.function`.
    ("keyword", HighlightKind::Keyword),
    ("conditional", HighlightKind::Keyword),
    ("repeat", HighlightKind::Keyword),
    ("include", HighlightKind::Keyword),
    ("exception", HighlightKind::Keyword),
    ("storageclass", HighlightKind::Keyword),
    ("keyword.directive", HighlightKind::Preproc),
    ("preproc", HighlightKind::Preproc),
    ("attribute", HighlightKind::Attribute),
    ("tag.attribute", HighlightKind::Attribute),
    ("boolean", HighlightKind::Boolean),
    ("constant.builtin.boolean", HighlightKind::Boolean),
    ("comment", HighlightKind::Comment),
    ("comment.doc", HighlightKind::CommentDoc),
    ("comment.documentation", HighlightKind::CommentDoc),
    ("constant", HighlightKind::Constant),
    ("constant.builtin", HighlightKind::Constant),
    ("constant.numeric", HighlightKind::Number),
    ("constant.character.escape", HighlightKind::StringEscape),
    ("constructor", HighlightKind::Constructor),
    ("embedded", HighlightKind::Embedded),
    ("emphasis", HighlightKind::Emphasis),
    ("text.emphasis", HighlightKind::Emphasis),
    ("markup.italic", HighlightKind::Emphasis),
    ("emphasis.strong", HighlightKind::EmphasisStrong),
    ("text.strong", HighlightKind::EmphasisStrong),
    ("markup.strong", HighlightKind::EmphasisStrong),
    ("markup.bold", HighlightKind::EmphasisStrong),
    ("enum", HighlightKind::Enum),
    ("function", HighlightKind::Function),
    ("method", HighlightKind::Function),
    ("label", HighlightKind::Label),
    ("link_text", HighlightKind::LinkText),
    ("text.reference", HighlightKind::LinkText),
    ("markup.link.label", HighlightKind::LinkText),
    ("markup.link.text", HighlightKind::LinkText),
    ("link_uri", HighlightKind::LinkUri),
    ("text.uri", HighlightKind::LinkUri),
    ("markup.link.url", HighlightKind::LinkUri),
    ("string.special.url", HighlightKind::LinkUri),
    ("number", HighlightKind::Number),
    ("float", HighlightKind::Number),
    ("operator", HighlightKind::Operator),
    ("property", HighlightKind::Property),
    ("field", HighlightKind::Property),
    ("variable.member", HighlightKind::Property),
    ("punctuation", HighlightKind::Punctuation),
    ("punctuation.bracket", HighlightKind::PunctuationBracket),
    ("punctuation.delimiter", HighlightKind::PunctuationDelimiter),
    (
        "punctuation.list_marker",
        HighlightKind::PunctuationListMarker,
    ),
    ("markup.list", HighlightKind::PunctuationListMarker),
    ("punctuation.special", HighlightKind::PunctuationSpecial),
    ("string", HighlightKind::String),
    ("character", HighlightKind::String),
    ("string.escape", HighlightKind::StringEscape),
    ("escape", HighlightKind::StringEscape),
    ("string.regex", HighlightKind::StringRegex),
    ("string.regexp", HighlightKind::StringRegex),
    ("string.special", HighlightKind::StringSpecial),
    ("string.special.symbol", HighlightKind::StringSpecialSymbol),
    ("symbol", HighlightKind::StringSpecialSymbol),
    ("tag", HighlightKind::Tag),
    ("text.literal", HighlightKind::TextLiteral),
    ("markup.raw", HighlightKind::TextLiteral),
    ("title", HighlightKind::Title),
    ("text.title", HighlightKind::Title),
    ("markup.heading", HighlightKind::Title),
    ("type", HighlightKind::Type),
    ("variable", HighlightKind::Variable),
    ("variable.parameter", HighlightKind::VariableParameter),
    ("parameter", HighlightKind::VariableParameter),
    ("variable.builtin", HighlightKind::VariableSpecial),
    ("variable.special", HighlightKind::VariableSpecial),
    ("variant", HighlightKind::Variant),
];
