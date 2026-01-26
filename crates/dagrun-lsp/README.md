# dagrun-lsp

Language server for `.dr` (dagrun) task runner files.

## Features

- **Diagnostics**: parse errors, undefined variables/tasks, dependency cycles, unused variables, missing files/executables
- **Semantic highlighting**: syntax colors for tasks, variables, annotations, comments
- **Go-to-definition**: jump to task/variable declarations
- **Find references**: find all usages of a task/variable
- **Rename**: rename task/variable across all usages
- **Hover**: documentation for annotations, variable values, task dependencies
- **Completions**: variables, tasks, annotation keywords, annotation options
- **Document symbols**: outline of tasks and variables

## Install

```bash
cargo install --git https://github.com/wseaton/dagrun.git dagrun-lsp
```

## Neovim Setup

Add to your Neovim config (e.g., `~/.config/nvim/lua/plugins/dagrun.lua`):

```lua
-- register filetype
vim.filetype.add({
  extension = {
    dr = "dagrun",
    dagrun = "dagrun",
  },
})

-- start LSP for dagrun files
vim.api.nvim_create_autocmd("FileType", {
  pattern = "dagrun",
  callback = function()
    vim.lsp.start({
      name = "dagrun-lsp",
      cmd = { "dagrun-lsp" },
      root_dir = vim.fn.getcwd(),
    })
  end,
})
```

Or with **lazy.nvim** + **nvim-lspconfig**:

```lua
return {
  "neovim/nvim-lspconfig",
  config = function()
    vim.filetype.add({ extension = { dr = "dagrun", dagrun = "dagrun" } })

    vim.api.nvim_create_autocmd("FileType", {
      pattern = "dagrun",
      callback = function()
        vim.lsp.start({
          name = "dagrun-lsp",
          cmd = { "dagrun-lsp" },
          root_dir = vim.fn.getcwd(),
        })
      end,
    })
  end,
}
```

## Claude Code Setup

Create a plugin marketplace directory and register the LSP:

```bash
mkdir -p ~/.claude/plugins/dagrun-lsp
```

Create `~/.claude/plugins/dagrun-lsp/plugin.json`:

```json
{
  "name": "dagrun-lsp",
  "version": "0.1.0",
  "description": "Language server for dagrun (.dr) task files",
  "author": { "name": "your-name" },
  "lspServers": {
    "dagrun": {
      "command": "dagrun-lsp",
      "extensionToLanguage": {
        ".dr": "dagrun",
        ".dagrun": "dagrun"
      }
    }
  }
}
```

Install the plugin:

```bash
claude plugins install ~/.claude/plugins/dagrun-lsp
```

The LSP will now provide diagnostics and completions when editing `.dr` files in Claude Code.

## Usage

Once installed, open any `.dr` file and the LSP will activate automatically.

**Neovim keybindings** (defaults):
- `gd` - go to definition
- `gr` - find references
- `K` - hover documentation
- `<C-Space>` - trigger completion
- `<leader>rn` - rename symbol
- `<leader>ds` - document symbols (with Telescope)

**Claude Code**:
Diagnostics appear automatically. The LSP icon in the status bar shows when it's active.
