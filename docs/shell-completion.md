# Shell completion reference

Setup automatically enables completion for the configured user's login shell.
It uses the account database's home and shell, never the terminal application,
`$SHELL`, root's home, or sudo's shell. Start a new shell after setup.

| Login shell | Completion file relative to the account home | Persistent activation |
| --- | --- | --- |
| Zsh | `.local/share/zsh/site-functions/_fin` | Fin-marked `.zshrc` block adds the directory to `fpath`, initializes completion only if needed, and registers `_fin`. |
| Bash | `.local/share/bash-completion/completions/fin` | Fin-marked `.bashrc` block and the first existing `.bash_profile`, `.bash_login`, or `.profile`. Creates `.bash_profile` only if none exists. |
| Fish | `.config/fish/completions/fin.fish` | Fish's normal completion autoloading; no RC edit. |

Blocks are bounded by `# >>> Fin shell completion >>>` and
`# <<< Fin shell completion <<<`. Fin preserves existing content and permissions.
It does not overwrite unrelated completion files or follow symlinked RC files.
Unsupported shells are skipped with a short notice; setup still succeeds.
Nonstandard `ZDOTDIR`/XDG directory configurations can use the custom command below.

The user-owned `.local/share/fin/completion-install.json` records generated files
and exact inserted blocks. Repeating setup does not duplicate them. Uninstall
uses recorded paths even if the login shell changed, removes only unchanged Fin
files and exact Fin blocks, and preserves later user edits. Files created by Fin
are removed when empty after cleanup; preexisting empty RC files are retained.
Modified Fin artifacts and their ownership records are retained and reported.

## Manual or custom integration

The offline generator remains available without installation or accounts:

```sh
fin completions zsh
fin completions bash
fin completions fish
```

Redirect the chosen script to your shell framework's completion directory, or
source it using your existing shell configuration. `--json` returns the stable
`{ "shell": "...", "script": "..." }` object without prompts or decoration.
These commands print scripts and never edit shell configuration.
