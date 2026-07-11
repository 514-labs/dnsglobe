//! Optional TOML config file that adds resolvers to the built-in list, or
//! replaces the list entirely (e.g. to check propagation across your own
//! infrastructure, or an internal split-horizon zone).
//!
//! Looked up at `$DNSGLOBE_CONFIG`, else `$XDG_CONFIG_HOME/dnsglobe/config.toml`,
//! else `~/.config/dnsglobe/config.toml`. A missing default file just means
//! the built-in list; a missing `$DNSGLOBE_CONFIG` file is an error.

use std::net::IpAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::app::ViewMode;
use crate::resolvers::{self, Resolver};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// When true, the config's resolvers replace the built-in list instead of
    /// extending it.
    #[serde(default)]
    replace: bool,
    /// Preferred map panel style; the --view flag overrides it.
    view: Option<ViewMode>,
    #[serde(default)]
    resolvers: Vec<Entry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    name: String,
    ip: String,
    /// Shown in the table's Loc column and used by the location sort.
    #[serde(default)]
    location: String,
    /// Optional map position; give both or neither. Without them the
    /// resolver is queried normally but not drawn on the world map.
    lat: Option<f64>,
    lon: Option<f64>,
}

/// Everything the config file contributes to a run.
pub struct Settings {
    pub resolvers: Vec<Resolver>,
    pub view: Option<ViewMode>,
}

impl Settings {
    fn defaults() -> Self {
        Self {
            resolvers: resolvers::defaults(),
            view: None,
        }
    }
}

/// Load run settings: the resolver list (built-ins plus, or replaced by, the
/// config file) and the preferred view, if a config file exists.
pub fn load() -> Result<Settings> {
    let (path, required) = match std::env::var_os("DNSGLOBE_CONFIG") {
        Some(path) => (Some(PathBuf::from(path)), true),
        None => (default_path(), false),
    };
    let Some(path) = path else {
        return Ok(Settings::defaults());
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && !required => {
            return Ok(Settings::defaults());
        }
        Err(err) => {
            return Err(err).with_context(|| format!("reading config file {}", path.display()));
        }
    };
    let config: Config =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let view = config.view;
    let resolvers =
        resolver_list(config).with_context(|| format!("invalid config {}", path.display()))?;
    Ok(Settings { resolvers, view })
}

fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("dnsglobe").join("config.toml"))
}

/// Validate the config and merge it with the built-in list.
fn resolver_list(config: Config) -> Result<Vec<Resolver>> {
    let mut list = if config.replace {
        Vec::new()
    } else {
        resolvers::defaults()
    };
    for entry in config.resolvers {
        let ip: IpAddr = entry.ip.parse().with_context(|| {
            format!(
                "resolver {:?}: invalid IP address {:?}",
                entry.name, entry.ip
            )
        })?;
        let coords = match (entry.lat, entry.lon) {
            (Some(lat), Some(lon)) => {
                if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
                    bail!(
                        "resolver {:?}: lat must be in -90..=90 and lon in -180..=180",
                        entry.name
                    );
                }
                Some((lat, lon))
            }
            (None, None) => None,
            _ => bail!(
                "resolver {:?}: lat and lon must be given together",
                entry.name
            ),
        };
        list.push(Resolver {
            name: entry.name,
            location: entry.location,
            ip,
            coords,
            probe: None,
        });
    }
    if list.is_empty() {
        bail!("`replace = true` needs at least one [[resolvers]] entry");
    }
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(toml_text: &str) -> Result<Vec<Resolver>> {
        resolver_list(toml::from_str(toml_text)?)
    }

    #[test]
    fn no_config_keeps_builtin_list() {
        let resolvers = list("").unwrap();
        assert_eq!(resolvers.len(), resolvers::defaults().len());
    }

    #[test]
    fn added_resolvers_extend_the_builtin_list() {
        let resolvers = list(
            r#"
            [[resolvers]]
            name = "Corp DNS"
            ip = "10.0.0.53"
            location = "HQ"
            lat = 40.7
            lon = -74.0

            [[resolvers]]
            name = "No map"
            ip = "2606:4700:4700::1111"
            "#,
        )
        .unwrap();
        assert_eq!(resolvers.len(), resolvers::defaults().len() + 2);
        let corp = &resolvers[resolvers.len() - 2];
        assert_eq!(corp.name, "Corp DNS");
        assert_eq!(corp.location, "HQ");
        assert_eq!(corp.ip, "10.0.0.53".parse::<IpAddr>().unwrap());
        assert_eq!(corp.coords, Some((40.7, -74.0)));
        // IPv6 works; omitted location/coords stay empty/off-map.
        let no_map = &resolvers[resolvers.len() - 1];
        assert!(no_map.ip.is_ipv6());
        assert_eq!(no_map.location, "");
        assert_eq!(no_map.coords, None);
    }

    #[test]
    fn replace_swaps_out_the_builtin_list() {
        let resolvers = list(
            r#"
            replace = true

            [[resolvers]]
            name = "Only me"
            ip = "192.0.2.1"
            "#,
        )
        .unwrap();
        assert_eq!(resolvers.len(), 1);
        assert_eq!(resolvers[0].name, "Only me");
    }

    #[test]
    fn replace_with_no_resolvers_is_an_error() {
        let err = list("replace = true").unwrap_err();
        assert!(err.to_string().contains("at least one"));
    }

    #[test]
    fn invalid_ip_is_an_error() {
        let err = list(
            r#"
            [[resolvers]]
            name = "Bad"
            ip = "not-an-ip"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("invalid IP address"));
    }

    #[test]
    fn lat_without_lon_is_an_error() {
        let err = list(
            r#"
            [[resolvers]]
            name = "Half a coordinate"
            ip = "192.0.2.1"
            lat = 12.0
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("given together"));
    }

    #[test]
    fn out_of_range_coords_are_an_error() {
        let err = list(
            r#"
            [[resolvers]]
            name = "Off the globe"
            ip = "192.0.2.1"
            lat = 91.0
            lon = 0.0
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("-90..=90"));
    }

    #[test]
    fn view_parses_all_modes_and_rejects_typos() {
        for (text, want) in [
            ("view = \"auto\"", ViewMode::Auto),
            ("view = \"map\"", ViewMode::Map),
            ("view = \"globe\"", ViewMode::Globe),
        ] {
            let config: Config = toml::from_str(text).unwrap();
            assert_eq!(config.view, Some(want));
        }
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.view, None);
        assert!(toml::from_str::<Config>("view = \"sphere\"").is_err());
    }

    #[test]
    fn unknown_keys_are_rejected_to_catch_typos() {
        assert!(toml::from_str::<Config>("replase = true").is_err());
        assert!(
            toml::from_str::<Config>(
                r#"
                [[resolvers]]
                name = "Typo"
                adress = "192.0.2.1"
                "#,
            )
            .is_err()
        );
    }
}
