// SPDX-License-Identifier: GPL-3.0-or-later
//! The configuration file, and its defaults.
//!
//! Replaces Packet's GSettings schema, keeping only the keys that still mean
//! something without a GUI: `device-name`, `device-visibility`,
//! `download-folder`, `enable-static-port` and `static-port-number`. Packet's
//! window-geometry, tray, autostart and plugin keys have no analogue here.
//!
//! `enable-static-port` + `static-port-number` collapse into one optional
//! `port`: a boolean guarding a number is a GUI artifact (a switch and a spin
//! button), and in a text file `# port = 45000` says the same thing.

use std::net::TcpListener;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// The file name inside `$XDG_CONFIG_HOME/quickshare-daemon/`.
const FILE_NAME: &str = "config.toml";

/// Written to a fresh config file, above the serialised defaults.
///
/// A generated file with no explanation is a file people are afraid to edit,
/// and there is no settings UI to discover these in.
const FILE_HEADER: &str = "\
# quickshare-daemon configuration.
#
# Reload after editing with `systemctl --user reload quickshare-daemon`
# (or send SIGHUP). Changing device_name, visible or port restarts the
# Quick Share service; anything else applies in place.
#
# Defaults are shown. Commented-out keys have no default.

";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// The name shown on other devices' Quick Share sheets.
    pub device_name: String,

    /// Whether to be discoverable by everyone nearby.
    ///
    /// `rqs_lib` also has a `Temporarily` visibility, which is not exposed:
    /// "visible for a while" needs a UI to start the clock and show what is
    /// left of it, and a config file is a bad place to put a stopwatch.
    pub visible: bool,

    /// Where received files are written.
    pub download_dir: PathBuf,

    /// A fixed TCP port, or `None` for a random one.
    ///
    /// Worth setting if a firewall rule needs a stable port to allow. Note
    /// that `rqs_lib` reuses whatever port it last bound across a service
    /// restart, even when this is `None`.
    pub port: Option<u16>,

    /// How long an unanswered transfer request waits before being declined.
    ///
    /// The transfer is declined rather than left hanging: the sending phone
    /// shows a spinner until it gets an answer, so silence is the least
    /// helpful response.
    pub accept_timeout_secs: u64,

    /// Device names whose transfers are accepted without asking.
    ///
    /// Matched against the sender's advertised name, which it chooses and
    /// which nothing verifies. Convenience only: do not treat a match here as
    /// proof of who is sending.
    pub auto_accept_from: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device_name: default_device_name(),
            visible: true,
            download_dir: default_download_dir(),
            port: None,
            accept_timeout_secs: 60,
            auto_accept_from: Vec::new(),
        }
    }
}

impl Config {
    /// Loads the config, creating it with defaults if absent.
    ///
    /// Returns the config and the path it came from, so callers can name the
    /// file in log messages without recomputing the path.
    pub fn load_or_create() -> Result<(Self, PathBuf)> {
        let path = Self::path()?;

        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let config: Self =
                    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
                config.validate()?;
                Ok((config, path))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let config = Self::default();
                // Best-effort: a read-only config directory is a reason to
                // warn, not to refuse to run with defaults.
                if let Err(err) = config.write_to(&path) {
                    tracing::warn!(
                        "could not write a default config to {}: {err:#}",
                        path.display()
                    );
                } else {
                    tracing::info!("wrote a default config to {}", path.display());
                }
                config.validate()?;
                Ok((config, path))
            }
            Err(err) => Err(anyhow::Error::new(err).context(format!("reading {}", path.display()))),
        }
    }

    /// `$XDG_CONFIG_HOME/quickshare-daemon/config.toml`.
    pub fn path() -> Result<PathBuf> {
        Ok(dirs::config_dir()
            .context("could not determine the XDG config directory")?
            .join(env!("CARGO_PKG_NAME"))
            .join(FILE_NAME))
    }

    /// Serialises to `path`, creating parent directories.
    fn write_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let body = toml::to_string_pretty(self).context("serialising the config")?;
        std::fs::write(path, format!("{FILE_HEADER}{body}"))
            .with_context(|| format!("writing {}", path.display()))
    }

    /// Rejects values that would fail later, in a less obvious place.
    fn validate(&self) -> Result<()> {
        if self.device_name.trim().is_empty() {
            bail!("device_name is empty; other devices would show a blank entry");
        }
        // Quick Share sends the name over the wire in the endpoint info, where
        // it is length-prefixed with a single byte.
        if self.device_name.len() > 255 {
            bail!(
                "device_name is {} bytes; the protocol allows at most 255",
                self.device_name.len()
            );
        }
        if self.accept_timeout_secs == 0 {
            bail!("accept_timeout_secs is 0; every transfer would be declined instantly");
        }
        Ok(())
    }

    /// Whether a reloaded config needs the Quick Share service restarted.
    ///
    /// Only the three values baked in at `RQS::new`/`run` time need one;
    /// `accept_timeout_secs` and `auto_accept_from` are read per transfer, and
    /// `download_dir` has a setter. Used by the SIGHUP path (PLAN.md Phase 2).
    // Kept ahead of its caller deliberately: it is the executable statement of
    // which settings are baked in at service-start time, it is covered by
    // tests, and Phase 2 wires it to SIGHUP.
    #[allow(dead_code)]
    pub fn needs_service_restart(&self, previous: &Self) -> bool {
        self.device_name != previous.device_name
            || self.visible != previous.visible
            || self.port != previous.port
    }

    /// Creates `download_dir` if it does not exist.
    ///
    /// `rqs_lib` writes into it without checking, so a missing directory would
    /// otherwise surface as a failure partway through a transfer.
    pub fn ensure_download_dir(&self) -> Result<()> {
        std::fs::create_dir_all(&self.download_dir)
            .with_context(|| format!("creating {}", self.download_dir.display()))
    }

    /// Whether this sender's transfers should skip the consent prompt.
    pub fn is_auto_accepted(&self, sender_name: &str) -> bool {
        self.auto_accept_from
            .iter()
            .any(|allowed| allowed == sender_name)
    }

    /// Checks that `port`, if fixed, can actually be bound.
    ///
    /// Only useful as a diagnostic: `rqs_lib` binds the port itself, and
    /// between this check and that bind the port could be taken. Reporting
    /// "port 45000 is already in use" at startup still beats the bind failing
    /// inside the library.
    pub fn port_is_available(&self) -> Option<bool> {
        let port = self.port?;
        Some(TcpListener::bind(("0.0.0.0", port)).is_ok())
    }
}

/// The hostname, without its domain, falling back to a generic name.
///
/// `rqs_lib` would default to the hostname too, but writing it into the config
/// file makes it visible and editable instead of mysterious.
///
/// The domain is stripped because this string is shown on a phone's share
/// sheet: a DHCP-assigned FQDN like `dell.fritz.box` is what the machine is
/// called on the network, not what its owner calls it.
fn default_device_name() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|name| {
            let name = name.trim();
            name.split_once('.')
                .map_or(name, |(host, _domain)| host)
                .to_owned()
        })
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Linux device".to_owned())
}

/// The XDG download directory, falling back to `$HOME/Downloads`.
fn default_download_dir() -> PathBuf {
    dirs::download_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join("Downloads")))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_device_name_drops_the_domain() {
        // The real machine this was developed on reports `dell.fritz.box`,
        // which is not a name anyone would choose to see on their phone.
        let name = default_device_name();
        assert!(!name.contains('.'), "got {name:?}");
        assert!(!name.is_empty());
    }

    #[test]
    fn defaults_round_trip_through_toml() {
        let config = Config::default();
        let text = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn an_empty_file_yields_the_defaults() {
        // Every key is optional, so an empty file must be valid: it is what a
        // user who deleted a line they did not understand ends up with.
        let parsed: Config = toml::from_str("").unwrap();
        assert_eq!(parsed, Config::default());
    }

    #[test]
    fn the_documented_example_parses() {
        // The exact example from PLAN.md, minus the tilde path, which is a
        // shell convenience and not something TOML expands.
        let parsed: Config = toml::from_str(
            r#"
            device_name = "alex-laptop"
            visible = true
            download_dir = "~/Downloads"
            port = 45000
            accept_timeout_secs = 60
            auto_accept_from = ["Pixel 9"]
            "#,
        )
        .unwrap();

        assert_eq!(parsed.device_name, "alex-laptop");
        assert_eq!(parsed.port, Some(45000));
        assert!(parsed.is_auto_accepted("Pixel 9"));
        assert!(!parsed.is_auto_accepted("Someone Else's Pixel"));
    }

    #[test]
    fn unknown_keys_are_an_error() {
        // A typo that is silently ignored is a setting that silently does not
        // apply, which is worse than a startup failure naming the line.
        let err = toml::from_str::<Config>("devicename = \"x\"").unwrap_err();
        assert!(err.to_string().contains("devicename"), "{err}");
    }

    #[test]
    fn validation_rejects_unusable_values() {
        let blank = Config {
            device_name: "  ".into(),
            ..Config::default()
        };
        assert!(blank.validate().is_err());

        let too_long = Config {
            device_name: "n".repeat(256),
            ..Config::default()
        };
        assert!(too_long.validate().is_err());

        let instant_decline = Config {
            accept_timeout_secs: 0,
            ..Config::default()
        };
        assert!(instant_decline.validate().is_err());

        assert!(Config::default().validate().is_ok());
    }

    #[test]
    fn only_service_level_changes_need_a_restart() {
        let base = Config::default();

        for changed in [
            Config {
                device_name: "other".into(),
                ..base.clone()
            },
            Config {
                visible: !base.visible,
                ..base.clone()
            },
            Config {
                port: Some(45000),
                ..base.clone()
            },
        ] {
            assert!(changed.needs_service_restart(&base), "{changed:?}");
        }

        for unchanged in [
            Config {
                accept_timeout_secs: base.accept_timeout_secs + 1,
                ..base.clone()
            },
            Config {
                auto_accept_from: vec!["Pixel 9".into()],
                ..base.clone()
            },
            Config {
                download_dir: "/tmp/elsewhere".into(),
                ..base.clone()
            },
        ] {
            assert!(!unchanged.needs_service_restart(&base), "{unchanged:?}");
        }
    }

    #[test]
    fn a_default_config_file_is_written_and_read_back() {
        let dir = std::env::temp_dir().join(format!("qsd-config-test-{}", std::process::id()));
        let path = dir.join("config.toml");
        let config = Config::default();

        config.write_to(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();

        // The header must be comments only, or it would not parse back.
        assert!(text.starts_with("# quickshare-daemon configuration."));
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), config);

        std::fs::remove_dir_all(&dir).ok();
    }
}
