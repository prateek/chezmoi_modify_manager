//! Defines supported transforms.

/// Plist `transform path` documentation. The plist transforms live in
/// `crate::config::parser::PlistTransform`; we duplicate the user-facing
/// docs here so `--help-transforms` can render both INI and plist
/// transforms in a single output.
const PLIST_TRANSFORM_HELP: &str = "\
join-lines
----------
Source side: array-of-strings to a single newline-joined string.
Used by apps that store a multi-line list (e.g. host whitelists) as a
single `<string>` containing `\\n`-separated entries. No arguments.

Example:
  transform path \"browserHostWhitelist\" join-lines

json-encode
-----------
Source side: encode the addressed value as canonical JSON, replacing the
original node with the resulting `<string>`. Useful when the live plist
stores a structured value as a JSON string. No arguments.

Example:
  transform path \"sidebar\" json-encode

data-encode
-----------
Source side: encode the addressed value as canonical JSON and wrap the
UTF-8 bytes as `<data>` (base64). Useful for apps that store
JSON-shaped values as `<data>` blobs in plist preferences. No
arguments.

Example:
  transform path \"customData\" data-encode

flatten-keys
------------
Source side: lift inner dict entries into the parent dict, prefixing
each lifted key. The addressed value must itself be a dict.

Arguments:
  prefix=\"<string>\"        Required. Prefix added to each lifted key.
  json-encode-values        Optional flag. Each lifted value is replaced
                            with its canonical JSON encoding (`<string>`).
  data-encode-values        Optional flag. Each lifted value is replaced
                            with `<data>` containing UTF-8 JSON bytes.

`json-encode-values` and `data-encode-values` are mutually exclusive.

Example:
  transform path \"shortcuts\" flatten-keys prefix=\"shortcut.\" data-encode-values

For an end-to-end walkthrough see docs/examples/plist.md and the
RFC at docs/dev/xml_support_rfc.md.";

use ini_merge::mutations::transforms as ini_transforms;
use std::collections::HashMap;
use strum::EnumIter;
use strum::EnumMessage;
use strum::EnumString;
use strum::IntoStaticStr;

#[allow(clippy::doc_markdown)]
/// Supported transforms
///
/// This serves as a central point for documentation, parsing, generating
/// lists etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, EnumString, IntoStaticStr, EnumMessage)]
pub(crate) enum Transform {
    /// Compare the value as an unsorted list.
    /// Useful because Konversation likes to reorder lists.
    ///
    /// Arguments:
    /// * separator="," (Separating character between list elements)
    #[strum(serialize = "unsorted-list")]
    UnsortedLists,
    /// Specialised transform to handle KDE changing certain global
    /// shortcuts back and forth between formats like:
    ///
    /// playmedia=none,,Play media playback
    /// playmedia=none,none,Play media playback
    ///
    /// No arguments.
    #[strum(serialize = "kde-shortcut")]
    KdeShortcut,
    /// Get the value for a key from the system keyring. Useful for passwords
    /// etc that you do not want in your dotfiles repo.
    ///
    /// Arguments:
    /// * service="service-name"  (service name to find entry in the keyring)
    /// * user="user-name"        (username to find entry in the keyring)
    ///
    /// On Linux you can add an entry to the keyring using:
    /// chezmoi_modify_manager --keyring-set "service-name" "user-name"
    #[strum(serialize = "keyring")]
    Keyring,
}

impl Transform {
    /// Print help for transforms
    pub(crate) fn help() {
        use strum::IntoEnumIterator;
        let docs = Self::iter().map(|elem| {
            let name: &str = elem.into();
            format!(
                "{}\n{}\n{}",
                name,
                "-".repeat(name.len()),
                elem.get_documentation().unwrap_or("Missing docs")
            )
        });
        println!("Supported transforms:");
        println!("====================\n");
        println!("INI transforms (used with `transform \"section\" \"key\" ...`):\n");
        // Workaround for https://github.com/rust-itertools/itertools/issues/942
        use itertools::Itertools;
        println!(
            "{}",
            Itertools::intersperse(docs, "\n\n".to_string()).collect::<String>()
        );

        println!("\n\nPlist transforms (used with `transform path \"<selector>\" ...`):");
        println!("---------------------------------------------------------------\n");
        println!("{PLIST_TRANSFORM_HELP}");
    }

    /// Construct transform with arguments
    pub(crate) fn construct(
        self,
        args: &HashMap<String, String>,
    ) -> anyhow::Result<ini_transforms::TransformerDispatch> {
        use ini_transforms::Transformer;
        match self {
            Self::UnsortedLists => {
                Ok(ini_transforms::TransformUnsortedLists::from_user_input(args)?.into())
            }
            Self::KdeShortcut => {
                Ok(ini_transforms::TransformKdeShortcut::from_user_input(args)?.into())
            }
            #[cfg(feature = "keyring")]
            Self::Keyring => Ok(ini_transforms::TransformKeyring::from_user_input(args)?.into()),
            #[cfg(not(feature = "keyring"))]
            Transform::Keyring => Err(anyhow::anyhow!(
                "This build of chezmoi_modify_manager does not support the keyring transform"
            )),
        }
    }
}
