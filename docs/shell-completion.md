# Shell completion reference

Setup automatically enables completion for the configured user's login shell.
It uses the account database's home and shell, never the terminal application,
`$SHELL`, root's home, or sudo's shell. Start a new shell after setup.

| Login shell | Completion file relative to the account home | Persistent activation |
| --- | --- | --- |
| Zsh | `.local/share/zsh/site-functions/_gofin` | GoFin-marked `.zshrc` block adds the directory to `fpath`, initializes completion only if needed, and registers `_gofin`. |
| Bash | `.local/share/bash-completion/completions/gofin` | GoFin-marked `.bashrc` block and the first existing `.bash_profile`, `.bash_login`, or `.profile`. Creates `.bash_profile` only if none exists. |
| Fish | `.config/fish/completions/gofin.fish` | Fish's normal completion autoloading; no RC edit. |

Blocks are bounded by `# >>> GoFin shell completion >>>` and
`# <<< GoFin shell completion <<<`. GoFin preserves existing content and permissions.
It does not overwrite unrelated completion files or follow symlinked RC files.
Unchanged completion files and blocks from the former `fin` command migrate during
completion setup. Edited legacy files or blocks are retained for review. The
ownership record keeps its legacy path to preserve that history.
Unsupported shells are skipped with a short notice; setup still succeeds.
Nonstandard `ZDOTDIR`/XDG directory configurations can use the custom command below.

The user-owned `.local/share/fin/completion-install.json` records generated files
and exact inserted blocks. Repeating setup does not duplicate them. Uninstall
uses recorded paths even if the login shell changed, removes only unchanged GoFin
files and exact GoFin blocks, and preserves later user edits. Files created by GoFin
are removed when empty after cleanup; preexisting empty RC files are retained.
Modified GoFin artifacts and their ownership records are retained and reported.

## Manual or custom integration

The offline generator remains available without installation or accounts:

```sh
gofin completions zsh
gofin completions bash
gofin completions fish
```

Redirect the chosen script to your shell framework's completion directory, or
source it using your existing shell configuration. `--json` returns the stable
`{ "shell": "...", "script": "..." }` object without prompts or decoration.
These commands print scripts and never edit shell configuration.
