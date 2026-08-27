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
    fn shipped_default_matches_the_shipped_profile() {
        use crate::profile::Profile;
        let c = super::starter();
        let p = Profile::builtin();
        // Every name in the config must exist on the device, or the control
        // silently never works.
        c.validate_against(&p).expect("shipped config must match the profile");
        // ...and every button the device has should be accounted for, so a
        // fresh install has a working panel rather than a mostly dead one.
        for (name, _) in p.buttons() {
            assert!(
                c.buttons.contains_key(name),
                "the shipped config says nothing about `{name}`"
            );
        }
    }

    #[test]
    fn editing_the_config_keeps_its_comments() {
        use crate::config::Config;
        let dir = std::env::temp_dir().join(format!("mk3doc{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, super::STARTER_TOML).unwrap();

        let count = |t: &str| t.lines().filter(|l| l.trim_start().starts_with('#')).count();
        let before = count(super::STARTER_TOML);
        assert!(before > 50, "the shipped file is mostly documentation");

        let mut c = Config::load(&path).unwrap();
        c.pads.channel = 3;
        c.save_preserving(&path).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(count(&after), before, "editing must not eat the documentation");
        assert_eq!(Config::load(&path).unwrap().pads.channel, 3);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn shipped_default_button_actions_all_parse() {
        let c = super::starter();
        for (name, b) in &c.buttons {
            crate::config::Action::parse(&b.resolve().send)
                .unwrap_or_else(|e| panic!("buttons.{name}: {e}"));
        }
    }
}
