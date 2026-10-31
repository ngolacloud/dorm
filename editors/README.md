# Editor & shell integration

Syntax highlighting for `dorm.schema`, and completions for the `dorm` command,
across the editors and shells you use.

## VS Code — syntax highlighting

The extension in [`vscode/`](vscode) highlights `dorm.schema` (and `*.dorm`)
files. To install it locally without the marketplace:

```sh
cd editors/vscode
npm install -g @vscode/vsce        # once
vsce package                       # produces dorm-schema-0.1.0.vsix
code --install-extension dorm-schema-0.1.0.vsix
```

Or, for development, copy (or symlink) `editors/vscode` into
`~/.vscode/extensions/` and reload VS Code.

## Terminal (bash / sh / zsh) — highlighting with `bat`

[`bat`](https://github.com/sharkdp/bat) is a `cat` with syntax highlighting, used
from any shell. It reads the same TextMate grammar, so the VS Code grammar works
there too:

```sh
mkdir -p "$(bat --config-dir)/syntaxes"
cp editors/vscode/syntaxes/dorm.tmLanguage.json "$(bat --config-dir)/syntaxes/dorm.sublime-syntax"
bat cache --build
bat dorm.schema                    # now highlighted
```

(Prefer a different pager/editor? The grammar is a standard TextMate
`tmLanguage`, which Sublime Text, `bat`, and most editors accept.)

## Shell completion for the `dorm` command

The CLI generates completion scripts for every major shell — tab-complete
subcommands, flags and values:

```sh
# bash
dorm completions bash  | sudo tee /etc/bash_completion.d/dorm > /dev/null
# zsh (a directory on your $fpath)
dorm completions zsh   > ~/.zfunc/_dorm
# fish
dorm completions fish  > ~/.config/fish/completions/dorm.fish
```

`dorm completions <bash|zsh|fish|powershell|elvish>` prints the script to stdout;
redirect it where your shell looks for completions, then start a new shell.
