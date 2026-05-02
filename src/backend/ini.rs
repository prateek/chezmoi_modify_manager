//! INI backend — wraps the existing INI behaviour behind the
//! [`Backend`](super::Backend) trait.

use crate::backend::Backend;
use crate::config;
use anyhow::Context;
use camino::Utf8Path;
use ini_merge::filter::filter_ini;
use ini_merge::merge::merge_ini;
use std::fs::File;
use std::io::Read;
use std::io::Write;

pub(crate) struct IniBackend;

impl Backend for IniBackend {
    fn process(
        &self,
        script: &config::Script,
        script_path: &Utf8Path,
        stdin: &mut dyn Read,
        stdout: &mut dyn Write,
    ) -> anyhow::Result<()> {
        let cfg = config::parse_for_merge(script)
            .with_context(|| format!("Failed to parse {script_path}"))?;
        let mut stdin = std::io::BufReader::new(stdin);
        let merged = if script.is_inline() {
            let body_bytes: &[u8] = script.body.as_deref().unwrap_or(&[]);
            let format = config::detect_inline_format(cfg.language, body_bytes, script_path)?;
            if format != config::InlineFormat::Ini {
                anyhow::bail!(
                    "{script_path}: language ini: inline body must be INI, got {format:?}"
                );
            }
            // INI must be UTF-8.
            let body = std::str::from_utf8(body_bytes)
                .with_context(|| format!("{script_path}: inline INI body is not valid UTF-8"))?;
            let mut src = std::io::Cursor::new(body.as_bytes());
            merge_ini(&mut stdin, &mut src, &cfg.mutations)?
        } else {
            let src_path = cfg
                .source_path(script_path)
                .context("Failed to get source path")?;
            let mut src_file = File::open(src_path.as_std_path())
                .with_context(|| format!("Failed to open source file at: {src_path}"))?;
            merge_ini(&mut stdin, &mut src_file, &cfg.mutations)?
        };
        for line in merged {
            writeln!(stdout, "{line}")?;
        }
        Ok(())
    }

    fn filter(
        &self,
        script: &config::Script,
        _script_path: &Utf8Path,
        live_contents: &[u8],
    ) -> anyhow::Result<Vec<u8>> {
        let cfg = config::parse_for_add(script)?;
        let mut file = std::io::Cursor::new(live_contents);
        let result = filter_ini(&mut file, &cfg.mutations)?;
        let s: String = itertools::intersperse(result, "\n".into()).collect();
        Ok(s.into_bytes())
    }
}
