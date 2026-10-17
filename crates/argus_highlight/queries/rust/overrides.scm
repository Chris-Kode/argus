; Argus additions to the Rust query (Apache-2.0). The last matching pattern
; wins, and the grammar's query lists `SCREAMING_CASE` constants before
; capitalized constructors, so constants would read as constructors.

((identifier) @constant
  (#match? @constant "^[A-Z][A-Z\\d_]+$"))
