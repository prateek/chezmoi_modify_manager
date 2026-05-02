# Syntax of configuration files

chezmoi_modify_manager uses configuration files to control how to merge
INI, XML, and plist (Apple property list) files. These are the
`modify_<config_file_name>` files. They can also be templated with
`chezmoi` by naming the file `modify_<config_file_name>.tmpl` instead.
The easiest way to get started is to use `-a` to add a file and generate
a skeleton configuration file. `--add` sniffs the input bytes (`bplist00`
magic, `<?xml ... <plist>`, or other XML) and picks a skeleton tailored
to the detected language.

For an end-to-end walkthrough of the plist backend, see the
[Plist example](./examples/plist.md). The full design is in the
[RFC](./dev/xml_support_rfc.md).

## Syntax

The file consists of directives, one per line. Comments are supported by
prefixing a line with #. Comments are only supported at the start of lines.

> **Note!** If a key appears before the first section, use `<NO_SECTION>` as the
section.

> **Note!** The modify script can itself be a chezmoi template (if it ends with
`.tmpl`), which can be useful if you want to do host specific configuration using
the `set` directive for example.\
\
This however will slow things down every so slightly as chezmoi has to run its
templating engine on the file. Typically, this will be an overhead of about half
a millisecond per templated modify script (measured on an AMD Ryzen 5 5600X).

## Languages

The `language` directive selects which backend processes the file. It
defaults to `ini` when omitted, so existing scripts keep working
unchanged.

```bash
language ini    # default; INI/properties files
language xml    # narrow XPath subset; byte-span patching
language plist  # Apple property list (binary or XML)
```

Plist scripts have two extra directives:

```bash
merge shallow   # default — only top-level keys are replaced
merge deep      # recursive dict merge; arrays and scalars replace
output xml      # encode result as XML plist
output binary   # default — encode result as binary plist (what macOS apps expect)
```

For XML and plist scripts, nodes are addressed by a path string rather
than `(section, key)` pairs. The directive forms are:

```bash
ignore path "<selector>"
remove path "<selector>"
set path "<selector>" "<literal>"
set path "<selector>" <type> "<literal>"
transform path "<selector>" <name> [arg="value"]...
add:hide path "<selector>"
add:remove path "<selector>"
```

The `<type>` tag in `set path` is plist-only and selects the encoding of
the literal that replaces the targeted scalar. Valid tags:

* `string`  — UTF-8 text (`<string>` in XML plist).
* `integer` — base-10 signed integer (`<integer>`).
* `real`    — IEEE-754 floating point (`<real>`).
* `data`    — base64-encoded bytes, written as `<data>`.
* `date`    — ISO-8601 timestamp (`<date>`), e.g. `2025-01-31T12:34:56Z`.

The type tag is **optional**: when the existing scalar's type is
unambiguous, the parser infers it from the value already present in the
source tree and you can write `set path "<selector>" "<literal>"`. Use
the explicit tag when you need to change the type or when the parser
cannot otherwise tell which encoding to emit. The XML backend rejects
type tags entirely.

Plist selector examples:

```bash
ignore path "NSGlobalDomain.AppleLanguages"
ignore path "Accounts[0].Password"
ignore path "Accounts[*].Password"
ignore path "Accounts[name=\"main\"].Password"
ignore path "Accounts[name='main'].Password"
```

Either single or double quotes may delimit a predicate value. The chosen
quote becomes the delimiter; the other quote needs no escape. Use single
quotes inside a `"..."` path string to avoid `\"` escaping.

XML selector examples:

```bash
ignore path "/config/window/@width"
remove path "/config/legacy"
transform path "/gui/Action[@name='open']/@shortcut" ...
set path "/config/title/text()" "My Title"
```

The full selector grammar is in the [RFC](./dev/xml_support_rfc.md). For
available `transform path` names (`join-lines`, `json-encode`,
`data-encode`, `flatten-keys`) see [Transforms](./transforms.md) or run
`chezmoi_modify_manager --help-transforms`.

The remaining directives below apply to INI scripts. Where a directive
also applies to XML/plist scripts in path form, that is noted inline.

## Directives

### source

This directive is required. It specifies where to find the source file
(i.e. the file in the dotfile repo). It should have the following format
to support Chezmoi versions older than v2.46.1:

```bash
source "{{ .chezmoi.sourceDir }}/{{ .chezmoi.sourceFile | trimSuffix ".tmpl" | replace "modify_" "" }}.src.ini"
```

From Chezmoi v2.46.1 and forward the following also works instead:

```bash
source auto
```

> See also: `source auto-path` (recommended for `language xml`/`language
> plist`) and inline (`---`) mode in
> [How chezmoi_modify_manager finds the data file](./source_specification.md).

### ignore

> XML/plist scripts use the `ignore path "<selector>"` form covered
> [above](#languages); the variants below are INI-specific.

Ignore a certain line, always taking it from the target file (i.e. file in
your home directory), instead of the source state. The following variants
are supported:

```bash
ignore section "my-section"
ignore section regex "^MySection.*"
ignore "my-section" "my-key"
ignore regex "section.*regex" "key regex.*"
```

* The first form ignores a whole section (exact literal match).
* The second form ignores a whole section (regex match).
* The third form ignores a specific key (exact literal match).
* The fourth form uses a regex to ignore a specific key.

Prefer the exact literal match variants where possible, they will be
marginally faster.

An additional effect is that lines that are missing in the source state
will not be deleted if they are ignored.

Finally, ignored lines will not be added back when using `--add` or
`--smart-add`, in order to reduce git diffs.

### set

> XML/plist scripts use the `set path "<selector>" [<type>] "<literal>"`
> form covered [above](#languages); the variants below are INI-specific.

Set an entry to a specific value. This is primarily useful together with
chezmoi templates, allowing you to override a specific value for only some
of your computers. The following variants are supported:

```bash
set "section" "key" "value"
set "section" "key" "value" separator="="
```

By default, separator is `" = "`, which might not match what the program that
the ini files belongs to uses.

Notes:

* Only exact literal matches are supported.
* It works better if the line exists in the source & target state, otherwise
  it is likely the line will get formatted weirdly (which will often be
  changed by the program the INI file belongs to).

### remove

> XML/plist scripts use the `remove path "<selector>"` form covered
> [above](#languages); the variants below are INI-specific.

Unconditionally remove everything matching the directive. This is primarily
useful together with chezmoi templates, allowing you to remove a specific
key or section for only some of your computers. The following variants are
supported:

```bash
remove section "my-section"
remove "my-section" "my-key"
remove regex "section.*regex" "key regex.*"
```

(Matching works identically to ignore, see above for more details.)

### transform

> XML/plist scripts use the `transform path "<selector>" <name> [arg=...]`
> form covered [above](#languages); the variants below are INI-specific.

Some specific situations need more complicated merging that a simple
ignore. For those situations you can use transforms. Supported variants
are:

```bash
transform "section" "key" transform-name arg1="value" arg2="value" ...
transform regex "section-regex.*" "key-regex.*" transform-name arg1="value" ...
```

(Matching works identically to ignore except matching entire sections is
not supported. See above for more details.)

For example, to treat `mykey` in `mysection` as an unsorted comma separated
list, you could use:

```bash
transform "mysection" "mykey" unsorted-list separator=","
```

The full list of supported transforms, and how to use them can be listed
using `--help-transforms`.

### add:remove & add:hide

> XML/plist scripts use the `add:remove path "<selector>"` and
> `add:hide path "<selector>"` forms covered [above](#languages); the
> variants below are INI-specific.

These two directives control the behaviour when using --add or --smart-add.
In particular, these allow filtering lines that will be added back to the
source state.

`add:remove` will remove the matching lines entirely. The following forms are
supported:

```bash
add:remove section "section name"
add:remove "section name" "key"
add:remove regex  "section-regex.*" "key-regex.*"
```

(Matching works identically to ignore, see above for more details.)

`add:hide` will instead keep the entries but replace the value associated with
those keys. This is useful together with the keyring transform in particular,
as the key needs to exist in the source or target state for it to trigger
the replacement. The following forms are supported:

```bash
add:hide section "section name"
add:hide "section name" "key"
add:hide regex  "section-regex.*" "key-regex.*"
```

(Matching works identically to ignore, see above for more details.)

### no-warn-multiple-key-matches

This directive quietens warnings on multiple regular expressions matching the
same section+key. While the warning is generally useful, sometimes you might
actually "know what you are doing" and want to suppress it.
