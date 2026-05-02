# RFC: XML and plist support

Status: draft

Related issues:

* [#98: Support for separate source files (revisit)](https://github.com/VorpalBlade/chezmoi_modify_manager/issues/98)
* [#131: JSON](https://github.com/VorpalBlade/chezmoi_modify_manager/issues/131)

## Summary

Add two new backends:

* An **XML** backend for configuration-style XML files (KDE, KXMLGUI). Uses
  byte-span patching to preserve source formatting.
* A **plist** backend for Apple property list files. Accepts XML plist,
  binary plist, or JSON on input; emits binary plist by default. Uses
  logical merge rather than byte-span patching, because macOS applications
  rewrite formatting routinely.

INI remains the default; current behaviour is unchanged when no language is
selected.

The first useful slice should support, per backend:

* Selecting one node by an absolute path expression.
* Preserving selected values from the system file during merge.
* Hiding or removing selected values during re-add.
* Resolving `source auto` to the backend-specific source extension.
* A **single-file mode** where the modify script and its source data live
  in one file separated by a `---` divider, alongside the existing
  sidecar mode.
* A **default merge** that does the obvious thing for the common
  preference-management pattern: shallow top-level override on plist
  (source wins for declared keys; system preserves the rest). Most
  plist modify scripts then need zero directives — the `language plist`
  line and the data block are enough.
* **Transforms** for shape adapters that real apps demand (`join-lines`,
  `json-encode`, `flatten-keys`).

This RFC deliberately avoids a general XML editor or a full plist toolkit. XML
is more complicated than INI: elements do not always have unique keys, text can
be a sibling of elements, and applications may rewrite formatting. Plist adds
the further wrinkle that its logical shape is a dict/array tree even though the
on-disk form is XML or binary — selectors that work for KDE configuration files
would be hostile for plist.

## Motivation

chezmoi_modify_manager currently handles INI-style files. Issue #131 documents
the main requirement for any new backend: output must preserve formatting and
other trivia, otherwise normal `chezmoi diff` output becomes noisy.

Issue #98 adds a second constraint. Applying a file can rely on chezmoi template
processing, but re-adding cannot fully recreate chezmoi's template context. The
backend therefore needs a direct filtering path for live files, just as INI has
today.

KDE XML files and macOS plist files are the two likely first targets. They
share the directive vocabulary, the language-dispatch boundary, the
single-file mode, and the transform pipeline, so they belong in one RFC.
They diverge below that line: XML uses byte-span patching for formatting
preservation, plist uses logical merge because the OS rewrites it
anyway.

## Goals

* Keep current INI behaviour unchanged when no language is selected.
* Preserve source formatting outside edited spans for the XML backend.
* For the plist backend, accept any of XML plist, binary plist, or JSON on
  input and write binary plist by default; logical preservation matters,
  byte preservation does not.
* Make the common preference-management pattern — "I have a JSON
  description of the keys I want to manage; merge them onto the live
  plist" — express in a five-line single-file modify script with no
  directives beyond `language` and `merge`.
* Apply chezmoi-managed updates to macOS preference plists without
  fighting `cfprefsd`.
* Express common preference-management patterns (override a small set
  of keys; preserve everything else; sanitise secrets on re-add)
  without writing a custom modify script per app.
* Match plist conventions where they differ from INI.
* Support merge and re-add flows for both backends before calling them
  complete.
* Use a path grammar per backend that matches the backend's data model:
  an XPath subset for XML, a dict/array key-path for plist.
* Provide transform primitives that adapt human-friendly authoring
  shapes to the runtime shape an app expects.
* Produce clear errors when a selector is ambiguous or unsupported.
* Leave room for a future JSON-on-disk backend to reuse the same dispatch,
  key-path grammar, and merge defaults.

## Non-goals

* Full XPath, XQuery, XSLT, XML schema validation, or DTD validation.
* Namespace-aware selectors in the first XML implementation.
* Editing mixed content.
* Byte-for-byte preservation of plist files. macOS rewrites them
  routinely; the plist backend optimises for stable *logical* output, not
  stable bytes. Diffs will be noisy when the owning application reformats.
* Wildcards, slices, recursive descent, or predicates beyond what the
  grammar declares.
* Guaranteeing stable diffs for any backend when the owning application
  reformats the whole file.

## Languages and configuration

The `language` directive selects the backend, the file syntax it parses, and
the path grammar used by matchers:

* `language ini` (default if omitted) — existing behaviour.
* `language xml` — generic XML configuration files.
* `language plist` — Apple property list files in XML plist format.

Each backend defines its own path grammar. Mixing matchers across languages is
rejected at config-parse time.

### XML

```text
language xml
source auto

ignore path "/gui/ActionProperties/Action[@name=\"open\"]/@shortcut"
add:hide path "/config/account[@name=\"main\"]/@password"
add:remove path "/gui/ActionProperties/Action[@name=\"recent\"]"
remove path "/gui/ActionProperties/Action[@name=\"volatile\"]"
```

### Plist

```text
language plist
source auto

ignore path "NSGlobalDomain.AppleLanguages[0]"
add:hide path "Accounts[0].Password"
add:remove path "RecentDocuments"
remove path "\"com.apple.dock\".persistent-apps"
```

## Single-file mode

A modify script can carry its source data inline. The first `---` line at
column 0 separates configuration (above) from data (below):

```text
#!/usr/bin/env chezmoi-modify-manager
language plist
merge shallow
---
{
  "tilesize": 48,
  "autohide": true,
  "orientation": "left"
}
```

The shebang makes chezmoi run the modify file as an executable; the binary
reads its own argv path, splits on the first column-0 `---`, and treats the
body as the source.

Format detection on the body:

* `language plist`: first non-whitespace `{` or `[` → JSON; `<` → XML
  plist. Binary plist is not allowed inline (not human-editable).
* `language xml`: body must be XML.
* `language ini`: body is INI.

### Inline vs sidecar

The two modes are picked by the presence of a `---` divider:

* **Inline** (divider present): the body is the source. `source auto` is
  implicit; an explicit `source` directive is rejected.
* **Sidecar** (no divider): the existing two-file mode. `source auto`
  resolves a sibling `.src.<ext>` file. `source "..."` overrides the
  sibling lookup.

Both modes use the same DSL, the same backends, and the same merge
algorithm. Authors pick per-app: a small preferences plist with a
handful of declared keys is a natural inline file; a large preferences
plist with hundreds of keys is better as a sidecar.

Defensive parsing rules:

* Only the *first* `---` at column 0 is the divider; subsequent ones in
  the body are content.
* chezmoi templating runs before the binary parses the file. Templates
  that produce a `---` line in the configuration region are rejected
  with a parse error naming the line.
* In sidecar mode, a stray `---` line in the modify script is a parse
  error. Authors switching modes must remove the divider explicitly.

## Default merge

Each backend defines a default merge for the case where the modify script
declares no path-level directives. The default has to be useful on its
own, because the common preference-management pattern (shipping a
desired set of top-level keys onto a live plist that the OS owns most
of) is a single declarative operation, not a list of selectors.

* `language ini`: existing INI merge behaviour.
* `language xml`: no implicit merge. Without a directive, the source XML
  is written unchanged. XML files are heterogeneous enough that there is
  no single useful default.
* `language plist` with `merge shallow` (default): for every top-level
  key in source, set live[key] := source[key]. Keys present only in live
  pass through unchanged. Keys present only in source are appended to
  live in source order. No removals.
* `language plist` with `merge deep`: same as shallow at the top level,
  except that when *both* sides have a dict at the same key, the merge
  recurses. Arrays at any depth replace wholesale (array-element merge
  is ambiguous and out of scope). Scalars at any depth override.

`merge` is selected with a top-level directive:

```text
language plist
merge deep
```

Path-level directives (`ignore`, `add:hide`, `add:remove`, `remove`,
`set`) layer on top of the default and override it for their selectors.
A script with only `language plist` + `merge shallow` and no other
directives is valid and minimal — the source data block does the work.

The choice of `shallow` as the default is conservative: nothing in
source's nested structures touches existing nested structures in live.
`merge deep` is the more intuitive choice for files whose top-level
keys are themselves large dicts you want to combine; authors opt in
explicitly.

## Transforms

Some apps store data in plist in shapes that are hostile to author by
hand: newline-joined string blobs, JSON-encoded payloads inside
`<string>` values, flattened key namespaces. Transforms run between
parsing the source data and applying the merge, and adapt one logical
shape to another.

```text
language plist
merge shallow

transform path "browserHostWhitelist"  join-lines
transform path "sidebar"                json-encode
transform path "shortcuts"              flatten-keys prefix="shortcut." json-encode-values
```

First-cut primitives, chosen to fit observed real-world cases:

* `join-lines` — array of strings → single string with `\n` separators.
* `json-encode` — any value → UTF-8 string holding canonical JSON.
* `data-encode` — any value → `<data>` blob containing the UTF-8 JSON
  bytes (base64-encoded by the plist serializer). Real macOS apps that
  store JSON-shaped values in plist preferences typically use `<data>`
  rather than `<string>`; this primitive matches that convention.
* `flatten-keys prefix="..." [json-encode-values | data-encode-values]`
  — the addressed key's parent dict has the inner dict's entries
  lifted into it, with the new keys named `<prefix><inner>`. The
  original addressed key is removed from its parent. The two value
  flags are mutually exclusive (parse-time error). When the addressed
  path is the root, lifting goes to the top level.

Rules:

* Transforms apply to the source side only; the live file is untouched
  before merge.
* A transform whose path resolves to no value is an error.
* A transform must produce a value of a type the target plist type
  accepts; a type mismatch errors with the path and expected type.
* Transforms run in declaration order. A `flatten-keys` transform that
  introduces new top-level keys can be followed by other transforms
  that target those keys.

This list is intentionally short. Each addition will follow the same
pattern: a real app demands a shape, the primitive is named for what
it does to the data, and it lives in the modify script rather than in
an out-of-band script.

### XML path grammar

The XML path syntax is intentionally smaller than XPath:

* Paths are absolute and start with `/`.
* Element names are matched literally.
* Element predicates may match one attribute: `Element[@name="value"]`.
* Attribute targets use `/@attribute`.
* Text targets use `/text()`.
* A selector must match exactly one node for scalar operations.

Examples:

```text
/gui/ActionProperties/Action[@name="open"]/@shortcut
/config/window/@width
/config/title/text()
```

Index selectors, descendant search, functions, namespace URI resolution, and
boolean predicates are deferred.

### Plist key-path grammar

Plist files model dictionaries and arrays, so an XPath-shaped grammar would
force users to write
`/plist/dict/key[text()="foo"]/following-sibling::string[1]` for every entry.
The plist backend uses a key-path grammar instead:

* Dict keys are joined with `.`.
* Array indices use `[N]`, zero-based. Indices refer to positions in the
  file as it appears on disk; selectors are resolved against the original
  index before any patches apply, so a removal does not renumber indices
  used elsewhere in the same script.
* `[*]` matches every element of the array at that position. It is a
  multi-match selector and is only valid for directives that apply per
  element (`ignore`, `add:hide`, `add:remove`).
* `[key="value"]` matches the single element of an array of dicts whose
  `<key>key</key>` child has the given string value. The predicate is
  bounded to one key/value pair and must match exactly one element. The
  value uses the same quoting rules as a quoted key.
* Keys that contain `.`, `[`, `]`, `"`, or whitespace must be double-quoted.
  Inside a quoted key the only escapes are `\"` and `\\`. An unquoted key
  must match `[A-Za-z_][A-Za-z0-9_-]*`; anything else (including keys that
  start with a digit) must be quoted.
* A leading `.` is allowed but not required.
* The selector must name at least one key or index. Replacing the whole
  plist is not a supported operation.
* A selector resolves to the *value* node bound to the final key or index,
  not the `<key>` element.

Examples:

```text
NSGlobalDomain.AppleLanguages[0]
"com.apple.dock".tilesize
Accounts[0].Password
Accounts[name="main"].Password
Accounts[*].Password
"Servers"[2]."Hidden Field"
```

The grammar omits slices, recursive descent, multi-attribute predicates,
and wildcards on dict keys. Those are deferred until a concrete file
demands them.

### Limits of the plist key path

The grammar above is intentionally narrow. Three limits matter when
writing modify scripts against real plists:

* **Indices are positional.** `Accounts[0]` names whichever entry is
  currently first, not "the entry I meant." If the owning application
  reorders the array between syncs, the selector silently points at a
  different value. For arrays of dicts, prefer the
  `[key="value"]` predicate when the elements have a stable identifying
  key (e.g. `Accounts[name="main"].Password`).
* **`[*]` is array-only.** It applies a directive to every current
  element of the array at that position. There is no equivalent for dict
  keys: hiding every entry of an unknown-shape dict requires ignoring or
  removing the parent.
* **No descendant search.** A path must walk every step. Selectors like
  "any `Password` field anywhere in the document" are out of scope.
* **`[+]` is reserved.** The token is rejected by the path parser today
  with a dedicated `AppendNotAllowed` error, reserving it for a future
  upsert/append selector (e.g. `Accounts[+]` to append a new dict to an
  array). Treat any current use as a parse error rather than a no-op so
  scripts authored against today's grammar keep parsing cleanly when the
  feature lands.

### Plist value operations

The plist backend treats the type tag (`<string>`, `<integer>`, `<dict>`, …)
as part of the value. Operations behave as follows:

* `ignore path` — replace the source value element (tag, content, and closing
  tag) with the system value element verbatim. Type changes between source and
  system are allowed; the patcher only substitutes byte ranges.
* `set path "literal"` — replace the inner text of a scalar value (`string`,
  `integer`, `real`, `data`, `date`). Setting `bool`, `dict`, or `array`
  values is rejected in the first slice.
* `add:hide path` — valid only on `<string>` and `<data>` values; replace
  inner text with `HIDDEN`. Other types error so a hidden field cannot
  silently change shape.
* `add:remove path` and `remove path` — on a dict entry, remove both the
  `<key>` element and its sibling value, including surrounding whitespace.
  On an array element, remove the value element and its surrounding
  whitespace. Indices in other selectors are evaluated against the source
  file before any patches apply, so removing one element does not break
  other selectors in the same script.

When a directive uses `[*]`, the directive is applied independently to
every matched element. For `add:hide`, every matched element must be a
`<string>` or `<data>` value; one type-incompatible element fails the
whole directive. When a directive uses `[key="value"]`, the predicate
must match exactly one element; zero or multiple matches are an error.

INI matchers are rejected in XML or plist mode, and vice versa, at
config-parse time. That makes configuration mistakes fail early.

## Source file resolution

In sidecar mode, `source auto` resolves to a sibling source file. With
languages, source resolution uses backend-specific extensions:

* `language ini` or omitted: `.src.ini`.
* `language xml`: `.src.xml`.
* `language plist`: `.src.plist` is canonical. `.src.json` is accepted as
  an authoring alias and parsed as JSON whose object/array/scalar shape
  maps directly to the plist logical model. If both exist, `.src.plist`
  wins and `.src.json` is reported as an error.

Explicit `source "..."` paths keep their current meaning and override the
auto-resolution lookup. Inline mode rejects an explicit `source` directive
because the data lives in the body.

A file named `modify_foo.xml` with `language xml` and `source auto`
resolves to `foo.xml.src.xml`. A file named `modify_foo.plist` with
`language plist` and `source auto` resolves to `foo.plist.src.plist` or
`foo.plist.src.json`. This mirrors the current `.src.ini` naming rule
rather than inventing a second convention.

JSON authoring loses access to plist-only types (`<data>` and `<date>`).
Authors who need them use `.src.plist` directly. The check happens at
parse time: a JSON value whose target plist type is `<data>` or `<date>`
errors with a hint to switch the source to `.src.plist`.

## Merge algorithm

The two backends share a directive vocabulary but execute merge
differently.

### XML backend (byte-span patching)

1. Parse the modify script and select the backend.
2. Read the source file and the system file.
3. Tokenise both with `xmlparser` and build an index of element,
   attribute, and simple text spans.
4. Evaluate each selector against both indexes using the XML path
   grammar.
5. Convert directives to raw byte-range patches.
6. Apply patches in descending byte-offset order.

For `ignore path` on an XML attribute, the backend replaces the source
attribute value span with the raw system value span. The surrounding
quote style and spacing from the source file remain unchanged.

If a scalar `ignore path` selector does not match exactly one source
node and one system node, the backend returns an error. This is stricter
than INI, but it avoids inventing insertion rules too early.

`remove path` deletes the selected source span. Attribute removal
removes the whole attribute, including adjacent spacing where possible.
Element removal removes the whole element span.

`set path` can be added after `ignore` works. For XML it replaces
attribute values or simple text content with an XML-escaped literal.

### Plist backend (logical merge)

1. Parse the modify script (or its inline body) and select the backend.
2. Decode the source (XML, binary, or JSON) and the live plist (XML or
   binary) into typed `plist::Value` trees.
3. Apply transforms to the source tree in declaration order.
4. Run the default merge (`shallow` or `deep`) onto the live tree.
5. Apply path-level directives (`ignore`, `add:hide`, `add:remove`,
   `remove`, `set`) as overrides.
6. Encode the result as binary plist by default; XML plist if `output
   xml` is set.

Path-level directives in plist override the merge default for their
selectors:

* `ignore path "X"` — leave live[X] untouched even though source has it.
* `set path "X" type "..." "value"` — force live[X] to a literal,
  ignoring source.
* `remove path "X"` and `add:remove path "X"` — delete from the merged
  tree (and from the live file during re-add).
* `add:hide path "X"` — at re-add time, replace live[X] with `HIDDEN`
  before writing source.

The plist backend has no byte-span semantics. The "selectors resolve
against the original index before patches apply" guarantee from the XML
backend translates to "selectors resolve against the original
`plist::Value` tree before mutations apply" for plist — same logical
property, different implementation.

## Re-add filtering algorithm

Re-add filtering operates on the live file and writes the filtered source
text:

1. Parse the modify script and select the backend.
2. Read the live file.
3. Tokenise and build the backend's index.
4. Apply filtering directives:
   * `ignore path`: remove the selected span.
   * `add:remove path`: remove the selected span (or dict entry).
   * `add:hide path`: replace the selected attribute value, text content, or
     plist scalar value with `HIDDEN`.
5. Apply patches in descending byte-offset order.

This gives #98 a backend-owned re-add path without trying to emulate chezmoi's
template engine.

## Parser choice

The two backends use different parsers because they have different
preservation contracts.

**XML backend** — uses
[`xmlparser`](https://docs.rs/xmlparser/latest/xmlparser/), a low-level
pull parser whose tokens carry spans into the original document. That
matches the byte-span patching model and preserves source formatting
outside edited spans.

[`xrust`](https://docs.rs/xrust/latest/xrust/) implements XPath/XQuery/
XSLT concepts but is not the right first dependency: its public model is
tree-oriented and serialises nodes back to XML, which puts formatting
preservation at risk. It could be revisited later as a selector engine
if we can map selected nodes back to raw source spans.

**Plist backend** — uses [`plist`](https://docs.rs/plist/latest/plist/)
for both decode (XML and binary) and encode (binary by default). The
backend is *not* byte-preserving for plist; it round-trips through the
crate's typed value model and re-serialises canonical output. This is an
explicit deviation from the XML backend, justified by the workload:
macOS apps and `cfprefsd` rewrite plists routinely, so any byte-level
preservation we shipped would be defeated within minutes of running the
target app.

JSON source files use [`serde_json`](https://docs.rs/serde_json/) and
map onto the same `plist::Value` shape used internally.

Because `xmlparser` is low-level, the XML backend must add checks that
matter for this project:

* Well-nested element stack validation.
* Duplicate attribute rejection.
* Clear diagnostics for unsupported tokens or selector shapes.

The plist backend adds:

* Structural validation that `<dict>` contains alternating `<key>`/value
  pairs and that the top level has exactly one container.
* Detection of binary vs XML input by the `bplist00`/`<?xml` magic.
* A round-trip self-check in tests: decode → encode → decode produces
  the same logical value.

## Backend shape

The current code has INI parsing, merging, filtering, and source resolution
wired directly into the top-level flow. The new backends introduce a
boundary:

```text
enum Language {
    Ini,
    Xml,
    Plist,
}
```

Each backend owns:

* Source extension.
* Accepted directives, matchers, and path grammar.
* Merge implementation.
* Re-add filtering implementation.
* Tests for source resolution, config parsing, merge, and filtering.

The XML and plist backends share the host shell rather than an
implementation layer:

* The `language` and `merge` directive parser.
* The single-file mode parser (divider detection, format dispatch).
* The transform pipeline.
* The path-grammar dispatch (each backend supplies its own grammar
  implementation, but the directive surface is uniform).
* Source-file resolution.

Below that line they are independent:

* XML uses `xmlparser` and a byte-span patcher.
* Plist uses the `plist` crate, decodes to a typed value tree, and
  applies merge logically.

This split is deliberate. Trying to share an indexer between an
order-preserving XML editor and a re-encoding plist value tree would
contort both. Sharing the directive surface is what users see; the
implementation underneath can be whatever each backend's preservation
contract demands.

The initial implementation should keep the existing INI code paths mostly
intact and dispatch to them through the new boundary. Avoid rewriting
`ini-merge` as part of this work.

## Error handling

Both backends prefer explicit errors over surprising output.

Return errors for:

* Unknown or unsupported path syntax.
* Selectors that match zero nodes.
* Selectors that match multiple nodes (for scalar operations or
  predicates).
* Path-shape conflicts: a dotted key continuation against an array node,
  `[N]` or `[*]` against a dict node, `[key="value"]` against an array
  whose elements are not dicts, or any segment that walks into a scalar.
  The error names the offending segment, the actual node kind, and the
  file role.
* Directives used before the backend can support them (e.g. `set path` on
  a plist `<dict>`).
* Files that cannot be indexed safely.
* For plist: structural violations such as a `<dict>` with an unpaired
  `<key>`, or a top-level `<plist>` with multiple children.

Error messages should include the selector and the file role: source,
system, or live file.

## Test plan

Add data-driven fixtures alongside the existing INI integration tests.

XML cases:

* `language xml` with `source auto` resolves `.src.xml`.
* `ignore path` preserves one system attribute value.
* `add:hide path` replaces one live attribute value with `HIDDEN`.
* `add:remove path` removes one live attribute.
* Ambiguous selectors fail.
* Missing selectors fail.

Plist cases:

* `language plist` with `source auto` resolves `.src.plist`.
* `language plist` with `source auto` resolves `.src.json` when only the
  JSON sibling exists.
* Sidecar with both `.src.plist` and `.src.json` errors.
* Inline mode: a five-line single-file modify script with a JSON body
  produces the same merged output as the equivalent sidecar pair.
* Inline mode rejects an explicit `source` directive.
* Inline mode: a `---` line in the configuration region (before the real
  divider) errors with the line number.
* `merge shallow` (default): top-level keys in source overwrite live;
  unmentioned live keys pass through.
* `merge deep`: nested dicts merge recursively; arrays at any depth
  replace wholesale; scalars override.
* Binary plist input round-trips: decode → merge → encode binary →
  decode produces the expected logical value.
* JSON source whose target type would be `<data>` errors with a hint to
  switch to `.src.plist`.
* `transform path "X" join-lines` turns `["a","b"]` into `"a\nb"`.
* `transform path "X" json-encode` encodes a dict as a UTF-8 JSON string.
* `transform path "X" flatten-keys prefix="p."` lifts inner dict entries
  to top-level keys named `p.<inner>`.
* Transform whose path resolves to no value errors.
* Transform whose result type does not fit the target plist type errors.
* Dotted key path resolves a nested string value.
* Quoted key path resolves a key containing `.`.
* Array index path resolves the right element.
* `ignore path` preserves one system scalar value, including its type
  tag when the type changes.
* `add:hide path` on a `<string>` replaces inner text with `HIDDEN`.
* `add:hide path` on a `<dict>` errors.
* `add:remove path` on a dict entry removes both `<key>` and value.
* `add:remove path` on an array index removes that element without
  shifting indices used by other selectors in the same script.
* `Accounts[*].Password` with `add:hide` hides every match; one
  type-incompatible element fails the directive.
* `Accounts[name="main"].Password` resolves the matching element; zero
  or multiple matches error.
* Path-shape conflicts (dotted continuation against an array, `[N]`
  against a dict, predicate against a non-dict array) error with the
  offending segment named.
* Unpaired `<key>` in XML plist input errors with file role and offset.

End-to-end fixture: a representative macOS `~/Library/Preferences/`
plist round-trips through the plist backend in single-file inline form
and produces binary plist output that decodes back to the same logical
value.

Cross-cutting:

* INI fixtures still pass unchanged.
* INI matchers in XML mode error at config parse, and vice versa.

Later cases:

* XML text node replacement and element removal.
* Escaping for `set path`.
* Namespace-prefixed lexical names.
* Plist `set path` on `<integer>`/`<real>`/`<date>`.

## Implementation slices

1. Add `language` parsing, source extension selection, and backend
   dispatch. Add `merge` directive parsing.
2. Add single-file mode: the `---` divider, format detection on the
   body, the explicit-`source`-rejected-in-inline-mode rule.
3. Add the XML path parser, plist key-path parser, and unit tests for
   both.
4. Add `xmlparser`-based tokenisation and the byte-span patcher for the
   XML backend.
5. Add the XML indexer and implement merge for `ignore path` on
   attributes.
6. Implement XML re-add filtering for `add:hide path` and `add:remove
   path` on attributes.
7. Add the plist backend skeleton: `plist` crate integration, decode of
   XML/binary/JSON source, encode as binary plist, the default
   `merge shallow`. After this slice, a two-line modify script with no
   directives plus an inline JSON body produces a working merge.
8. Add `merge deep` and the path-level plist directives (`ignore`,
   `add:hide`, `add:remove`, `remove`) with literal indices only.
9. Add the transform primitives (`join-lines`, `json-encode`,
   `flatten-keys`).
10. Extend the plist key-path with `[*]` and `[key="value"]`; wire them
    through merge and re-add.
11. Add XML element removal and simple text support.
12. Add `set path` for XML attributes/text and plist scalar values
    (replace-only). Upsert is deferred to a follow-up RFC.

The first mergeable slice should keep INI tests green and add at least
one XML merge fixture and one XML re-add fixture. Slice 7 is the
milestone that makes the common macOS `~/Library/Preferences/`
workflow expressible declaratively; slices 8–10 round it out.

## Adding a plist transform

A new transform primitive (analogous to `join-lines`, `json-encode`,
`flatten-keys`) currently touches five files. The path is discoverable
but tedious; this checklist makes the edit sites explicit.

1. **`src/config/parser.rs`** — extend the `transform path` directive
   parser to recognise the new keyword and any flags it accepts.
2. **`src/transforms.rs`** — add the new variant to the cross-backend
   `Transform` enum (or its plist-only equivalent) and any shared
   plumbing.
3. **`src/backend/plist/transforms.rs`** — implement the actual
   transform: it takes a `&mut plist::Value` (or surrounding
   `Dictionary`) at the resolved path and applies the reshape. Add
   unit tests next to the existing transforms.
4. **Help text in `src/transforms.rs`** — register the new keyword in
   the `--help` listing so `chezmoi_modify_manager --help` documents
   it. Keep the wording close to the surrounding entries.
5. **`docs/src/transforms.md`** — add a user-facing entry with one
   worked example. Cross-link from
   [examples/plist.md](../examples/plist.md) if the transform fits
   naturally into the walkthrough.

If the new transform changes the value's type (e.g. dict → string),
include a unit test that exercises the resulting `merge shallow` shape
— a transform that emits a string where the live plist expects a
`<data>` blob will only fail at decode time without explicit coverage.

## Open questions

* Should the directive be named `language`, `format`, or something else?
  Issue #131 used `language`; this RFC follows that wording.
* Should `ignore path` fail when the system selector is missing, or should it
  delete the source span? The first implementation should fail.
* XML namespaces: match lexical prefixes first, then URI-aware names later?
  The path grammar should reserve `prefix:Element` syntax now to avoid a
  breaking grammar change later, even if URI resolution is deferred.
* Should plist key paths require a leading `.` (jq style) or accept it as
  optional? This RFC picks optional; jq style is also reasonable.
* `[*]` and `[key="value"]` are reserved in the grammar but only
  implemented in slice 10. Should the first plist slice ship without them
  (literal indices only), or should they land together so users do not
  see two plist releases with materially different selector power?
* Predicate equality is string-only (`[key="value"]`). Should integer or
  bool comparison be supported, given that plist `<integer>` and
  `<true/>`/`<false/>` are common identifying fields? First answer: defer
  until a concrete file needs it.
* **Default merge for plist: shallow or deep?** This RFC picks
  `shallow` because it is conservative — it never reaches into nested
  structures the OS owns — but `deep` is more intuitive for files
  whose top-level keys are themselves large dicts. The default is
  hard to change later without breaking existing modify scripts.
  Worth one more pass before slice 7 lands.
* **Output format default for plist: binary or XML?** Binary matches
  what macOS apps expect and what `cfprefsd` will rewrite to anyway, so
  binary is the default in this RFC. XML output stays available via
  `output xml` for users who want a readable on-disk artifact.
* Should `.src.json` be accepted only as an alias, or should JSON become
  a first-class source format with its own backend (`language json`)?
  The latter is the right long-term move for apps that natively store
  JSON configuration; defer until issue #131 demands it.
* Transform primitives — should they accumulate as named directives
  (`join-lines`, `json-encode`, …) or evolve into a small expression
  language? Named directives first; revisit once a fourth or fifth
  primitive shows up.
* Should single-file mode become the recommended default in
  documentation, with sidecar mode positioned as the opt-in for large
  bodies, or should both be presented as equally idiomatic?
* Should XML/plist support live in this crate, or should the span index
  and patcher become a separate crate after the design settles?
