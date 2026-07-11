//! Orthographic globe projection and the flat↔globe morph.
//!
//! The globe is drawn into the same canvas coordinate space as the flat map
//! (x = lon −170..180, y = lat −55..72, in degrees). That space is what makes
//! the morph work: `project` returns the flat position at t=0, the globe
//! position at t=1, and a straight interpolation between the two in between,
//! so every coastline point and resolver dot slides from its map position to
//! its place on the sphere. The map area is already sized so one degree spans
//! the same number of braille dots in x and y (see `MAP_ASPECT` in ui.rs),
//! which is also exactly what keeps the globe's limb circular.

use std::time::{Duration, Instant};

/// Globe center in canvas coordinates: the midpoint of the flat map bounds.
pub const CENTER_X: f64 = 5.0; // (−170 + 180) / 2
pub const CENTER_Y: f64 = 8.5; // (−55 + 72) / 2
/// Sphere radius in canvas degrees; just inside the 127° latitude span.
pub const RADIUS: f64 = 60.0;

/// Axial tilt toward the viewer. Pitching the north pole into view reads as
/// "a planet" rather than "a circle with edges", and brings the high-lat
/// resolver clusters (Europe, North America) away from the limb where
/// orthographic foreshortening would squash them together.
const TILT_DEG: f64 = 15.0;

/// One full revolution. Slow enough to follow a dot across the disc, fast
/// enough that the far hemisphere never feels out of reach; at the event
/// loop's 100ms tick this advances 1°/frame, which braille resolves cleanly.
const SECS_PER_REV: f64 = 36.0;

/// Flat↔globe morph duration.
const TRANSITION: Duration = Duration::from_millis(700);

/// Flat map canvas bounds: lon −170..180, lat −55..72 (poles cropped).
pub const MAP_LON_SPAN: f64 = 350.0;
pub const MAP_LAT_SPAN: f64 = 127.0;
/// Rows per column that keep the flat projection square: braille dots are
/// ~square in a 1:2 terminal font, and a cell is 2 dots wide × 4 tall, so
/// rows = cols × (lat/lon span) × 2/4. Sizing the map by this instead of
/// filling available height is what keeps the continents recognizable.
pub const MAP_ASPECT: f64 = MAP_LAT_SPAN / MAP_LON_SPAN * 2.0 / 4.0;
pub const MAP_MAX_WIDTH: u16 = 170;
/// Panel rows kept below the globe for the legend/majority-answer box.
const INFO_RESERVE: u16 = 4;

/// Map panel dimensions and canvas zoom, all interpolated by the morph so
/// the panel itself reshapes with the transition.
pub struct PanelGeom {
    pub width: u16,
    pub height: u16,
    /// Canvas longitude span. The flat map shows the full 350°; the globe
    /// zooms in until its disc fills the panel. Derived from the panel's
    /// dot grid so a degree stays square (and the limb circular).
    pub x_span: f64,
    pub t: f64,
}

impl PanelGeom {
    pub fn x_bounds(&self) -> [f64; 2] {
        [CENTER_X - self.x_span / 2.0, CENTER_X + self.x_span / 2.0]
    }

    pub fn y_bounds(&self) -> [f64; 2] {
        [CENTER_Y - MAP_LAT_SPAN / 2.0, CENTER_Y + MAP_LAT_SPAN / 2.0]
    }
}

/// Size the map panel at morph parameter `t`, given the columns available to
/// it and the body height. The flat endpoint is the classic wide panel; the
/// globe endpoint is a square dot grid (2 braille dots per cell horizontally,
/// 4 vertically → cell height ≈ half the width) just big enough for the disc,
/// so the globe earns its keep on narrow terminals instead of floating small
/// inside a map-shaped canvas. Between the endpoints everything lerps:
/// width, height, and zoom animate together with the coastline morph.
pub fn panel_geometry(avail_width: u16, body_height: u16, t: f64) -> PanelGeom {
    // Floors keep the geometry sane (and division-safe) on degenerate
    // terminal sizes; ceilings are lifted to the floor so clamp can't panic.
    let flat_w = avail_width.clamp(6, MAP_MAX_WIDTH);
    let flat_h =
        ((f64::from(flat_w - 2) * MAP_ASPECT).round() as u16 + 2).clamp(4, body_height.max(4));
    // Square dot grid for the globe, height-capped so the info box below
    // keeps its rows on wide-but-short terminals.
    let globe_h = ((flat_w - 2) / 2 + 2).clamp(4, body_height.saturating_sub(INFO_RESERVE).max(4));
    let globe_w = (2 * (globe_h - 2) + 2).clamp(6, flat_w);
    // Degrees per dot equal in x and y ⇒ round limb, whatever the clamps did.
    let globe_span = MAP_LAT_SPAN * f64::from(globe_w - 2) / (2.0 * f64::from(globe_h - 2));

    let lerp = |a: f64, b: f64| a + (b - a) * t;
    PanelGeom {
        width: lerp(f64::from(flat_w), f64::from(globe_w)).round() as u16,
        height: lerp(f64::from(flat_h), f64::from(globe_h)).round() as u16,
        x_span: lerp(MAP_LON_SPAN, globe_span),
        t,
    }
}

/// Project a (lon, lat) point at morph parameter `t` (0 = flat map,
/// 1 = globe centered on `center_lon`). Returns the canvas position, or None
/// when the point has rotated onto the far hemisphere. The visibility cutoff
/// eases with `t` — depth runs −1 (antipode) to 1 (face center), and a point
/// is kept while `depth ≥ t − 1` — so back-side points don't vanish at once
/// when the morph starts: they drop out progressively, antipode first.
pub fn project(lon: f64, lat: f64, center_lon: f64, t: f64) -> Option<(f64, f64)> {
    let lambda = (lon - center_lon).to_radians();
    let phi = lat.to_radians();
    let (sx, sy, sz) = (
        phi.cos() * lambda.sin(),
        phi.sin(),
        phi.cos() * lambda.cos(),
    );
    // Tilt = rotation about the x axis; the pole moves up-front (+y, +z),
    // the face center dips down (−y).
    let (tilt_sin, tilt_cos) = TILT_DEG.to_radians().sin_cos();
    let y = sy * tilt_cos - sz * tilt_sin;
    let depth = sy * tilt_sin + sz * tilt_cos;
    if depth < t - 1.0 {
        return None;
    }
    let gx = CENTER_X + RADIUS * sx;
    let gy = CENTER_Y + RADIUS * y;
    Some((lon + (gx - lon) * t, lat + (gy - lat) * t))
}

/// Flat↔globe view state: the morph target, its progress, and the spin phase.
/// All time-dependent so the caller passes `now` — keeps it testable.
pub struct GlobeView {
    /// Morph target: true = globe.
    on: bool,
    /// Progress at the moment of the last toggle, so reversing mid-morph
    /// continues from where the animation was instead of jumping.
    origin: f64,
    toggled_at: Option<Instant>,
    /// Spin phase anchor. The globe keeps rotating while hidden — toggling
    /// back shows wherever the planet has turned to, like a real one.
    epoch: Instant,
}

impl GlobeView {
    pub fn new(now: Instant) -> Self {
        Self {
            on: false,
            origin: 0.0,
            toggled_at: None,
            epoch: now,
        }
    }

    pub fn toggle(&mut self, now: Instant) {
        self.origin = self.progress(now);
        self.on = !self.on;
        self.toggled_at = Some(now);
    }

    /// Current morph target (what the view is heading toward, not where the
    /// animation is right now).
    pub fn target(&self) -> bool {
        self.on
    }

    /// Steer toward `on`, animating from wherever the morph currently is.
    /// Idempotent — callers re-assert the target every frame.
    pub fn set_target(&mut self, on: bool, now: Instant) {
        if on != self.on {
            self.toggle(now);
        }
    }

    /// Jump straight to `on` with no animation — for the first frame, where
    /// a terminal that opens narrow should simply start on the globe rather
    /// than replay the morph on every launch.
    pub fn snap(&mut self, on: bool) {
        self.on = on;
        self.toggled_at = None;
    }

    /// Raw morph progress, 0 (flat) to 1 (globe), moving linearly toward the
    /// target since the last toggle.
    fn progress(&self, now: Instant) -> f64 {
        let Some(at) = self.toggled_at else {
            return if self.on { 1.0 } else { 0.0 };
        };
        let elapsed = now.saturating_duration_since(at).as_secs_f64() / TRANSITION.as_secs_f64();
        if self.on {
            (self.origin + elapsed).min(1.0)
        } else {
            (self.origin - elapsed).max(0.0)
        }
    }

    /// Eased morph parameter for rendering (smoothstep: gentle at both ends).
    pub fn t(&self, now: Instant) -> f64 {
        let p = self.progress(now);
        p * p * (3.0 - 2.0 * p)
    }

    /// Longitude currently facing the viewer. Increasing = eastward spin,
    /// matching the real planet: features drift toward the left limb.
    pub fn center_lon(&self, now: Instant) -> f64 {
        (now.saturating_duration_since(self.epoch).as_secs_f64() * 360.0 / SECS_PER_REV) % 360.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-9;

    #[test]
    fn t_zero_is_the_identity_onto_the_flat_map() {
        // The morph must start pixel-identical to the flat map, wherever the
        // globe has spun to and even for points the sphere would cull.
        for &(lon, lat) in &[(-73.6, 45.5), (139.7, 35.7), (0.0, 0.0), (-170.0, -55.0)] {
            let (x, y) = project(lon, lat, 123.4, 0.0).unwrap();
            assert!(
                (x - lon).abs() < EPS && (y - lat).abs() < EPS,
                "({lon}, {lat})"
            );
        }
    }

    #[test]
    fn face_center_projects_to_the_disc_center() {
        // The point looking at us sits at the center, dipped by the tilt.
        let (x, y) = project(30.0, 0.0, 30.0, 1.0).unwrap();
        assert!((x - CENTER_X).abs() < EPS);
        let expected_y = CENTER_Y - RADIUS * TILT_DEG.to_radians().sin();
        assert!((y - expected_y).abs() < EPS);
    }

    #[test]
    fn antipode_is_culled_on_the_globe_but_kept_flat() {
        assert!(project(150.0, 0.0, -30.0, 1.0).is_none());
        assert!(project(150.0, 0.0, -30.0, 0.0).is_some());
        // Mid-morph the cutoff has not yet reached a point just past the
        // limb (depth slightly below 0).
        assert!(project(95.0, 0.0, 0.0, 0.5).is_some());
        assert!(project(95.0, 0.0, 0.0, 1.0).is_none());
    }

    #[test]
    fn tilt_brings_the_north_pole_into_view() {
        // Without tilt the pole would sit exactly on the limb (depth 0);
        // the tilt pitches it toward the viewer, visible from any spin angle.
        for center in [0.0, 90.0, 200.0] {
            let (x, y) = project(0.0, 90.0, center, 1.0).expect("pole visible");
            assert!((x - CENTER_X).abs() < EPS);
            assert!((y - (CENTER_Y + RADIUS * TILT_DEG.to_radians().cos())).abs() < EPS);
        }
    }

    #[test]
    fn projection_stays_inside_the_disc() {
        for lon in (-180..180).step_by(7) {
            for lat in (-90..=90).step_by(7) {
                let Some((x, y)) = project(f64::from(lon), f64::from(lat), 42.0, 1.0) else {
                    continue;
                };
                let r = ((x - CENTER_X).powi(2) + (y - CENTER_Y).powi(2)).sqrt();
                assert!(r <= RADIUS + EPS, "({lon}, {lat}) at r={r}");
            }
        }
    }

    #[test]
    fn toggle_reaches_the_target_and_holds_it() {
        let now = Instant::now();
        let mut view = GlobeView::new(now);
        assert!(view.t(now).abs() < EPS);
        view.toggle(now);
        assert!(view.t(now).abs() < EPS); // starts from flat
        let done = now + TRANSITION;
        assert!((view.t(done) - 1.0).abs() < EPS);
        assert!((view.t(done + TRANSITION) - 1.0).abs() < EPS); // clamped
    }

    #[test]
    fn reversing_mid_morph_continues_from_the_current_progress() {
        let now = Instant::now();
        let mut view = GlobeView::new(now);
        view.toggle(now);
        let mid = now + TRANSITION / 2;
        view.toggle(mid); // flip back halfway through
        // Progress resumes at 0.5 and runs back down, hitting flat after
        // half a transition, not a full one.
        let t0 = view.t(mid);
        assert!((t0 - 0.5).abs() < EPS);
        assert!(view.t(mid + TRANSITION / 2).abs() < EPS);
    }

    #[test]
    fn set_target_is_idempotent_mid_morph() {
        // sync re-asserts the target every frame; that must not restart the
        // animation from scratch each time.
        let now = Instant::now();
        let mut view = GlobeView::new(now);
        view.set_target(true, now);
        let mid = now + TRANSITION / 2;
        view.set_target(true, mid); // same target re-asserted mid-morph
        assert!((view.t(mid) - 0.5).abs() < EPS);
        assert!((view.t(now + TRANSITION) - 1.0).abs() < EPS);
    }

    #[test]
    fn snap_jumps_without_animating() {
        let now = Instant::now();
        let mut view = GlobeView::new(now);
        view.snap(true);
        assert!((view.t(now) - 1.0).abs() < EPS);
        // A later target change still animates normally.
        view.set_target(false, now);
        assert!((view.t(now + TRANSITION / 2) - 0.5).abs() < EPS);
    }

    #[test]
    fn flat_geometry_matches_the_classic_panel() {
        let g = panel_geometry(80, 50, 0.0);
        assert_eq!(g.width, 80);
        assert_eq!(g.height, (78.0 * MAP_ASPECT).round() as u16 + 2);
        assert!((g.x_span - MAP_LON_SPAN).abs() < EPS);
        assert_eq!(g.x_bounds(), [-170.0, 180.0]);
        assert_eq!(g.y_bounds(), [-55.0, 72.0]);
    }

    #[test]
    fn globe_geometry_is_a_square_dot_grid_that_fits_the_disc() {
        let g = panel_geometry(80, 50, 1.0);
        // 2 dots/cell wide × 4 tall: square grid means height ≈ width/2.
        assert_eq!(g.height, (g.width - 2) / 2 + 2);
        // Zoomed so the 127° lat span fills the panel: the disc (2×RADIUS
        // = 120°) occupies ~94% of it.
        assert!((g.x_span - MAP_LAT_SPAN).abs() < EPS);
    }

    #[test]
    fn short_terminals_shrink_the_globe_panel_and_keep_it_round() {
        // Height-capped: the panel narrows to stay square instead of leaving
        // the globe floating in a wide canvas.
        let g = panel_geometry(160, 30, 1.0);
        assert!(g.height <= 30 - 4);
        assert_eq!(g.width, 2 * (g.height - 2) + 2);
        // Round limb invariant: degrees per dot equal in x and y.
        let per_dot_x = g.x_span / (2.0 * f64::from(g.width - 2));
        let per_dot_y = MAP_LAT_SPAN / (4.0 * f64::from(g.height - 2));
        assert!((per_dot_x - per_dot_y).abs() < EPS);
    }

    #[test]
    fn geometry_survives_degenerate_sizes() {
        for (w, h) in [(0, 0), (1, 1), (6, 4), (7, 50), (300, 2)] {
            let g = panel_geometry(w, h, 1.0);
            assert!(g.width >= 6 && g.height >= 4);
            assert!(g.x_span.is_finite() && g.x_span > 0.0);
        }
    }

    #[test]
    fn spin_advances_and_wraps() {
        let now = Instant::now();
        let view = GlobeView::new(now);
        assert!(view.center_lon(now).abs() < EPS);
        let quarter = now + Duration::from_secs_f64(SECS_PER_REV / 4.0);
        assert!((view.center_lon(quarter) - 90.0).abs() < 1e-6);
        let wrapped = now + Duration::from_secs_f64(SECS_PER_REV * 1.25);
        assert!((view.center_lon(wrapped) - 90.0).abs() < 1e-6);
    }
}
