/// <reference types="tree-sitter-cli/dsl" />
// @ts-check

module.exports = grammar({
  name: "dagrun",

  extras: ($) => [/[ \t]/],

  externals: ($) => [$._indent, $._dedent, $._newline, $._string_content],

  rules: {
    source_file: ($) => repeat($._item),

    _item: ($) =>
      choice(
        $.task,
        $.variable,
        $.set_directive,
        $.lua_block,
        $.context_block,
        $.comment,
        $._newline
      ),

    // comments: # or ## (doc comments)
    comment: ($) => seq("#", optional($.comment_text)),
    comment_text: ($) => /[^\n]*/,

    // variable assignment: name := value or name := `shell`
    variable: ($) =>
      seq(
        field("name", $.identifier),
        ":=",
        field("value", choice($.shell_expansion, $.static_value))
      ),

    // static_value must not start with backtick (shell_expansion handles that)
    static_value: ($) => token(prec(-1, /[^\n`][^\n]*/)),

    shell_expansion: ($) =>
      seq("`", field("command", optional($.shell_command)), "`"),
    shell_command: ($) => /[^`\n]+/,

    // set directive: set key := value
    set_directive: ($) =>
      seq("set", field("key", $.identifier), ":=", field("value", $.static_value)),

    // task: name param1 param2="default": dep1, dep2
    task: ($) =>
      seq(
        repeat($.annotation),
        field("name", $.identifier),
        repeat($.parameter),
        ":",
        optional($.dependency_list),
        $._newline,
        optional($.task_body)
      ),

    parameter: ($) =>
      seq(field("name", $.identifier), optional(seq("=", $.parameter_default))),

    parameter_default: ($) =>
      choice($.quoted_string, $.variable_reference, $.bare_value),

    quoted_string: ($) => seq('"', optional(/[^"]+/), '"'),
    // must not start with quote (quoted_string handles that)
    bare_value: ($) => /[^\s:"'][^\s:]*/,

    variable_reference: ($) =>
      seq("{{", field("name", $.identifier), "}}"),

    dependency_list: ($) => seq($.dependency, repeat(seq(optional(","), $.dependency))),

    dependency: ($) =>
      choice(
        field("task", $.identifier),
        seq("service", ":", field("service", $.identifier))
      ),

    // task body: indented lines
    task_body: ($) =>
      seq($._indent, repeat1(seq($._body_line, optional($._newline))), $._dedent),

    _body_line: ($) => choice(prec(2, $.shebang), $.command_line),

    shebang: ($) =>
      seq(
        token(prec(10, "#!")),
        field("interpreter", $.shebang_path),
        optional(field("args", $.shebang_args))
      ),
    shebang_path: ($) => /[^\s\n]+/,
    shebang_args: ($) => /[^\n]+/,

    command_line: ($) => prec.right(repeat1(choice($.command_text, $.interpolation))),

    // match text, but not #! at start (shebang handles that)
    command_text: ($) => choice(/[^\n{#]+/, /#[^!\n][^\n{]*/, /#/, prec(-1, "{")),

    interpolation: ($) =>
      seq("{{", field("name", $.identifier), "}}"),

    // annotations: @timeout 5m, @ssh host=user@host, etc.
    annotation: ($) =>
      seq(
        "@",
        field("name", $.annotation_name),
        optional(field("args", $.annotation_args)),
        $._newline
      ),

    annotation_name: ($) =>
      choice(
        "timeout",
        "retry",
        "join",
        "pipe_from",
        "ssh",
        "upload",
        "download",
        "service",
        "extern",
        "k8s",
        "k8s-configmap",
        "k8s-secret",
        "k8s-upload",
        "k8s-download",
        "k8s-forward",
        "use",
        $.identifier
      ),

    annotation_args: ($) =>
      repeat1(choice($.key_value, $.file_transfer, $.plain_arg)),

    // fallback for annotation args that aren't key=value or file:transfer
    plain_arg: ($) => token(prec(-10, /[^\s\n=:]+/)),

    key_value: ($) =>
      seq(
        field("key", $.identifier),
        "=",
        field("value", choice($.quoted_string, $.key_value_value))
      ),

    key_value_value: ($) => /[^\s\n]+/,

    // file transfer: local_path:remote_path (single token to avoid partial matches)
    file_transfer: ($) => /[^\s=\n]+:[^\s\n]+/,

    // lua block: @lua ... @end
    lua_block: ($) =>
      seq("@lua", $._newline, optional($.lua_content), "@end"),

    lua_content: ($) => repeat1(choice(/[^\n@]+/, prec(-1, "@"), $._newline)),

    // context block: @context name ... @end
    context_block: ($) =>
      seq(
        "@context",
        field("name", $.identifier),
        $._newline,
        repeat($.annotation),
        "@end"
      ),

    identifier: ($) => /[a-zA-Z_][a-zA-Z0-9_-]*/,
  },
});
