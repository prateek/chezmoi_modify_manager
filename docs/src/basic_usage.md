# Basic Usage

## Theory of operation

For each settings file you want to manage with `chezmoi_modify_manager` there
will be two files in your chezmoi source directory:

* `modify_<config file>` or `modify_<config file>.tmpl`, e.g. `modify_private_kdeglobals.tmpl` \
  This is the modify script/configuration file that calls `chezmoi_modify_manager`.
  It contains the directives describing what to ignore.
* `<config file>.src.ini`, e.g. `private_kdeglobals.src.ini`\
  This is the source state of the INI file.

The `modify_` script is responsible for generating the new state of the file
given the current state in your home directory. The `modify_` script is set
up to use `chezmoi_modify_manager` as an interpreter to do so.
`chezmoi_modify_manager` will read the modify script to read configuration and
the `.src.ini` file and by default will apply that file exactly (ignoring blank
lines and comments).

However, by giving additional directives to `chezmoi_modify_manager` in the
`modify_` script you can tell it to ignore certain sections (see
`chezmoi_modify_manager --help-syntax` for details). For example:

```bash
ignore "KFileDialog Settings" "Show Inline Previews"
ignore section "DirSelect Dialog"
```

will tell it to ignore the key `Show Inline Previews` in the section
`KFileDialog Settings` and the entire section `DirSelect Dialog`. More on
this [below](#configuring-filters).

## Adding files

> Always refer to `chezmoi_modify_manager --help` for the *most* up-to-date details
that matches the version you are using.

There are two modes to add files in:

* `-s`/`--smart-add`: Smart re-add mode that re-adds files as managed `.src.ini`
  if they are already managed, otherwise adds with plain chezmoi.
* `-a`/`--add`: This adds or converts from plain chezmoi to managed `.src.ini`.

`--add` (and `--smart-add`) sniff the input bytes and pick the matching
sidecar extension for you: a binary or XML plist becomes `.src.plist`,
a non-plist XML file becomes `.src.xml`, JSON-shaped input intended for
the plist backend becomes `.src.json`, and everything else falls back to
`.src.ini`. The skeleton script generated alongside the sidecar is
selected to match. See [Other backends](#other-backends) below for the
non-INI languages.

Here are some examples:

```bash
# Add configs to be handled by chezmoi_modify_manager (or convert configs
# managed by chezmoi to be managed by chezmoi_modify_manager).
chezmoi_modify_manager --add ~/.config/kdeglobals ~/.config/kwinrc

# Re-add config after changes in the live system.
chezmoi_modify_manager --add ~/.config/kdeglobals

# Don't remember if chezmoi_modify_manager handles the file or if it is raw chezmoi?
# Use smart mode (-s/--smart-add) to update the file!
chezmoi_modify_manager --smart-add ~/.config/PrusaSlicer/PrusaSlicer.ini
```

In addition, you can control *re*adding behaviour with some settings in the
`modify_<config file>`, to filter out entries while readding. This is covered
in the [next chapter](configuration_files.md).

## Configuring filters

The file `modify_<config file>` (or `modify_<config file>.tmpl` if you wish
to use chezmoi templating) contain the control directives for that config
file which controls the behaviour for `chezmoi apply` of those files (as well as
when readding files from the system). The full details on this file are in the
[next chapter](configuration_files.md), this section just covers the basics.

A basic such file will have this structure:

```bash
#!/usr/bin/env chezmoi_modify_manager

source auto

# This is a comment
# The next line is a directive. Directives are delimited by newlines.
ignore "SomeSection" "SomeKey"

ignore section "An entire section that is ignored"

ignore regex "Some Sections .*" "A key regex .*"

ignore section regex "^AllSectionsStartingWithThisString.*"
```

This illustrates some basics:

* The first line needs to be a `#!` that tells the OS that `chezmoi_modify_manager`
  should be the interpreter for this file. (This still works on Windows because
  `chezmoi` handles that internally as far as I understand, though I don't use
  Windows myself.)
* The `source` directive tells `chezmoi_modify_manager` where to look for the
  `.src.ini` file. As of chezmoi 2.46.1 this can be auto-detected. If you use an
  older version, `chezmoi_modify_manager --add` will detect that and insert the
  appropriate template based line instead.
* The `ignore` directive is the most important directive. It has two effects:
  * When running `chezmoi apply` it results in the matching entries from
    `.src.ini` being ignored, and the current system state is used instead.
  * When running `chezmoi_modify_manager --add` (or `--smart-add`) it results
    in not copying matching entries to the `.src.ini` to begin with.

There are several other directives as well, here is a basic rundown of them,
they are covered in more detail in the [next chapter](configuration_files.md). Here
is a short summary:

* `set`: Sets an entry to a specific value. Useful together with chezmoi templating.
* `remove`: Remove a specific entry, also useful together with chezmoi templating.
* `transform`: Apply a custom transformation to the value. Can be used to handle
  some hard to deal with corner cases, supported transforms are covered in a
  [later chapter](transforms.md).
* `add:hide` & `add:remove`: Useful together with certain transforms to control
  re-adding behaviour.
* `no-warn-multiple-key-matches`: If there are multiple regex rules that overlap
  a warning will be issued. You can use this directive to quieten those warnings
  if this is intentional. See [action evaluation order](actions.md#order-of-action-matching)
  for more information on this.

## Other backends

The discussion above is INI-specific (the historic default). You can
also manage XML and Apple plist sources by adding a `language` directive
to the top of the modify script. `language` defaults to `ini` when
omitted, so existing scripts keep working unchanged.

```bash
language ini    # default; INI/properties files (`.src.ini`)
language xml    # XML; `.src.xml`
language plist  # Apple property list (binary or XML); `.src.plist` or `.src.json`
```

Source-file naming follows the language:

* `language ini`   reads `<base>.src.ini` next to the modify script.
* `language xml`   reads `<base>.src.xml`.
* `language plist` reads `<base>.src.plist` (binary or XML plist) **or**
  `<base>.src.json` (decoded as JSON, then merged). It is an error for
  both `<base>.src.plist` and `<base>.src.json` to exist alongside the
  same script.

For these backends, prefer `source auto-path`, which lets the binary
pick the language-specific sidecar extension. A single-file mode is
also available: place a `---` divider in the script and put the source
body below it. This is useful for short inline XML or plist fragments.

`--add` sniffs the input bytes and writes a language-appropriate
skeleton (including the matching `language` and `source auto-path`
lines), so you usually don't have to wire any of this up by hand.

For an end-to-end walkthrough see the
[Plist example](./examples/plist.md), and for the per-language
directive forms see [Syntax of configuration files](./configuration_files.md).
