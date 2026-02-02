; default: task bodies are bash
((task_body) @injection.content
  (#set! injection.language "bash"))

; python shebang detection
((task_body
  (shebang
    interpreter: (shebang_path) @_interp))
  @injection.content
  (#match? @_interp "python")
  (#set! injection.language "python"))

; ruby shebang
((task_body
  (shebang
    interpreter: (shebang_path) @_interp))
  @injection.content
  (#match? @_interp "ruby")
  (#set! injection.language "ruby"))

; node/javascript shebang
((task_body
  (shebang
    interpreter: (shebang_path) @_interp))
  @injection.content
  (#match? @_interp "node")
  (#set! injection.language "javascript"))

; perl shebang
((task_body
  (shebang
    interpreter: (shebang_path) @_interp))
  @injection.content
  (#match? @_interp "perl")
  (#set! injection.language "perl"))

; lua block content
((lua_block
  (lua_content) @injection.content)
  (#set! injection.language "lua"))

; shell expansion in variables
((shell_expansion
  command: (shell_command) @injection.content)
  (#set! injection.language "bash"))
