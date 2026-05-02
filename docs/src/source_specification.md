# `source`: How chezmoi_modify_manager finds the data file

## Background

chezmoi_modify_manager needs three inputs to work:

* The modify script with directives (ignores, transforms, etc)
* The state of the config file in your home directory
* The source state of the config file.

The first two are provided by chezmoi, no issues. But as far as chezmoi is
concerned, the modify script itself is the source state. As such we need
an alternative mechanism.

## Problem

The obvious solution would be a path relative to the modify script. However,
chezmoi always copies the modify script to a temporary directory before executing
it, even if the modify script isn't templated. So this doesn't work. (It is however
used internally in the test suite of chezmoi_modify_manager using
`source auto-path`, which might be relevant if you are working on the
chezmoi_modify_manager codebase itself.)

Prior to chezmoi 2.46.1, we had to rely on making the modify script a template,
as chezmoi didn't expose enough information to us (see
[this chezmoi issue](https://github.com/twpayne/chezmoi/issues/2934) for more
historical details). Basically we can make chezmoi find the source file for us
using the following line:

```bash
source "{{ .chezmoi.sourceDir }}/{{ .chezmoi.sourceFile | trimSuffix ".tmpl" | replace "modify_" "" }}.src.ini"
```

Since chezmoi 2.46.1, chezmoi now provides us with two environment variables:

* `CHEZMOI_SOURCE_DIR`: Path to the source directory root
* `CHEZMOI_SOURCE_FILE`: Path to our modify script (relative the source directory root)

With these two together we no longer need templating, and the following works:

```bash
source auto
```

## What the code does

Since chezmoi_modify_manager 3.1, it will auto-detect the version of chezmoi
(based on executing `chezmoi --version`). This is used for:

* The template that `--add` creates to either use the templated source string or
  the simpler `source auto`.
* Interpreting the meaning of `--style=auto` (default value for style) to either
  create a templated modify script or a non-templated modify script.

The main benefit of the simpler `source auto` is that if your modify script
*doesn't need* to be a template for any other reason, it will speed up execution,
as chezmoi no longer needs to run its template engine.

### Per-language source files

The discussion above is INI-specific. The XML and plist backends work
the same way but pick a different sidecar extension based on the
script's `language` directive. For the new backends, prefer
`source auto-path`:

```bash
language xml
source auto-path     # reads `<base>.src.xml`
```

```bash
language plist
source auto-path     # reads `<base>.src.plist` or `<base>.src.json`
```

The resolution rules are:

* `language ini`   → `<base>.src.ini` (default; the same thing the
  historic `source auto` form has always done).
* `language xml`   → `<base>.src.xml`.
* `language plist` → `<base>.src.plist` (binary or XML plist) **or**
  `<base>.src.json` (a JSON document, decoded then merged into the live
  plist tree). If both `<base>.src.plist` and `<base>.src.json` are
  present alongside the same script the run aborts with an error — pick
  one.

`source auto-path` differs from `source auto` only in that it derives
the sibling filename from the modify script's name rather than relying
on `CHEZMOI_SOURCE_*` envs. It works on every chezmoi version that
exposes the script path on disk.

### Inline (single-file) mode

For short or tightly-coupled sources you can skip the sidecar entirely
and embed the source body inline using a `---` divider:

```bash
#!/usr/bin/env chezmoi_modify_manager
language plist
merge shallow
output xml
---
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" ...>
<plist version="1.0">
  <dict>...</dict>
</plist>
```

Everything above the first `---` line is parsed as directives; everything
below is the source body, fed straight to the backend. The body's
encoding must match the active `language` (XML for `language xml` and
`language plist`; INI text for `language ini`).

When inline mode is in effect the `source` directive is omitted — any
form of it (`source "..."`, `source auto`, `source auto-path`) is
mutually exclusive with `---`.

### Overriding auto detection

Auto-detection has one downside though: What if you use multiple versions of
chezmoi (such as an old version from Debian stable on some server but an up-to-date
version on your personal computer). In that case you don't want to use the newer
syntax for compatibility reasons.

The workaround is to export an environment
variable `CHEZMOI_MODIFY_MANAGER_ASSUME_CHEZMOI_VERSION` set to the oldest
version that you use. E.g:

```bash
CHEZMOI_MODIFY_MANAGER_ASSUME_CHEZMOI_VERSION=2.46.0
```

This could be set in your `.bashrc`/`.zshrc`/`.profile` or similar file (the
details of how to best set environment variables for a particular platform and
shell is out of scope of this documentation).
