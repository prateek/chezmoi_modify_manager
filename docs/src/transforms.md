# Transforms

This is a list of supported transforms. These are used to support some special
hard-to-handle cases. The general syntax is [documented elsewhere](configuration_files.md#transform),
but in short:

```bash
transform "section" "key" transform-name arg1="value" arg2="value" ...
transform regex "section-regex.*" "key-regex.*" transform-name arg1="value" ...
```

For example:

```bash
transform "mysection" "mykey" unsorted-list separator=","
```

Below is a list of supported transforms, but remember to check
`chezmoi_modify_manager --help-transforms` for the most up-to-date list.

## unsorted-list

Compare the value as an unsorted list.
Useful because Konversation likes to reorder lists.

Arguments:

* `separator=","`: Separating character between list elements

## kde-shortcut

Specialised transform to handle KDE changing certain global
shortcuts back and forth between formats like:

```ini
playmedia=none,,Play media playback
playmedia=none,none,Play media playback
```

No arguments.

## keyring

Get the value for a key from the system keyring. Useful for passwords
etc that you do not want in your dotfiles repo.

Arguments:

* `service="service-name"`: Service name to find entry in the keyring.
* `user="user-name"`: Username to find entry in the keyring.

You can add an entry to the secret store for your platform with:

```bash
chezmoi_modify_manager --keyring-set service-name user-name
```

## Plist transforms

Scripts using `language plist` have their own set of transforms applied
to the source side via the path-matcher form:

```bash
transform path "<selector>" <name> [arg="value"]...
```

These reshape the *source* tree before the merge runs; the live tree is
left untouched. See the [Plist example](./examples/plist.md) for an
end-to-end walkthrough and the [XML and plist RFC](./dev/xml_support_rfc.md)
for the full design.

### join-lines

Turn an array-of-strings into a single newline-joined string. Useful for
apps that store a multi-line list as one `<string>` containing
`\n`-separated entries.

No arguments.

```bash
transform path "includePaths" join-lines
```

### json-encode

Encode the addressed value as canonical JSON, replacing the original
node with the resulting `<string>`. Useful when the live plist stores a
structured value as a JSON string.

No arguments.

```bash
transform path "sidebar" json-encode
```

### data-encode

Encode the addressed value as canonical JSON and wrap the UTF-8 bytes
as `<data>` (base64). Useful for apps that store JSON-shaped values as
`<data>` blobs in plist preferences rather than `<string>`.

No arguments.

```bash
transform path "customData" data-encode
```

### flatten-keys

Lift inner dict entries into the parent dict, prefixing each lifted key.
The addressed value must itself be a dict.

Arguments:

* `prefix="<string>"` (required): Prefix added to each lifted key.
* `json-encode-values` (optional flag): each lifted value is replaced
  with its canonical JSON encoding (`<string>`).
* `data-encode-values` (optional flag): each lifted value is replaced
  with `<data>` containing UTF-8 JSON bytes.

`json-encode-values` and `data-encode-values` are mutually exclusive.

```bash
transform path "shortcuts" flatten-keys prefix="shortcut." data-encode-values
```
