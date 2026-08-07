//! UI color roles, overridable from the config file's `[theme]` table.
//!
//! The defaults favor bright (90-range) ANSI colors and the faint attribute
//! over dark (30-range) colors and DarkGray: on terminal themes with a
//! mid-toned background (macOS Terminal's "Ocean" blue), the dark palette is
//! illegible, while a theme's own default foreground — which faint dims — is
//! always readable against its background (issue #25).

use std::str::FromStr;
use std::sync::OnceLock;

use anyhow::{Result, anyhow};
use ratatui::style::{Color, Modifier, Style};

/// De-emphasized text. Faint dims whatever the terminal theme's default
/// foreground is, so it survives any background; a fixed color is the
/// escape hatch for terminals that render faint poorly (legacy conhost).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Muted {
    Faint,
    Color(Color),
}

impl Muted {
    pub fn style(self) -> Style {
        match self {
            Self::Faint => Style::new().add_modifier(Modifier::DIM),
            Self::Color(color) => Style::new().fg(color),
        }
    }
}

/// A role that can carry a background as well as a foreground, written
/// `"<fg> on <bg>"` in the config. Only `error` needs one today: a red
/// foreground is exactly what washes out against a mid-toned terminal
/// background, while white-on-red survives any theme (issue #33). The type is
/// role-agnostic so another role can adopt it without new parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Paint {
    pub fg: Color,
    pub bg: Option<Color>,
}

impl Paint {
    pub const fn color(fg: Color) -> Self {
        Self { fg, bg: None }
    }

    pub const fn on(fg: Color, bg: Color) -> Self {
        Self { fg, bg: Some(bg) }
    }

    /// Background included — for the badge-sized places where a filled block
    /// reads as a marker: the row's status glyph and word.
    pub fn style(self) -> Style {
        let style = Style::new().fg(self.fg);
        match self.bg {
            Some(bg) => style.bg(bg),
            None => style,
        }
    }

    /// The role's own color. When it renders as a badge that's the
    /// *background*: what a filled marker actually shows — "white" alone
    /// would say nothing on a map dot.
    pub fn hue(self) -> Color {
        self.bg.unwrap_or(self.fg)
    }

    /// The hue as a plain foreground — for map dots, gauge fills, legend
    /// swatches and prose, where a background would paint a garish block
    /// behind a single glyph or drag a bar across a whole line.
    pub fn tint(self) -> Style {
        Style::new().fg(self.hue())
    }
}

/// One color per UI meaning; every style in `ui.rs` draws from these, so a
/// role recolors consistently everywhere it appears (table, map dot, legend).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// Borders, titles, the input cursor, discovered anycast sites.
    pub accent: Color,
    /// Answers matching the majority; doubles as the fast-latency color.
    pub agree: Color,
    /// Answers disagreeing with the majority.
    pub differ: Color,
    /// Query failures (ERR / SERVFAIL / NONE); doubles as slow latency.
    pub error: Paint,
    /// Queries still in flight; doubles as middling latency.
    pub pending: Color,
    /// A cache serving an answer past its own TTL.
    pub stale: Color,
    /// Refetched, but upstream still serves the old data.
    pub upstream: Color,
    /// Labels, hints, countdowns, and borders around quiet panels.
    pub muted: Muted,
    /// Map and globe land outline.
    pub coastline: Color,
    /// Globe graticule and limb; dimmer than the coastline so the
    /// continents stay in front.
    pub grid: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            accent: Color::LightCyan,
            agree: Color::LightGreen,
            differ: Color::LightMagenta,
            // White on bright red: the failure badge has to stay legible on
            // any background, and a lone red foreground doesn't (issue #33).
            // Only the badge takes the background — see `Paint::tint`.
            error: Paint::on(Color::White, Color::LightRed),
            pending: Color::LightYellow,
            // Orange, to stay distinct from `error` red now that errors are
            // bright. Indexed: 8-color terminals will approximate it.
            stale: Color::Indexed(208),
            upstream: Color::LightBlue,
            muted: Muted::Faint,
            coastline: Color::Gray,
            // Indexed so it degrades to something readable on 256-color
            // terminals; true 8-color ones will approximate.
            grid: Color::Indexed(244),
        }
    }
}

/// The theme for this run. Set once at startup (after the config file is
/// applied); falls back to the defaults so tests need no setup.
static ACTIVE: OnceLock<Theme> = OnceLock::new();

/// Install the theme for this run. Must be called before the first
/// `active()` call and at most once.
pub fn init(theme: Theme) {
    ACTIVE.set(theme).expect("theme initialized more than once");
}

pub fn active() -> &'static Theme {
    ACTIVE.get_or_init(Theme::default)
}

/// Parse a config color: an ANSI name, a 256-color index, or `#RRGGBB` hex
/// (ratatui's `FromStr` accepts all three, plus aliases like "bright red").
pub fn parse_color(s: &str) -> Result<Color> {
    Color::from_str(s).map_err(|_| {
        anyhow!(
            "unrecognized color {s:?} (expected an ANSI name like \"lightcyan\", \
             a 256-color index like \"208\", or hex like \"#ff8700\")"
        )
    })
}

/// Parse a role that may name a background: `"white on red"`, or a single
/// color for foreground-only. The separator is a spaced " on " so ratatui's
/// two-word aliases still parse ("bright white on bright red").
pub fn parse_paint(s: &str) -> Result<Paint> {
    match s.split_once(" on ") {
        Some((fg, bg)) => Ok(Paint::on(parse_color(fg.trim())?, parse_color(bg.trim())?)),
        None => Ok(Paint::color(parse_color(s)?)),
    }
}

/// The `muted` role additionally accepts "faint"/"dim" (the default).
pub fn parse_muted(s: &str) -> Result<Muted> {
    match s {
        "faint" | "dim" => Ok(Muted::Faint),
        _ => parse_color(s).map(Muted::Color),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_parse_as_names_indexes_and_hex() {
        assert_eq!(parse_color("lightcyan").unwrap(), Color::LightCyan);
        assert_eq!(parse_color("bright red").unwrap(), Color::LightRed);
        assert_eq!(parse_color("208").unwrap(), Color::Indexed(208));
        assert_eq!(
            parse_color("#ff8700").unwrap(),
            Color::Rgb(0xff, 0x87, 0x00)
        );
    }

    #[test]
    fn bad_color_names_the_expected_forms() {
        let err = parse_color("ocean").unwrap_err().to_string();
        assert!(err.contains("\"ocean\""));
        assert!(err.contains("ANSI name"));
    }

    #[test]
    fn muted_accepts_faint_dim_or_any_color() {
        assert_eq!(parse_muted("faint").unwrap(), Muted::Faint);
        assert_eq!(parse_muted("dim").unwrap(), Muted::Faint);
        assert_eq!(
            parse_muted("darkgray").unwrap(),
            Muted::Color(Color::DarkGray)
        );
        assert!(parse_muted("blurple").is_err());
    }

    #[test]
    fn paint_parses_with_or_without_a_background() {
        assert_eq!(
            parse_paint("lightred").unwrap(),
            Paint::color(Color::LightRed)
        );
        assert_eq!(
            parse_paint("white on bright red").unwrap(),
            Paint::on(Color::White, Color::LightRed)
        );
        assert_eq!(
            parse_paint("#ffffff on 196").unwrap(),
            Paint::on(Color::Rgb(0xff, 0xff, 0xff), Color::Indexed(196))
        );
        // Both halves are validated, and the bad one is named.
        assert!(
            parse_paint("white on ocean")
                .unwrap_err()
                .to_string()
                .contains("\"ocean\"")
        );
        assert!(parse_paint("ocean on red").is_err());
    }

    #[test]
    fn badges_paint_a_background_but_tints_show_the_hue() {
        let badge = Paint::on(Color::White, Color::LightRed);
        assert_eq!(badge.style().fg, Some(Color::White));
        assert_eq!(badge.style().bg, Some(Color::LightRed));
        // A map dot or gauge fill wants the red, not the white text on it.
        assert_eq!(badge.tint().fg, Some(Color::LightRed));
        assert_eq!(badge.tint().bg, None);

        let plain = Paint::color(Color::LightRed);
        assert_eq!(plain.style().bg, None);
        assert_eq!(plain.tint().fg, Some(Color::LightRed));
    }

    #[test]
    fn faint_style_carries_no_color() {
        let style = Muted::Faint.style();
        assert_eq!(style.fg, None);
        assert!(style.add_modifier.contains(Modifier::DIM));
        assert_eq!(
            Muted::Color(Color::DarkGray).style().fg,
            Some(Color::DarkGray)
        );
    }
}
