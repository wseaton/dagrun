# tree-sitter-dagrun

Tree-sitter grammar for dagrun task files (`.dagrun`). Provides syntax highlighting with nested language injection for task bodies.

## Features

- Syntax highlighting for dagrun-specific constructs (tasks, annotations, variables)
- Nested highlighting via language injection:
  - Bash (default for task bodies)
  - Python, Ruby, JavaScript, Perl (via shebang detection)
  - Lua (for `@lua` blocks)
- Text objects for task selection
- Code folding support

## Neovim Setup

### 1. Install the parser

Add to your nvim-treesitter config:

```lua
local parser_config = require("nvim-treesitter.parsers").get_parser_configs()

parser_config.dagrun = {
  install_info = {
    url = "https://github.com/justflow/tree-sitter-dagrun", -- or local path
    files = { "src/parser.c", "src/scanner.c" },
    branch = "main",
    generate_requires_npm = true,
  },
  filetype = "dagrun",
}

-- register the filetype
vim.filetype.add({
  extension = {
    dagrun = "dagrun",
  },
})
```

Then run `:TSInstall dagrun` or `:TSInstallFromGrammar dagrun`.

### 2. Install queries

The queries need to be in nvim's runtime path. Either:

**Option A: Symlink**
```bash
ln -s /path/to/tree-sitter-dagrun/queries/dagrun ~/.config/nvim/queries/dagrun
```

**Option B: Add to runtimepath**
```lua
vim.opt.runtimepath:append("/path/to/tree-sitter-dagrun")
```

### 3. Manual parser compilation (alternative)

If TSInstall doesn't work:

```bash
cd tree-sitter-dagrun
cc -shared -fPIC -o dagrun.so src/parser.c src/scanner.c -I src
cp dagrun.so ~/.local/share/nvim/lazy/nvim-treesitter/parser/
```

## LSP Integration

The `dagrun-lsp` binary provides diagnostics, go-to-definition, and completions.

### Neovim lspconfig setup

```lua
local lspconfig = require("lspconfig")
local configs = require("lspconfig.configs")

if not configs.dagrun then
  configs.dagrun = {
    default_config = {
      cmd = { "dagrun-lsp" },
      filetypes = { "dagrun" },
      root_dir = lspconfig.util.root_pattern(".git", "*.dagrun"),
      single_file_support = true,
    },
  }
end

lspconfig.dagrun.setup({})
```

Make sure `dagrun-lsp` is in your PATH (install via `cargo install --path crates/dagrun-lsp`).

## Development

```bash
# generate parser
npm install && npx tree-sitter generate

# test grammar
npx tree-sitter test

# test highlighting on a file
npx tree-sitter highlight example.dagrun
```

Or use the tasks in `tasks.dagrun`:

```bash
dagrun run generate
dagrun run test
dagrun run highlight file=example.dagrun
```
