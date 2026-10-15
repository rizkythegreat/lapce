# Tailwind CSS autocomplete

The built-in Tailwind CSS integration starts a language server when a workspace
has an open HTML, JavaScript/JSX, TypeScript/TSX, Vue, Svelte, Astro, CSS, SCSS,
Less, PHP, or Handlebars file. It uses the existing completion, hover, and
diagnostics UI alongside other language plugins.

Install Node.js and the official language server:

```sh
npm install -g @tailwindcss/language-server
```

Alternatively, install it in your web project:

```sh
npm install --save-dev @tailwindcss/language-server
```

Lapce searches the workspace and its parent directories for
`node_modules/@tailwindcss/language-server/bin/tailwindcss-language-server`,
then searches PATH for the global installation. Node.js must also be on the
editor's PATH. When launched from the desktop, Lapce imports the environment
from an interactive login shell so version managers initialized in `.zshrc`
or `.bashrc`, including NVM, are available to language servers.

Open your project as a workspace, with Tailwind CSS installed.
Tailwind v3 projects need a `tailwind.config.*` file; Tailwind v4 projects need
a CSS entry point importing Tailwind, such as `@import "tailwindcss";`.

Type a class name inside `class="..."` or `className="..."` to get suggestions.
The editor's manual completion command can also request suggestions before
you type a class name. The server discovers project configuration and watches
files itself.

For a custom installation, put the JavaScript entry point in Lapce's
`settings.toml` (a relative path is resolved from the workspace):

```toml
[tailwindcss]
enabled = true
server-path = "/absolute/path/to/@tailwindcss/language-server/bin/tailwindcss-language-server"

[tailwindcss.settings.tailwindCSS]
classAttributes = ["class", "className", "ngClass", "class:list"]
```

Reload the workspace after installing the server or changing these settings.
Startup errors appear in Lapce's logs with installation instructions. Set
`enabled = false` if you use a separate Tailwind Volt, to avoid duplicate
servers.

If an older build reports that the server cannot be found despite a successful
global installation, quit Lapce and start its executable from a terminal where
`node` and `tailwindcss-language-server` are available. For the custom macOS app:

```sh
"/Applications/Lapce Split.app/Contents/MacOS/lapce" --wait
```

This gives that build the terminal's PATH without changing your shell settings.

If automatic discovery scans archived projects or unrelated CSS entry points,
select the active Tailwind project in the workspace's `.lapce/settings.toml`:

```toml
[tailwindcss.settings.tailwindCSS.experimental]
configFile = "app/globals.css"
```

Use your project's CSS entry point for Tailwind v4, or its `tailwind.config.js`
for Tailwind v3, then reload the workspace.

Run the integration's Rust unit tests with:

```sh
cargo test -p lapce-proxy plugin::tailwind::tests
```

Official server documentation:
<https://github.com/tailwindlabs/tailwindcss-intellisense/tree/main/packages/tailwindcss-language-server>.
