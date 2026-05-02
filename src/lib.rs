//! This is not a stable API, and is to be used internally by the binary and
//! the integration tests only.

use crate::utils::CHEZMOI_AUTO_SOURCE_VERSION;
use crate::utils::RealChezmoi;
pub use add::Style;
use anyhow::Context;
pub use arguments::ChmmArgs;
pub use arguments::parse_args;
use indoc::printdoc;
use std::io::Read;
use std::io::Write;

mod add;
mod arguments;
mod backend;
mod config;
mod doctor;
mod path;
#[cfg(test)]
mod test_support;
mod transforms;
mod update;
mod utils;

/// Main function, amenable to integration tests.
///
/// In order to support integration tests we need to be able to provide stdin
/// and capture stdout. It would be very nice if the non-test case could simply
/// call us with `stdin.lock()` and `stdout.lock()`. However, that breaks the
/// self-updater case, which uses stdio directly. Instead, call with functions
/// that return stdio streams. Note! stderr is not captured, it is used for
/// logging.
pub fn inner_main<R: Read, W: Write, WS: Write, FR, FW, FS>(
    opts: ChmmArgs,
    stdin: FR,
    stdout: FW,
    status: FS,
) -> anyhow::Result<()>
where
    FR: FnOnce() -> R,
    FW: FnOnce() -> W,
    FS: FnOnce() -> WS,
{
    match opts {
        ChmmArgs::Process(file_name) => {
            let raw =
                std::fs::read(&file_name).with_context(|| format!("Failed to load {file_name}"))?;
            let script = config::Script::parse(&raw, &file_name)
                .with_context(|| format!("Failed to parse {file_name}"))?;
            let language = config::peek_language(&script)
                .with_context(|| format!("Failed to parse {file_name}"))?;
            let backend = backend::backend_for(language);
            let mut stdin = stdin();
            let mut stdout = stdout();
            backend
                .process(&script, &file_name, &mut stdin, &mut stdout)
                .with_context(|| format!("{file_name}: backend processing failed"))?;
        }
        ChmmArgs::Add {
            _a,
            recursive,
            files,
            style,
        } => {
            let mut stdout = status();
            for file in files {
                add::add(
                    &RealChezmoi::default(),
                    add::Mode::Normal,
                    recursive,
                    style,
                    &file,
                    &mut stdout,
                )?;
            }
        }
        ChmmArgs::Smart {
            _a,
            recursive,
            files,
        } => {
            let mut stdout = status();
            for file in files {
                add::add(
                    &RealChezmoi::default(),
                    add::Mode::Smart,
                    recursive,
                    Style::Auto, // Style unused for the smart case, so doesn't matter
                    &file,
                    &mut stdout,
                )?;
            }
        }
        #[cfg(feature = "updater-tls-rusttls")]
        ChmmArgs::Update { _a, no_confirm } => {
            update::update(no_confirm)?;
        }
        #[cfg(not(feature = "updater-tls-rusttls"))]
        ChmmArgs::Update { .. } => {
            println!("Support for the updater was not included in this build.");
            println!(
                "Please refer to the way you installed this software to determine how to update \
                 it."
            );
            std::process::exit(1);
        }
        ChmmArgs::Doctor { _a } => doctor::doctor()?,
        ChmmArgs::HelpSyntax { _a } => help_syntax(),
        ChmmArgs::HelpTransforms { _a } => transforms::Transform::help(),
        #[cfg(feature = "keyring")]
        ChmmArgs::KeyringSet {
            _a,
            service,
            username,
        } => {
            let password = rpassword::prompt_password("Password: ")?;
            let entry = ini_merge::keyring::Entry::new(&service, &username)?;
            entry.set_password(&password)?;
        }
        #[cfg(feature = "keyring")]
        ChmmArgs::KeyringRemove {
            _a,
            service,
            username,
        } => {
            let entry = ini_merge::keyring::Entry::new(&service, &username)?;
            entry.delete_credential()?;
        }
    }
    Ok(())
}

/// Run the re-add filter for `script_path` against `live_contents`,
/// returning the filtered bytes that would be written to the source file.
///
/// Exposed so integration tests can exercise the [`Backend::filter`] path
/// without having to drive `chezmoi add` end-to-end.
pub fn run_filter(script_path: &camino::Utf8Path, live_contents: &[u8]) -> anyhow::Result<Vec<u8>> {
    let raw =
        std::fs::read(script_path).with_context(|| format!("Failed to load {script_path}"))?;
    let script = config::Script::parse(&raw, script_path)
        .with_context(|| format!("Failed to parse {script_path}"))?;
    let language =
        config::peek_language(&script).with_context(|| format!("Failed to parse {script_path}"))?;
    let backend = backend::backend_for(language);
    backend.filter(&script, script_path, live_contents)
}

/// Print help for the overall syntax of the configuration language.
#[allow(clippy::too_many_lines)]
fn help_syntax() {
    printdoc! {r#"
    Configuration files
    ===================

    chezmoi_modify_manager uses configuration files to control how to merge
    INI, XML, or plist files. The easiest way to get started is to use -a to
    add a file and generate a skeleton configuration file.

    Syntax
    ======

    The file consists of directives, one per line. Comments are supported by
    prefixing a line with #. Comments are only supported at the start of lines.

    See also: docs/configuration_files.md, docs/transforms.md, docs/actions.md.

    Directives (cross-language)
    ===========================

    language
    --------
    Selects the configuration-file syntax. Defaults to `ini` when omitted.

    language ini    (default)
    language xml
    language plist

    source
    ------
    Tells chezmoi_modify_manager where to find the source file. Three
    forms are accepted across all languages:

    source "<path>"          Explicit path. Required for INI when running
                             on Chezmoi older than {0}; typically:
                             {1}
    source auto              Resolve the sibling source file using the
                             CHEZMOI_SOURCE_* environment variables
                             (Chezmoi {0} and newer). The historic INI
                             default; works for `language ini`.
    source auto-path         Resolve sibling using the script's filename;
                             recommended for `language xml`/`language
                             plist`. Picks the language-specific sidecar
                             extension (.src.ini / .src.xml / .src.plist).

    Single-file mode (no sidecar): place a `---` line in the script. The
    directives appear above the divider; the source body appears below
    it. Useful for inline XML or plist sources.

    See docs/source_specification.md and docs/examples/plist.md.

    merge
    -----
    Plist only. Selects the top-level merge strategy. Defaults to `shallow`.

    merge shallow   (default — only top-level keys are replaced)
    merge deep      (recursive dict merge; arrays and scalars replace)

    output
    ------
    Plist only. Selects the encoding written to stdout. Defaults to binary
    plist (what macOS apps and `cfprefsd` expect).

    output xml      (encode result as an XML plist)
    output binary   (default — encode result as a binary plist)

    Path-matcher directives (XML and plist)
    =======================================
    The XML and plist backends address nodes by an XPath-like path string
    instead of section/key pairs. The directive forms are:

    ignore path "<selector>"
    remove path "<selector>"
    set path "<selector>" "<literal>"
    set path "<selector>" <type> "<literal>"
    transform path "<selector>" <name> [arg="value"]...
    add:hide path "<selector>"
    add:remove path "<selector>"

    The optional `<type>` on `set path` is plist-only. Valid tags:
      string   UTF-8 text                (`<string>`)
      integer  base-10 signed integer    (`<integer>`)
      real     IEEE-754 floating point   (`<real>`)
      data     base64-encoded bytes      (`<data>`)
      date     ISO-8601 timestamp        (`<date>`)
    Omit the tag when the existing scalar's type is unambiguous; the
    parser infers it. The XML backend rejects type tags.

    Plist selector examples:
      "NSGlobalDomain.AppleLanguages"
      "Accounts[0].Password"
      "Accounts[*].Password"
      "Accounts[name=\"main\"].Password"   (or single-quoted: [name='main'])

    XML selector examples:
      "/config/window/@width"
      "/gui/Action[@name=\"open\"]/@shortcut"
      "/config/title/text()"

    See `--help-transforms` for the supported `transform path` names
    (including the plist-only `join-lines`, `json-encode`, `data-encode`,
    `flatten-keys`).

    Directives (INI)
    ================
    The remaining directives below are INI-specific. The `source`
    directive is documented in the cross-language section above.

    ignore
    ------
    XML/plist scripts use `ignore path "..."`; see Path-matcher above.

    Ignore a certain line, always taking it from the target file (i.e. file in
    your home directory), instead of the source state. The following variants
    are supported:

    ignore section "my-section"
    ignore section regex "^MySection.*"
    ignore "my-section" "my-key"
    ignore regex "section.*regex" "key regex.*"

    The first form ignores a whole section (exact literal match).
    The second form ignores a whole section (regex match).
    The third form ignores a specific key (exact literal match).
    The fourth form uses a regex to ignore a specific key.

    Prefer the exact literal match variants where possible, they will be
    marginally faster.

    An additional effect is that lines that are missing in the source state
    will not be deleted if they are ignored.

    Finally, ignored lines will not be added back when using --add or
    --smart-add, in order to reduce git diffs.

    set
    ---
    XML/plist scripts use `set path "..." "..."`; see Path-matcher above.

    Set an entry to a specific value. This is primarily useful together with
    chezmoi templates, allowing you to override a specific value for only some
    of your computers. The following variants are supported:

    set "section" "key" "value"
    set "section" "key" "value" separator="="

    By default separator is " = ", which might not match what the program that
    the ini files belongs to uses.

    Notes:
    * Only exact literal matches are supported.
    * It works better if the line exists in the source & target state, otherwise
      it is likely the line will get formatted weirdly (which will often be
      changed by the program the INI file belongs to).

    remove
    ------
    XML/plist scripts use `remove path "..."`; see Path-matcher above.

    Unconditionally remove everything matching the directive. This is primarily
    useful together with chezmoi templates, allowing you to remove a specific
    key or section for only some of your computers. The following variants are
    supported:

    remove section "my-section"
    remove "my-section" "my-key"
    remove regex "section.*regex" "key regex.*"

    (Matching works identically to ignore, see above for more details.)

    transform
    ---------
    XML/plist scripts use `transform path "..." <name> [arg="..."]...`;
    see Path-matcher above.

    Some specific situations need more complicated merging that a simple
    ignore. For those situations you can use transforms. Supported variants
    are:

    transform "section" "key" transform-name arg1="value" arg2="value" ...
    transform regex "section-regex.*" "key-regex.*" transform-name arg1="value" ...

    (Matching works identically to ignore except matching entire sections is
    not supported. See above for more details.)

    For example, to treat mykey in mysection as an unsorted comma separated
    list, you could use:

    transform "mysection" "mykey" unsorted-list separator=","

    The full list of supported transforms, and how to use them can be listed
    using --help-transforms.

    add:remove & add:hide
    ---------------------
    XML/plist scripts use `add:remove path "..."` and `add:hide path "..."`;
    see Path-matcher above.

    These two directives control the behaviour when using --add or --smart-add.
    In particular, these allow filtering lines that will be added back to the
    source state.

    add:remove will remove the matching lines entirely. The following forms are
    supported:

    add:remove section "section name"
    add:remove "section name" "key"
    add:remove regex  "section-regex.*" "key-regex.*"

    (Matching works identically to ignore, see above for more details.)

    add:hide will instead keep the entries but replace the value associated with
    those keys. This is useful together with the keyring transform in particular,
    as the key needs to exist in the source or target state for it to trigger
    the replacement. The following forms are supported:

    add:hide section "section name"
    add:hide "section name" "key"
    add:hide regex  "section-regex.*" "key-regex.*"

    (Matching works identically to ignore, see above for more details.)

    no-warn-multiple-key-matches
    ----------------------------
    This directive quitens warnings on multiple regular expressions matching the
    same section+key. While the warning is generally useful, sometimes you might
    actually "know what you are doing" and want to suppress it.
    "#,
    CHEZMOI_AUTO_SOURCE_VERSION,
    r#"source "{{ .chezmoi.sourceDir }}/{{ .chezmoi.sourceFile | trimSuffix ".tmpl" | replace "modify_" "" }}.src.ini""#};
}
