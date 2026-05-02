//! Language/backend dispatch boundary.
//!
//! Each `Language` selects a [`Backend`] implementation that knows how to
//! perform the `process` (merge) and `filter` (re-add) operations for a given
//! configuration file syntax.

use camino::Utf8Path;
use std::io::Read;
use std::io::Write;

mod ini;
mod plist;
mod xml;

/// The language selected by a modify script's `language` directive.
///
/// Defaults to [`Language::Ini`] when no `language` directive is present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Language {
    #[default]
    Ini,
    Xml,
    Plist,
}

impl Language {
    /// Returns the sidecar source-file extension for this language.
    ///
    /// Used by `source auto` resolution to pick the correct sibling file.
    pub(crate) fn sidecar_extension(self) -> &'static str {
        match self {
            Self::Ini => ".src.ini",
            Self::Xml => ".src.xml",
            Self::Plist => ".src.plist",
        }
    }
}

/// A backend able to perform merge (`process`) and re-add filtering
/// (`filter`) for a particular [`Language`].
pub(crate) trait Backend {
    /// Run the merge for `language X`.
    ///
    /// Reads the system file from `stdin`, the source file from the location
    /// implied by `script` + `script_path`, and writes the merged output to
    /// `stdout`.
    fn process(
        &self,
        script: &crate::config::Script,
        script_path: &Utf8Path,
        stdin: &mut dyn Read,
        stdout: &mut dyn Write,
    ) -> anyhow::Result<()>;

    /// Run the re-add filter for the selected language.
    ///
    /// Filters the live file contents according to the directives in `script`
    /// and returns the filtered bytes that should be written to the source
    /// file.
    fn filter(
        &self,
        script: &crate::config::Script,
        script_path: &Utf8Path,
        live_contents: &[u8],
    ) -> anyhow::Result<Vec<u8>>;
}

/// Construct the backend for a given `language`.
pub(crate) fn backend_for(language: Language) -> Box<dyn Backend> {
    match language {
        Language::Ini => Box::new(ini::IniBackend),
        Language::Xml => Box::new(xml::XmlBackend),
        Language::Plist => Box::new(plist::PlistBackend),
    }
}
