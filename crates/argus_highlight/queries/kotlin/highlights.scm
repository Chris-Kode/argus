; Kotlin highlights for the `tree-sitter-kotlin-ng` grammar.
;
; Written for Argus (Apache-2.0): the grammar crate ships no queries, and the
; older `tree-sitter-kotlin` queries target another grammar. When several
; patterns match one node the last one wins, so generic patterns come first
; and specific ones after.

; Identifiers (generic)

(identifier) @variable

(variable_declaration
  (identifier) @variable)

(navigation_expression
  (identifier) @property .)

(user_type
  (identifier) @type)

; Declarations

(package_header
  (qualified_identifier) @namespace)
(import
  (qualified_identifier) @namespace)

(class_declaration
  name: (identifier) @type)
(object_declaration
  name: (identifier) @type)
(companion_object
  name: (identifier) @type)
(type_alias
  type: (identifier) @type)
(type_parameter
  (identifier) @type)
(enum_entry
  (identifier) @constant)

(class_parameter
  (identifier) @property)
(parameter
  (identifier) @variable.parameter)
(lambda_parameters
  (variable_declaration
    (identifier) @variable.parameter))

(function_declaration
  name: (identifier) @function)
(call_expression
  (identifier) @function)
(call_expression
  (navigation_expression
    (identifier) @function.method .))

(label) @label

; Annotations

(annotation
  "@" @attribute
  (_) @attribute)
(file_annotation
  "@" @attribute)
(use_site_target) @attribute

; Builtin receivers and literals spelled as identifiers

(this_expression) @variable.special
(super_expression) @variable.special
((identifier) @variable.special
  (#any-of? @variable.special "it" "field"))
((identifier) @boolean
  (#any-of? @boolean "true" "false"))
((identifier) @constant.builtin
  (#eq? @constant.builtin "null"))

; Keywords

[
  "package"
  "import"
  "class"
  "interface"
  "object"
  "companion"
  "fun"
  "val"
  "var"
  "typealias"
  "constructor"
  "init"
  "by"
  "get"
  "set"
  "where"
  "if"
  "else"
  "when"
  "for"
  "while"
  "do"
  "try"
  "catch"
  "finally"
  "throw"
  "return"
  "return@"
  "in"
  "!in"
  "is"
  "!is"
  "as"
  "as?"
  "this@"
  "super@"
  "enum"
  "data"
  "sealed"
  "annotation"
  "inner"
  "value"
  "abstract"
  "final"
  "open"
  "override"
  "lateinit"
  "const"
  "suspend"
  "inline"
  "noinline"
  "crossinline"
  "tailrec"
  "operator"
  "infix"
  "external"
  "vararg"
  "public"
  "private"
  "protected"
  "internal"
  "expect"
  "actual"
  "out"
  "dynamic"
] @keyword

(reification_modifier) @keyword

; Operators and punctuation

[
  "!" "!!" "!=" "!==" "%" "%=" "&&" "*" "*=" "+" "++" "+=" "-" "--" "-="
  "->" ".." "..<" "/" "/=" "<" "<=" "=" "==" "===" ">" ">=" "?:" "||" "::"
] @operator

["(" ")" "[" "]" "{" "}"] @punctuation.bracket
["," "." "?." ";" ":"] @punctuation.delimiter

; Literals and comments last: nothing inside them is code.

(number_literal) @number
(float_literal) @number
(string_literal) @string
(multiline_string_literal) @string
(character_literal) @string
(escape_sequence) @string.escape
(interpolation
  "${" @punctuation.special
  "}" @punctuation.special)
(interpolation
  "$" @punctuation.special)

(line_comment) @comment
(block_comment) @comment
(shebang) @comment
