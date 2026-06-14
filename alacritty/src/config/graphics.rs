use serde::Serialize;

use alacritty_config_derive::ConfigDeserialize;
use alacritty_terminal::graphics::GraphicsOptions;

/// Struct for graphics protocol related settings.
#[derive(ConfigDeserialize, Serialize, Copy, Clone, Debug, PartialEq, Eq)]
pub struct Graphics {
    /// Master switch for all terminal graphics protocols.
    pub enabled: bool,

    /// Kitty graphics protocol support.
    pub kitty_protocol: bool,

    /// Sixel graphics support.
    pub sixel: bool,

    /// iTerm2 inline image support.
    pub iterm2: bool,

    /// Storage quota for decoded image data, in mebibytes.
    pub max_storage_mib: u64,
}

impl Default for Graphics {
    fn default() -> Self {
        Self {
            enabled: true,
            kitty_protocol: true,
            sixel: true,
            iterm2: true,
            max_storage_mib: 320,
        }
    }
}

impl Graphics {
    /// Derive the terminal-level [`GraphicsOptions`] from the config.
    pub fn options(&self) -> GraphicsOptions {
        let max_storage_mib = usize::try_from(self.max_storage_mib).unwrap_or(usize::MAX);
        GraphicsOptions {
            enabled: self.enabled,
            kitty_protocol: self.kitty_protocol,
            sixel: self.sixel,
            iterm2: self.iterm2,
            max_storage: max_storage_mib.saturating_mul(1024 * 1024),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_graphics_config() {
        let graphics = toml::from_str::<Graphics>("").unwrap();

        assert!(graphics.enabled);
        assert!(graphics.kitty_protocol);
        assert!(graphics.sixel);
        assert!(graphics.iterm2);
        assert_eq!(graphics.max_storage_mib, 320);

        let options = graphics.options();
        assert!(options.kitty_enabled());
        assert!(options.sixel_enabled());
        assert!(options.iterm2_enabled());
        assert_eq!(options.max_storage, 320 * 1024 * 1024);
    }

    #[test]
    fn explicit_graphics_config() {
        let graphics = toml::from_str::<Graphics>(
            "enabled = false\nkitty_protocol = false\nsixel = false\niterm2 = \
             false\nmax_storage_mib = 64\n",
        )
        .unwrap();

        assert!(!graphics.enabled);
        assert!(!graphics.kitty_protocol);
        assert!(!graphics.sixel);
        assert!(!graphics.iterm2);
        assert_eq!(graphics.max_storage_mib, 64);

        let options = graphics.options();
        assert!(!options.kitty_enabled());
        assert_eq!(options.max_storage, 64 * 1024 * 1024);
    }

    #[test]
    fn master_switch_overrides_protocol_toggles() {
        let graphics = toml::from_str::<Graphics>("enabled = false").unwrap();
        let options = graphics.options();

        assert!(options.kitty_protocol);
        assert!(!options.kitty_enabled());
        assert!(!options.sixel_enabled());
        assert!(!options.iterm2_enabled());
    }
}
