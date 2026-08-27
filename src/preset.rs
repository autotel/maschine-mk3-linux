//! Named sets of settings, and the shipped ones.
//!
//! A preset is just a config file with a name and a sentence about what it is
//! for. Keeping them as ordinary single files is deliberate: sharing one means
//! sending a file, and reading one means opening it in an editor. There is no
//! database and no format anyone has to learn twice.
//!
//! Presets are *settings*, not hardware. The device profile
//! ([`crate::profile`]) says which bit is Play; a preset says what Play should
//! send. Switching preset never changes the hardware description, which is why
//! switching cannot break the panel.
//!
//! Some presets are compiled in, so a fresh install has something to choose
//! from. A user file of the same name shadows a built-in one, so a shipped
//! preset can be adjusted without being lost.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use crate::config::Config;

/// The presets compiled into the driver, as `(name, contents)`.
///
/// Kept in source order rather than alphabetical: the list is a suggested
/// path through them, from the most general to the most specific.
pub static BUILTIN: &[(&str, &str)] = &[
    ("default", include_str!("../presets/default.toml")),
    ("drums", include_str!("../presets/drums.toml")),
    ("keys", include_str!("../presets/keys.toml")),
    ("mixer", include_str!("../presets/mixer.toml")),
    ("minimal", include_str!("../presets/minimal.toml")),
];

/// Where a preset came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Compiled into the driver.
    Builtin,
    /// A file in the user's presets directory.
    User,
}

/// One preset, as listed.
#[derive(Debug, Clone)]
pub struct Entry {
    /// The name used to load it.
    pub name: String,
    /// One line describing what it is for.
    pub description: String,
    /// Where it came from.
    pub origin: Origin,
    /// Whether a user file of this name shadows a built-in one.
    pub shadows_builtin: bool,
}

/// The directory user presets live in.
pub fn dir() -> PathBuf {
    Config::default_path()
        .parent()
        .map(|d| d.join("presets"))
        .unwrap_or_else(|| PathBuf::from("presets"))
}

/// Path of a user preset by name.
pub fn path_of(name: &str) -> Result<PathBuf> {
    check_name(name)?;
    Ok(dir().join(format!("{name}.toml")))
}

/// Reject a name that would escape the presets directory or confuse a shell.
///
/// Preset names arrive from a config file, a command line and eventually from
/// whatever a friend called the file they sent, so this is not a formality.
pub fn check_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("a preset name cannot be empty");
    }
    if name.len() > 64 {
        bail!("preset names are limited to 64 characters");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("preset names may only contain letters, digits, `-` and `_` (got `{name}`)");
    }
    Ok(())
}

/// Read the `[preset]` header out of a config file's text.
///
/// Done by parsing rather than by string search so a description containing a
/// bracket cannot confuse it.
fn describe(text: &str) -> String {
    text.parse::<toml::Table>()
        .ok()
        .and_then(|t| {
            t.get("preset")?
                .as_table()?
                .get("description")?
                .as_str()
                .map(str::to_string)
        })
        .unwrap_or_default()
}

/// Every preset available, built-in and user, sorted by name.
///
/// A user file shadows a built-in of the same name, and only the user one is
/// listed -- with a flag, so the interface can say so.
pub fn list() -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    let mut user_names: Vec<String> = Vec::new();

    if let Ok(rd) = std::fs::read_dir(dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("toml") {
                continue;
            }
            let Some(name) = p.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if check_name(name).is_err() {
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap_or_default();
            user_names.push(name.to_string());
            out.push(Entry {
                name: name.to_string(),
                description: describe(&text),
                origin: Origin::User,
                shadows_builtin: BUILTIN.iter().any(|(n, _)| *n == name),
            });
        }
    }

    for (name, text) in BUILTIN {
        if user_names.iter().any(|n| n == name) {
            continue;
        }
        out.push(Entry {
            name: (*name).to_string(),
            description: describe(text),
            origin: Origin::Builtin,
            shadows_builtin: false,
        });
    }

    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The text of a preset, preferring a user file over a built-in.
pub fn read(name: &str) -> Result<String> {
    check_name(name)?;
    let p = path_of(name)?;
    if p.exists() {
        return std::fs::read_to_string(&p)
            .with_context(|| format!("reading {}", p.display()));
    }
    BUILTIN
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, t)| (*t).to_string())
        .ok_or_else(|| {
            let names: Vec<&str> = list().iter().map(|_| "").collect();
            let _ = names;
            anyhow::anyhow!(
                "no preset called `{name}`. Available: {}",
                list()
                    .iter()
                    .map(|e| e.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

/// Load a preset over the live config.
///
/// The preset is validated against the device *before* anything is written, so
/// a bad file cannot leave the driver without a working config. The previous
/// config is kept as `<name>.toml.prev` so an unwanted switch is one copy away
/// from being undone.
pub fn load_into(
    name: &str,
    config_path: &Path,
    profile: &crate::profile::Profile,
) -> Result<Config> {
    let text = read(name)?;
    let cfg: Config = toml::from_str(&text)
        .with_context(|| format!("parsing preset `{name}`"))?;
    cfg.validate_against(profile)
        .with_context(|| format!("preset `{name}` does not fit this device"))?;

    if config_path.exists() {
        let backup = config_path.with_extension("toml.prev");
        let _ = std::fs::copy(config_path, &backup);
    }
    if let Some(d) = config_path.parent() {
        std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
    }
    let tmp = config_path.with_extension("toml.tmp");
    std::fs::write(&tmp, &text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, config_path)
        .with_context(|| format!("replacing {}", config_path.display()))?;
    Ok(cfg)
}

/// Save the current config as a named preset.
///
/// The `[preset]` header is rewritten so the saved file describes itself; the
/// rest of the file, comments included, is copied as it stands.
pub fn save_from(name: &str, description: &str, config_path: &Path) -> Result<PathBuf> {
    check_name(name)?;
    let text = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    // Parse before writing: saving something that cannot be loaded back is
    // worse than refusing.
    let _: Config = toml::from_str(&text).context("the current config does not parse")?;

    let body = strip_header(&text);
    let header = format!(
        "[preset]\nname = {}\ndescription = {}\n\n",
        toml_string(name),
        toml_string(description)
    );

    let out = path_of(name)?;
    if let Some(d) = out.parent() {
        std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
    }
    let tmp = out.with_extension("toml.tmp");
    std::fs::write(&tmp, header + &body).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &out).with_context(|| format!("replacing {}", out.display()))?;
    Ok(out)
}

/// Delete a user preset. Built-in presets cannot be deleted, only shadowed.
pub fn delete(name: &str) -> Result<()> {
    let p = path_of(name)?;
    if !p.exists() {
        if BUILTIN.iter().any(|(n, _)| *n == name) {
            bail!("`{name}` is built in and cannot be deleted");
        }
        bail!("no preset called `{name}`");
    }
    std::fs::remove_file(&p).with_context(|| format!("removing {}", p.display()))?;
    Ok(())
}

/// Write someone else's preset text into the user's presets directory.
pub fn import(name: &str, text: &str, profile: &crate::profile::Profile) -> Result<PathBuf> {
    check_name(name)?;
    let cfg: Config = toml::from_str(text).context("the file does not parse as a config")?;
    cfg.validate_against(profile)
        .context("the file does not fit this device")?;
    let out = path_of(name)?;
    if let Some(d) = out.parent() {
        std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
    }
    std::fs::write(&out, text).with_context(|| format!("writing {}", out.display()))?;
    Ok(out)
}

/// Remove a leading `[preset]` table, so a fresh one can be written.
fn strip_header(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_header = false;
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with('[') {
            in_header = t.starts_with("[preset]");
        }
        if !in_header {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.trim_start().to_string()
}

fn toml_string(s: &str) -> String {
    toml::Value::from(s).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::Profile;

    #[test]
    fn every_shipped_preset_parses_and_fits_the_device() {
        let profile = Profile::builtin();
        for (name, text) in BUILTIN {
            let cfg: Config = toml::from_str(text)
                .unwrap_or_else(|e| panic!("preset `{name}` does not parse: {e}"));
            cfg.validate_against(&profile)
                .unwrap_or_else(|e| panic!("preset `{name}` does not fit the device: {e:#}"));
        }
    }

    #[test]
    fn every_shipped_preset_describes_itself() {
        for (name, text) in BUILTIN {
            let d = describe(text);
            assert!(
                !d.is_empty(),
                "preset `{name}` has no description, so a chooser cannot say what it is"
            );
        }
    }

    #[test]
    fn names_that_would_escape_the_directory_are_refused() {
        for bad in ["../evil", "a/b", "", "with space", "dot.dot"] {
            assert!(check_name(bad).is_err(), "`{bad}` should be refused");
        }
        for good in ["default", "my-kit", "kit_2"] {
            check_name(good).unwrap();
        }
    }

    #[test]
    fn saving_replaces_the_header_rather_than_stacking_them() {
        let text = "[preset]\nname = \"old\"\ndescription = \"old one\"\n\n[pads]\nchannel = 3\n";
        let body = strip_header(text);
        assert!(!body.contains("[preset]"));
        assert!(body.starts_with("[pads]"));
        assert!(body.contains("channel = 3"));
    }
}
