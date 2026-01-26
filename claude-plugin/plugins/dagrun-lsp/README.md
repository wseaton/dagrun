# dagrun-lsp

Language server for dagrun (.dr) task runner files.

## Features

- **Diagnostics**: Parse errors, undefined variables, undefined tasks, dependency cycles
- **Semantic Highlighting**: Tasks, variables, annotations, interpolations
- **Go-to-definition**: Jump to task/variable declarations
- **Completions**: Task names, variables, annotation keywords

## Installation

### 1. Install the binary

```bash
cd /path/to/justflow
cargo install --path crates/dagrun-lsp
```

### 2. Install the Claude Code plugin

```bash
# From the justflow repo root:
claude plugins install ./claude-plugin
```

### 3. Enable the plugin

```bash
claude plugins enable dagrun-lsp
```

## Supported Extensions

- `.dr`
- `.dagrun`
