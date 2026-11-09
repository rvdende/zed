(struct_name
  (identifier) @type)

(enum_variant) @constant

(struct_entry
  (identifier) @property)

(unit_struct) @type.builtin

(string) @string

(char) @string

(escape_sequence) @string.escape

(boolean) @boolean

[
  (integer)
  (float)
] @number

[
  (line_comment)
  (block_comment)
] @comment

[
  "("
  ")"
  "["
  "]"
  "{"
  "}"
] @punctuation.bracket

[
  ","
  ":"
] @punctuation.delimiter

"-" @operator
