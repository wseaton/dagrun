-- nvim-treesitter integration for dagrun
-- add this to your neovim config (e.g., after lazy.nvim setup)

local parser_config = require("nvim-treesitter.parsers").get_parser_configs()

parser_config.dagrun = {
  install_info = {
    -- point to local path or github url
    url = "~/git/justflow/tree-sitter-dagrun", -- or "https://github.com/your-org/tree-sitter-dagrun"
    files = { "src/parser.c", "src/scanner.c" },
    branch = "main",
    generate_requires_npm = true,
  },
  filetype = "dagrun",
}

-- register filetype
vim.filetype.add({
  extension = {
    dagrun = "dagrun",
  },
})

-- ensure queries are found (copy queries/ to your nvim runtime or symlink)
-- option 1: symlink queries to nvim config
--   ln -s ~/git/justflow/tree-sitter-dagrun/queries ~/.config/nvim/queries/dagrun

-- option 2: add to runtimepath
vim.opt.runtimepath:append("~/git/justflow/tree-sitter-dagrun")

-- then install the parser
-- :TSInstall dagrun
-- or
-- :TSInstallFromGrammar dagrun
