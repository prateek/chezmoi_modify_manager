//! Support for adding files

// Doc comments are used to generate --help, not to for rustdoc.
#![allow(clippy::doc_markdown)]

use crate::backend;
use crate::config;
use crate::utils::CHEZMOI_AUTO_SOURCE_VERSION;
use crate::utils::Chezmoi;
use crate::utils::ChezmoiVersion;
use anyhow::Context;
use anyhow::Ok;
use anyhow::anyhow;
use camino::Utf8Path;
use camino::Utf8PathBuf;
use indoc::formatdoc;
use std::fs::File;
use std::io::Write;
use strum::Display;
use strum::EnumIter;
use strum::EnumMessage;
use strum::EnumString;
use strum::IntoStaticStr;

#[cfg(test)]
mod tests;

/// The style of calls to the executable
#[derive(
    Debug, Eq, PartialEq, EnumString, Clone, Copy, EnumIter, EnumMessage, Display, IntoStaticStr,
)]
pub enum Style {
    /// Selects between path and path-tmpl based on detected chezmoi version
    #[strum(serialize = "auto")]
    Auto,
    /// chezmoi_modify_manager is searched for in PATH
    ///    (modify_ script is not templated for best performance)
    #[strum(serialize = "path")]
    InPath,
    /// chezmoi_modify_manager is searched for in PATH
    ///    (modify_ script is templated for your convenience)
    #[strum(serialize = "path-tmpl")]
    InPathTmpl,
    /// Program is in .utils of chezmoi source state
    ///    (modify_ script is always templated)
    #[strum(serialize = "src")]
    InSrc,
}

/// The mode for adding
#[derive(Debug, Clone, Copy)]
pub(crate) enum Mode {
    Normal,
    Smart,
}

/// Template for newly created INI scripts
const TEMPLATE_INI: &str = indoc::indoc! {r#"
    #!(PATH)

    (SOURCE)

    # Add your ignores and transforms here
    #ignore section "my-section"
    #ignore "exact section name without brackets" "exact key name"
    #ignore regex "section.*" "key_prefix_.*"
    #transform "section" "key" transform_name read="the docs" for="more detail on transforms"
"#};

/// Template for newly created XML scripts
const TEMPLATE_XML: &str = indoc::indoc! {r#"
    #!(PATH)

    language xml
    source auto-path

    # Add your ignores and transforms here. See `--help-syntax` for the
    # path-matcher grammar and docs/configuration_files.md for examples.
    #ignore path "/config/window/@width"
    #remove path "/config/legacy"
    #set path "/config/title/text()" "My Title"
"#};

/// Template for newly created plist (XML or binary) scripts
const TEMPLATE_PLIST: &str = indoc::indoc! {r#"
    #!(PATH)

    language plist
    source auto-path
    # Default merge is shallow: only top-level keys are replaced.
    # Use `merge deep` for a recursive dict merge.
    #merge shallow
    # Default output is binary plist; uncomment `output xml` if your live
    # file is XML and you want stable diffs (so the on-disk shape doesn't
    # flip to binary on first apply).
    #output xml

    # The source file alongside this script may be `.src.plist` (XML or
    # binary) or `.src.json` (JSON, decoded then merged). Single-file mode
    # via the `---` divider is also available; see
    # docs/examples/plist.md.

    # Add your ignores and transforms here. See `--help-syntax` and
    # `--help-transforms` for the available directives and transforms.
    #ignore path "Accounts[name=\"main\"].Password"
    #transform path "browserHostWhitelist" join-lines
    #transform path "shortcuts" flatten-keys prefix="shortcut." data-encode-values
"#};

const SOURCE_NEW: &str = "source auto";
const SOURCE_OLD: &str = indoc::indoc! {r#"
    # This is needed to figure out where the source file is on older Chezmoi versions.
    # See https://github.com/twpayne/chezmoi/issues/2934
    source "{{ .chezmoi.sourceDir }}/{{ .chezmoi.sourceFile | trimSuffix ".tmpl" | replace "modify_" "" }}.src.ini""#};

/// Shebang line to use when command is in PATH
const IN_PATH: &str = "/usr/bin/env chezmoi_modify_manager";
/// Shebang line to use when command is in dotfile repo.
const IN_SRC: &str =
    "{{ .chezmoi.sourceDir }}/.utils/chezmoi_modify_manager-{{ .chezmoi.os }}-{{ .chezmoi.arch }}";

/// The detected source-file format used to pick a skeleton template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputFormat {
    /// Default: INI (or anything we don't recognise).
    Ini,
    /// XML, but not a `<plist>` document.
    Xml,
    /// XML plist (`<?xml ... <plist ...>`).
    PlistXml,
    /// Binary plist (`bplist00...`).
    PlistBinary,
}

/// Sniff the input bytes and return the most-specific format we can
/// recognise. Falls back to [`InputFormat::Ini`] for anything we don't
/// recognise.
pub(crate) fn detect_input_format(bytes: &[u8]) -> InputFormat {
    if bytes.starts_with(b"bplist00") {
        return InputFormat::PlistBinary;
    }

    // Skip a UTF-8 BOM if present so we can match `<?xml` after it.
    let trimmed = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let leading: &[u8] = if trimmed.len() > 4096 {
        &trimmed[..4096]
    } else {
        trimmed
    };

    // Lossy decode for substring searches; we only use ASCII tokens.
    let head = String::from_utf8_lossy(leading);

    if head.trim_start().starts_with("<?xml") {
        // Heuristic: search for `<plist` in the first 4 KiB. The DOCTYPE
        // declaration for plists also names "plist" but only inside
        // PUBLIC/SYSTEM identifiers; that still indicates a plist
        // document, which is fine for the skeleton template.
        if head.contains("<plist") || head.contains("//Apple//DTD PLIST") {
            return InputFormat::PlistXml;
        }
        return InputFormat::Xml;
    }
    InputFormat::Ini
}

/// Return the sidecar source-file extension that matches the skeleton
/// the `--add` flow will emit for a given detected [`InputFormat`].
///
/// The skeleton uses `source auto-path`, which looks up the sibling file
/// by [`backend::Language::sidecar_extension`]; this helper keeps the
/// write side of `--add` in sync with that lookup.
pub(crate) fn sidecar_extension_for_format(format: InputFormat) -> &'static str {
    match format {
        InputFormat::Ini => backend::Language::Ini.sidecar_extension(),
        InputFormat::Xml => backend::Language::Xml.sidecar_extension(),
        InputFormat::PlistXml | InputFormat::PlistBinary => {
            backend::Language::Plist.sidecar_extension()
        }
    }
}

/// Format the template for the given input format and chezmoi version.
fn template(path: &str, version: &ChezmoiVersion, format: InputFormat) -> String {
    match format {
        InputFormat::PlistBinary | InputFormat::PlistXml => TEMPLATE_PLIST.replace("(PATH)", path),
        InputFormat::Xml => TEMPLATE_XML.replace("(PATH)", path),
        InputFormat::Ini => {
            let result = TEMPLATE_INI.replace("(PATH)", path);
            if version < &CHEZMOI_AUTO_SOURCE_VERSION {
                result.replace("(SOURCE)", SOURCE_OLD)
            } else {
                result.replace("(SOURCE)", SOURCE_NEW)
            }
        }
    }
}

/// Perform actual adding with a script
fn add_with_script(
    chezmoi: &impl Chezmoi,
    src_path: Option<Utf8PathBuf>,
    path: &Utf8Path,
    style: Style,
    status_out: &mut impl Write,
) -> anyhow::Result<()> {
    chezmoi.add(path)?;
    // If we don't already know the source path (newly added file), get it now
    let src_path = match src_path {
        Some(path) => path,
        None => chezmoi
            .source_path(path)?
            .context("chezmoi couldn't find added file")?,
    };
    let src_name = src_path.file_name().context("File has no filename")?;
    // Sniff the *user-facing* file (not the chezmoi-staged copy) to pick a
    // language-appropriate skeleton when the modify script doesn't yet
    // exist. Failure to read the bytes shouldn't block the add — fall
    // back to the INI skeleton (and the historic `.src.ini` sidecar).
    let format = std::fs::read(path)
        .map(|bytes| detect_input_format(&bytes))
        .unwrap_or(InputFormat::Ini);
    // Derive the sidecar extension from the detected format so plist/XML
    // inputs are written to `.src.plist`/`.src.xml`, matching what the
    // emitted skeleton's `source auto-path` will look up at apply time.
    let data_path = src_path.with_file_name(format!(
        "{src_name}{}",
        sidecar_extension_for_format(format)
    ));
    let script_path = match style {
        Style::Auto => panic!("Impossible: Auto should already have been mapped"),
        Style::InPath => src_path.with_file_name(format!("modify_{src_name}")),
        Style::InPathTmpl | Style::InSrc => {
            src_path.with_file_name(format!("modify_{src_name}.tmpl"))
        }
    };

    // Add while respecting filtering directives
    filtered_add(&data_path, &src_path, None, status_out)?;

    // Remove the temporary file that chezmoi added
    std::fs::remove_file(src_path)?;

    maybe_create_script(&script_path, style, status_out, &chezmoi.version()?, format)?;
    Ok(())
}

/// Add and handle filtering directives (add:remove, add:hide and ignore)
///
/// * `target_path`: Path to write to
/// * `src_path`: Path to actually read file data from
/// * `script_path`: Path to modify script (if it exists)
/// * `status_out`: Where to write status messages
fn filtered_add(
    target_path: &Utf8Path,
    src_path: &Utf8Path,
    script_path: Option<&Utf8Path>,
    status_out: &mut impl Write,
) -> Result<(), anyhow::Error> {
    let file_contents =
        std::fs::read(src_path).context("Failed to load data from file we are adding")?;

    // If we are updating an existing script, run the contents through the filtering
    let mut file_contents = if let Some(sp) = script_path {
        _ = writeln!(
            status_out,
            "Has existing modify script, parsing to check for filtering..."
        );
        let config_data = std::fs::read(sp).context("Failed to load modify script")?;
        internal_filter(&config_data, sp, &file_contents)?
    } else {
        file_contents
    };

    // Preserve binary plist bytes verbatim: appending a trailing `\n`
    // would corrupt the bplist trailer. For text inputs we keep the
    // existing newline-terminator behaviour.
    let is_binary_plist = file_contents.starts_with(b"bplist00");
    if !is_binary_plist && !file_contents.ends_with(b"\n") {
        file_contents.push(b'\n');
    }

    _ = writeln!(status_out, "Writing out file data");
    std::fs::write(target_path, file_contents)?;
    Ok(())
}

/// Perform internal filtering using add:hide and add:remove (modern filtering)
fn internal_filter(
    config_data: &[u8],
    script_path: &Utf8Path,
    contents: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let script = config::Script::parse(config_data, script_path)?;
    let language = config::peek_language(&script)?;
    let backend = backend::backend_for(language);
    backend.filter(&script, script_path, contents)
}

/// Create a modify script if one doesn't exist.
///
/// `format` is sniffed from the source-file bytes and selects the
/// language-appropriate skeleton: INI by default, plist when the bytes
/// begin with `bplist00` or look like an XML plist, and XML for other
/// XML inputs.
fn maybe_create_script(
    script_path: &Utf8Path,
    style: Style,
    status_out: &mut impl Write,
    version: &ChezmoiVersion,
    format: InputFormat,
) -> anyhow::Result<()> {
    if script_path.exists() {
        return Ok(());
    }
    let mut file = File::create(script_path)?;
    file.write_all(
        template(
            match style {
                Style::Auto => panic!("Impossible: Auto should already have been mapped"),
                Style::InPath => IN_PATH,
                Style::InPathTmpl => IN_PATH,
                Style::InSrc => IN_SRC,
            },
            version,
            format,
        )
        .as_bytes(),
    )?;
    _ = writeln!(status_out, "New script at {script_path}");

    Ok(())
}

/// Classifies the state of the file in chezmoi source state.
#[derive(Debug)]
enum ChezmoiState {
    NotInChezmoi,
    ExistingNormal {
        data_path: Utf8PathBuf,
    },
    ExistingManaged {
        script_path: Utf8PathBuf,
        data_path: Utf8PathBuf,
    },
}

fn recurse_files(path: &Utf8Path, buf: &mut Vec<Utf8PathBuf>) -> anyhow::Result<()> {
    let entries = Utf8Path::read_dir_utf8(path)?;

    for entry in entries {
        let entry = entry?;
        let meta = entry.metadata()?;

        if meta.is_dir() {
            recurse_files(entry.path(), buf)?;
        }

        if meta.is_file() {
            buf.push(entry.into_path());
        }
    }

    Ok(())
}

/// Add a file
pub(crate) fn add(
    chezmoi: &impl Chezmoi,
    mode: Mode,
    recursive: bool,
    mut style: Style,
    path: &Utf8Path,
    status_out: &mut impl Write,
) -> anyhow::Result<()> {
    // Check for auto style
    if style == Style::Auto {
        style = if chezmoi.version()? < CHEZMOI_AUTO_SOURCE_VERSION {
            Style::InPathTmpl
        } else {
            Style::InPath
        }
    }
    // Start with a sanity check on the input file and environment
    sanity_check(path, style, chezmoi)?;

    if path.is_dir() {
        if !recursive {
            return Err(anyhow!(
                "Trying to add a directory, but -r (--recursive) flag is not set, ignoring"
            ));
        }
        let mut files = vec![];
        recurse_files(path, &mut files)?;
        let num_files = files.len();
        _ = writeln!(status_out, "Adding {num_files} files");
        for file in files {
            _ = writeln!(status_out, "Adding {file:?}");
            add_file(chezmoi, mode, style, &file, status_out)?;
        }
        Ok(())
    } else {
        add_file(chezmoi, mode, style, path, status_out)
    }
}

pub(crate) fn add_file(
    chezmoi: &impl Chezmoi,
    mode: Mode,
    style: Style,
    path: &Utf8Path,
    status_out: &mut impl Write,
) -> anyhow::Result<()> {
    // Let's check if the managed path exists
    let src_path = chezmoi.source_path(path)?;

    // Then lets classify the situation we are in
    let situation = classify_chezmoi_state(src_path)?;

    // Inform user of what we found
    match &situation {
        ChezmoiState::NotInChezmoi => {
            _ = writeln!(status_out, "State: New (to chezmoi) file");
        }
        ChezmoiState::ExistingNormal { .. } => {
            _ = writeln!(
                status_out,
                "State: Managed by chezmoi, but not a modify script."
            );
        }
        ChezmoiState::ExistingManaged { .. } => {
            _ = writeln!(
                status_out,
                "State: Managed by chezmoi and is a modify script."
            );
        }
    }

    // Finally decide on an action based on source state and the user selected mode.
    match (situation, mode) {
        (ChezmoiState::NotInChezmoi | ChezmoiState::ExistingNormal { .. }, Mode::Smart) => {
            _ = writeln!(
                status_out,
                "Action: Adding as plain chezmoi (since we are in smart mode)."
            );
            chezmoi.add(path)?;
        }
        (ChezmoiState::NotInChezmoi, Mode::Normal) => {
            _ = writeln!(
                status_out,
                "Action: Adding & setting up new modify_ script."
            );
            add_with_script(chezmoi, None, path, style, status_out)?;
        }
        (ChezmoiState::ExistingNormal { data_path }, Mode::Normal) => {
            _ = writeln!(
                status_out,
                "Action: Converting & setting up new modify_ script."
            );
            add_with_script(chezmoi, Some(data_path), path, style, status_out)?;
        }
        (
            ChezmoiState::ExistingManaged {
                script_path,
                data_path,
            },
            _,
        ) => {
            // Derive the extension from the actual sidecar path so the
            // diagnostic is correct for plist/XML scripts (`.src.plist`,
            // `.src.xml`, `.src.json`) — not just the INI default. We
            // use the data file's whole filename suffix because chezmoi
            // sidecars are conventionally `<basename>.src.<ext>`; the
            // user expects to see the same thing in the action message.
            let extension = sidecar_extension_for(&script_path);
            _ = writeln!(
                status_out,
                "Action: Updating existing {extension} file for {script_path}."
            );
            filtered_add(
                data_path.as_ref(),
                path,
                Some(script_path.as_ref()),
                status_out,
            )?;
        }
    }
    Ok(())
}

/// Find out what the state of the file in chezmoi currently is.
fn classify_chezmoi_state(src_path: Option<Utf8PathBuf>) -> Result<ChezmoiState, anyhow::Error> {
    let situation = match src_path {
        Some(existing_file) => {
            let src_filename = existing_file.file_name().context("No file name?")?;
            let is_mod_script = src_filename.starts_with("modify_");
            if is_mod_script {
                let src_dir = existing_file
                    .parent()
                    .context("Couldn't extract directory")?;
                let targeted_file = find_data_file(&existing_file, src_dir)?;
                ChezmoiState::ExistingManaged {
                    script_path: existing_file,
                    data_path: targeted_file,
                }
            } else {
                ChezmoiState::ExistingNormal {
                    data_path: existing_file,
                }
            }
        }
        None => ChezmoiState::NotInChezmoi,
    };
    Ok(situation)
}

/// Perform preliminary environment sanity checks
fn sanity_check(
    path: &Utf8Path,
    style: Style,
    chezmoi: &impl Chezmoi,
) -> Result<(), anyhow::Error> {
    if !path.exists() {
        return Err(anyhow!("{path} does not exist"));
    }
    if Style::InPath == style && chezmoi.version()? < CHEZMOI_AUTO_SOURCE_VERSION {
        return Err(anyhow!(
            "To use \"--style path\" you need chezmoi {CHEZMOI_AUTO_SOURCE_VERSION} or newer"
        ));
    }
    match crate::doctor::hook_paths(chezmoi)?.as_slice() {
        [] => Ok(()),
        _ => Err(anyhow!(
            "Legacy hook script found, see chezmoi_modify_manager --doctor and please read https://github.com/VorpalBlade/chezmoi_modify_manager/blob/main/doc/migration_3.md"
        )),
    }
}

/// Given a modify script, find the associated sidecar source file.
///
/// The sidecar extension is selected by the script's `language` directive
/// via [`backend::Language::sidecar_extension`]. Defaults to `.src.ini`
/// when no `language` directive is present (or when the script can't be
/// parsed; in that case we fall through and let the existence check below
/// surface the user-facing error).
fn find_data_file(
    modify_script: &Utf8Path,
    src_dir: &Utf8Path,
) -> Result<Utf8PathBuf, anyhow::Error> {
    let extension = sidecar_extension_for(modify_script);
    let data_file = modify_script
        .file_name()
        .context("Failed to get filename")?
        .strip_prefix("modify_")
        .and_then(|s| s.strip_suffix(".tmpl").or(Some(s)))
        .context("This should never happen")?
        .to_owned()
        + extension;
    let mut targeted_file: Utf8PathBuf = src_dir.into();
    targeted_file.push(data_file);
    if !targeted_file.exists() {
        let err_str = formatdoc!(
            r#"Found existing modify_ script but no associated {extension} file (looked at {targeted_file}).
                        Possible causes:
                        * Did you change the "source" directive from the default value?
                        * Remove the file by mistake?

                        Either way: the automated adding code is not smart enough to handle this situation by itself."#
        );
        return Err(anyhow!(err_str));
    }
    Ok(targeted_file)
}

/// Read the modify script (best-effort) and resolve its sidecar source-file
/// extension. Falls back to `.src.ini` (the INI default) on any error so
/// that older scripts and broken scripts still produce the same diagnostic
/// path they did before sidecar-extension awareness was added.
fn sidecar_extension_for(modify_script: &Utf8Path) -> &'static str {
    let Result::Ok(bytes) = std::fs::read(modify_script) else {
        return backend::Language::Ini.sidecar_extension();
    };
    let Result::Ok(script) = config::Script::parse(&bytes, modify_script) else {
        return backend::Language::Ini.sidecar_extension();
    };
    config::peek_language(&script)
        .unwrap_or_default()
        .sidecar_extension()
}
