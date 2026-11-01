//! Windows native overlay: a layered top-most HUD window plus its settings
//! window and the frame-mark window.
//!
//! Split by responsibility:
//! - [`geometry`] pure layout/animation math (unit-tested, no window handles)
//! - [`hud`]      the HUD window: lifecycle, message loop, painting, pointer
//! - [`menu`]     the settings window: a plain framed window whose client area
//!                is built from native Win32 controls (no Slint surface)
//! - [`axes`]     the frame-mark list window (still Slint-rendered)
//!
//! This module owns only the constants shared across those pieces.

mod axes;
mod frame;
mod geometry;
mod hud;
mod menu;

pub(crate) use hud::run;

const OVERLAY_TIMER_INTERVAL_MS: u32 = 16;

// Fixed logical design size of `hud.slint`. Physical size = logical * scale.
// The panel height stays the scale anchor; extra host height is for controls
// rendered outside the panel, not extra internal HUD content.
const LOGICAL_W: f32 = 210.0;
const LOGICAL_PANEL_H: f32 = 56.0;
const LOGICAL_H: f32 = 82.0;
// Keep this in sync with the toolbar `width` in `ui/hud.slint`; hit-testing
// uses the same rect for the transparent window extension.
const LOGICAL_TOOLBAR_W: f32 = 150.0;
const LOGICAL_UPDATE_BADGE_W: f32 = 48.0;
const LOGICAL_UPDATE_BADGE_LEFT_PAD: f32 = 4.0;
const LOGICAL_TOOLBAR_H: f32 = 24.0;
const LOGICAL_TOOLBAR_RIGHT_PAD: f32 = 4.0;
const LOGICAL_TOOLBAR_TOP_GAP: f32 = 2.0;
