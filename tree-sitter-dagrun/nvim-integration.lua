-- nvim-treesitter integration for dagrun
-- WARNING: treesitter is currently disabled due to crashes on malformed syntax
-- just register the filetype for now, enable treesitter at your own risk

-- register filetype (always safe)
vim.filetype.add({
  extension = {
    dagrun = "dagrun",
  },
})

--[[ TREESITTER CONFIG (disabled - crashes on bad syntax)

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

-- ensure queries are found (copy queries/ to your nvim runtime or symlink)
-- option 1: symlink queries to nvim config
--   ln -s ~/git/justflow/tree-sitter-dagrun/queries ~/.config/nvim/queries/dagrun

-- option 2: add to runtimepath
vim.opt.runtimepath:append("~/git/justflow/tree-sitter-dagrun")

-- then install the parser
-- :TSInstall dagrun
-- or
-- :TSInstallFromGrammar dagrun

]]
