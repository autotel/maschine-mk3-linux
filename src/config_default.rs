//! The starter configuration.
//!
//! Shipped as commented TOML rather than as serialised defaults, because the
//! file is meant to be read and edited by hand and `toml::to_string` throws
//! comments away. A unit test parses it, so the comments cannot drift out of
//! sync with the fields the driver actually accepts.

use crate::config::Config;

/// The commented starter file, written when no config exists yet.
pub const STARTER_TOML: &str = include_str!("../config/default.toml");

/// Parse [`STARTER_TOML`].
///
/// Panics only if the shipped file is malformed, which the test below rules out.
pub fn starter() -> Config {
    toml::from_str(STARTER_TOML).expect("shipped config/default.toml must parse")
}

/// Write the commented starter file to `path`, creating parent directories.
///
/// This writes the text verbatim rather than re-serialising [`starter`], so
/// the user's first config keeps every comment explaining what the fields do.
pub fn install(path: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(path, STARTER_TOML).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn shipped_default_is_valid() {
        let c = super::starter();
        c.validate().expect("shipped config must validate");
    }

    #[test]
    fn recording_buttons_does_not_strip_the_shipped_comments() {
        use crate::config::{ButtonCfg, Config};
        use std::collections::BTreeMap;

        let dir = std::env::temp_dir().join(format!("mk3doc{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, super::STARTER_TOML).unwrap();

        let count_comments = |t: &str| t.lines().filter(|l| l.trim_start().starts_with('#')).count();
        let before = count_comments(super::STARTER_TOML);
        assert!(before > 50, "the shipped file is mostly documentation");

        let mut buttons = BTreeMap::new();
        buttons.insert(
            "play".to_string(),
            ButtonCfg { bit: 45, led: -1, midi: "cc 1 118".into(), ..Default::default() },
        );
        buttons.insert(
            "channel_midi".to_string(),
            ButtonCfg { bit: 56, led: 12, ..Default::default() },
        );
        Config::write_buttons_preserving(&path, &buttons).unwrap();

        let after_text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            count_comments(&after_text),
            before,
            "a discovery run must not eat the file's documentation"
        );
        let back = Config::load(&path).unwrap();
        back.validate().unwrap();
        assert_eq!(back.button.len(), 2);
        assert_eq!(back.button["play"].bit, 45);
        assert_eq!(back.button["channel_midi"].led, 12);
        // And a second run is idempotent.
        Config::write_buttons_preserving(&path, &back.button).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), after_text);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn shipped_default_button_actions_all_parse() {
        let c = super::starter();
        for (name, b) in &c.button {
            crate::config::Action::parse(&b.midi)
                .unwrap_or_else(|e| panic!("button.{name}.midi: {e}"));
        }
    }
}
