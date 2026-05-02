# Plist example: macOS Dock preferences

This walks through managing a macOS preference plist with
`chezmoi_modify_manager`. The Dock (`com.apple.dock`) is a natural
target: it has a handful of widely-known scalar preferences
(`tilesize`, `autohide`, `orientation`) and the OS owns the rest of the
file. The full design is documented in the
[XML and plist RFC](../dev/xml_support_rfc.md).

## Setup

First, get the surrounding chezmoi plumbing in place. Use `--add` to
let the binary sniff the bytes (`bplist00...` or `<plist>`) and emit a
plist-aware skeleton plus a `.src.plist` sidecar:

```bash
chezmoi_modify_manager --add ~/Library/Preferences/com.apple.dock.plist
```

For a typical layout this gives you two files under your chezmoi source
directory:

* `<source-dir>/Library/Preferences/modify_com.apple.dock.plist.tmpl`
  — the modify script.
* `<source-dir>/Library/Preferences/com.apple.dock.plist.src.plist`
  — the sidecar source file (binary or XML plist).

The default skeleton already ships with `language plist` and
`source auto-path`, so keep those lines and replace the rest of the body
with the DSL shown below.

## Vanilla: declare a few Dock preferences

The simplest plist script declares a small set of top-level keys you
want to manage. Everything else in the live plist passes through
unchanged.

```text
#!/usr/bin/env chezmoi_modify_manager

language plist
source auto-path
merge shallow
```

The sidecar source file (`com.apple.dock.plist.src.json` — JSON is
accepted as an authoring alias for plist sidecars) then declares the
keys you care about:

```json
{
  "tilesize": 48,
  "autohide": true,
  "orientation": "left"
}
```

What each line does:

* `language plist` selects the plist backend.
* `source auto-path` reads the sibling source file
  (`<base>.src.{plist,json}`). Single-file mode via `---` is also
  available for very short bodies; see the [RFC](../dev/xml_support_rfc.md).
* `merge shallow` is the default: for every top-level key in source,
  set live[key] := source[key]. Keys present only in live pass through
  untouched. Use `merge deep` for a recursive dict merge when both
  sides have nested dicts you want combined.

The output is a binary plist by default, matching what `cfprefsd`
expects. Add `output xml` if you specifically want a human-readable
on-disk artifact; note that macOS apps usually rewrite the file as
binary on next save, producing diff churn each apply.

> **Note!** If your app stores the file on disk as binary plist (most
macOS apps do), omit `output xml` so the first `chezmoi apply` does not
flip the format on you.

## Transforms: reshape during merge

Some apps store data in plist in shapes that are hostile to author by
hand: a list-of-strings stored as a single newline-joined `<string>`,
or a structured value stored as a JSON-encoded `<string>`. Transforms
run on the source side before the merge and reshape one logical shape
to another.

For example, suppose a hypothetical editor stores its include-paths
preference as a single newline-joined string. You would rather author
the value as a list, so use `transform path` with `join-lines`:

```text
#!/usr/bin/env chezmoi_modify_manager

language plist
source auto-path
merge shallow

transform path "includePaths" join-lines
```

```json
{
  "includePaths": [
    "/usr/local/include",
    "/opt/homebrew/include"
  ],
  "tabSize": 4
}
```

At process time the source side becomes
`{"includePaths": "/usr/local/include\n/opt/homebrew/include",
"tabSize": 4}`, which is what the live plist expects.

The full set of plist transforms (`join-lines`, `json-encode`,
`data-encode`, `flatten-keys`) is in
[Transforms](../transforms.md#plist-transforms); run
`chezmoi_modify_manager --help-transforms` for the most up-to-date
list.
