; text objects for nvim-treesitter-textobjects

; @function.outer / @function.inner - tasks
(task) @function.outer
(task
  (task_body) @function.inner)

; @block.outer / @block.inner - lua and context blocks
(lua_block) @block.outer
(lua_block
  (lua_content) @block.inner)

(context_block) @block.outer

; @parameter.outer / @parameter.inner
(parameter) @parameter.outer
(parameter
  name: (identifier) @parameter.inner)

; @comment.outer
(comment) @comment.outer
