-- nvim-treesitter integration for dr
-- WARNING: treesitter is currently disabled due to crashes on malformed syntax
-- just register the filetype for now, enable treesitter at your own risk

-- register filetype (always safe)
vim.filetype.add({
  extension = {
    dr = "dr",
    dagrun = "dr", -- legacy extension support
  },
})

--[[ TREESITTER CONFIG (disabled - crashes on bad syntax)

local parser_config = require("nvim-treesitter.parsers").get_parser_configs()

parser_config.dr = {
  install_info = {
    -- point to local path or github url
    url = "~/git/justflow/tree-sitter-dr", -- or "https://github.com/your-org/tree-sitter-dr"
    files = { "src/parser.c", "src/scanner.c" },
    branch = "main",
    generate_requires_npm = true,
  },
  filetype = "dr",
}

-- ensure queries are found (copy queries/ to your nvim runtime or symlink)
-- option 1: symlink queries to nvim config
--   ln -s ~/git/justflow/tree-sitter-dr/queries ~/.config/nvim/queries/dr

-- option 2: add to runtimepath
vim.opt.runtimepath:append("~/git/justflow/tree-sitter-dr")

-- then install the parser
-- :TSInstall dr
-- or
-- :TSInstallFromGrammar dr

]]
