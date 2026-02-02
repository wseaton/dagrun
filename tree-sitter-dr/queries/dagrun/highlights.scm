; keywords and operators
":=" @operator
":" @punctuation.delimiter
"," @punctuation.delimiter
"@" @punctuation.special
"{{" @punctuation.special
"}}" @punctuation.special
"=" @operator
"`" @punctuation.special
"#!" @keyword.directive
"set" @keyword

; comments
(comment) @comment
(comment_text) @comment

; variables
(variable
  name: (identifier) @variable.definition)
(static_value) @string
(shell_expansion
  command: (shell_command) @string.special)

; set directive
(set_directive
  key: (identifier) @property
  value: (_) @string)

; tasks
(task
  name: (identifier) @function.definition)

; parameters
(parameter
  name: (identifier) @variable.parameter)
(parameter_default) @string
(quoted_string) @string

; dependencies
(dependency
  task: (identifier) @function.call)
(dependency
  service: (identifier) @type)
"service" @keyword

; annotations
(annotation
  name: (annotation_name) @attribute)
(annotation_name) @attribute

; annotation arguments
(key_value
  key: (identifier) @property)
(key_value
  value: (_) @string)
; file transfer is now a single token (local:remote pattern)
(file_transfer) @string.special.path

; shebang in task body
(shebang
  interpreter: (shebang_path) @keyword.directive)
(shebang_args) @string

; interpolation
(interpolation
  name: (identifier) @variable)
(variable_reference
  name: (identifier) @variable)

; lua block
"@lua" @keyword
"@end" @keyword
(lua_content) @none

; context block
"@context" @keyword
(context_block
  name: (identifier) @type.definition)
