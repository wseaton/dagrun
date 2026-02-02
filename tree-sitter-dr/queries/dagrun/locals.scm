; scopes
(task) @local.scope
(context_block) @local.scope
(lua_block) @local.scope

; definitions
(variable
  name: (identifier) @local.definition.var)
(task
  name: (identifier) @local.definition.function)
(parameter
  name: (identifier) @local.definition.parameter)
(context_block
  name: (identifier) @local.definition.type)

; references
(interpolation
  name: (identifier) @local.reference)
(variable_reference
  name: (identifier) @local.reference)
(dependency
  task: (identifier) @local.reference)
