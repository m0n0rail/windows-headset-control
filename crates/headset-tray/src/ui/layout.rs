//! Panel layout and hit-testing. Pure: no OS access, no device, no window.
//!
//! Everything that decides *where* something is lives here, so tick spacing,
//! value-to-position mapping and its inverse, and every clickable region can be
//! tested without a GPU. `render` walks the primitive list and decides nothing.

use headset_protocol::v3pro::noise::{NoiseControl, NoiseMode, ANC_LEVEL_RANGE};

use crate::output::{Problem, Slot};
use crate::state::HeadsetState;
use crate::ui::theme::*;

/// The ANC level bounds come from the protocol crate rather than being restated
/// here, so the panel cannot offer a level the encoder would refuse to send.
const ANC_MIN: u8 = ANC_LEVEL_RANGE.0;
const ANC_MAX: u8 = ANC_LEVEL_RANGE.1;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn center_y(&self) -> f32 {
        self.y + self.h / 2.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Primitive {
    /// Filled and/or stroked rounded rectangle.
    RoundRect {
        rect: Rect,
        radius: f32,
        fill: Option<Color>,
        stroke: Option<Color>,
        stroke_w: f32,
    },
    Circle {
        cx: f32,
        cy: f32,
        r: f32,
        fill: Option<Color>,
        stroke: Option<Color>,
        stroke_w: f32,
    },
    /// A soft radial fade, used for the knob glow.
    Glow {
        cx: f32,
        cy: f32,
        r: f32,
        color: Color,
    },
    Line {
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        w: f32,
        color: Color,
    },
    /// Open or closed polyline. Icons are built from these.
    Path {
        points: Vec<(f32, f32)>,
        closed: bool,
        fill: Option<Color>,
        stroke: Option<Color>,
        stroke_w: f32,
    },
    Text {
        rect: Rect,
        text: String,
        size: f32,
        weight: u32,
        color: Color,
        align: Align,
        /// Extra spacing between characters, for the small tracked-out captions.
        tracking: f32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HitTarget {
    Gear,
    Back,
    MutePill,
    Switcher,
    SliderTrack,
    Refresh,
    ToggleStartup,
    ToggleWarning,
    /// Turns the power-off output switch on or off.
    ToggleOutputSwitch,
    /// Turns the game/chat split on or off.
    ToggleSplit,
    /// Opens the device picker, for one of the three choices.
    OpenOutputPicker(Slot),
    /// One device in the picker, by its position in the drawn list. Positional
    /// rather than carrying the id because [`HitTarget`] is `Copy` and a `String`
    /// is not; [`Panel::output_ids`] resolves it against the list that was
    /// actually drawn, so a device appearing or vanishing between the paint and
    /// the click cannot silently select a different one.
    OutputChoice(usize),
    /// The three segments of the noise-mode row, in drawn order.
    NoiseOff,
    NoiseAnc,
    NoiseAmbient,
    /// The ANC level track. Only present while the mode is ANC.
    NoiseLevel,
    /// The three segments of the Appearance row, in the Settings view.
    AppearanceSystem,
    AppearanceDark,
    AppearanceLight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Main,
    Settings,
    /// The output-device picker, reached from Settings. Carries which of the
    /// three choices is being made, because the list it draws is the same one
    /// either way and only the heading and the tick differ.
    Output(Slot),
}

/// Which parameter the single slider is currently driving.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SliderParam {
    Sidetone,
    GameChat,
}

impl SliderParam {
    /// Inclusive maximum. One tick is drawn per selectable value, which is why
    /// the game/chat track reads denser than the sidetone track.
    pub fn max(self) -> u8 {
        match self {
            SliderParam::Sidetone => 15,
            SliderParam::GameChat => 20,
        }
    }
    pub fn ticks(self) -> usize {
        self.max() as usize + 1
    }
    pub fn button_label(self) -> &'static str {
        match self {
            SliderParam::Sidetone => "Sidetone",
            SliderParam::GameChat => "Game / Chat",
        }
    }
    pub fn end_labels(self) -> [&'static str; 3] {
        match self {
            SliderParam::Sidetone => ["OFF", "", "MAX"],
            // Low values are game, high values are chat: verified by listening,
            // not by the captures, which never established the direction.
            SliderParam::GameChat => ["GAME", "BALANCED", "CHAT"],
        }
    }
    pub fn other(self) -> SliderParam {
        match self {
            SliderParam::Sidetone => SliderParam::GameChat,
            SliderParam::GameChat => SliderParam::Sidetone,
        }
    }
}

/// Which value the slider should draw.
///
/// `drag` is a live drag in progress. `pending` is a value already committed to
/// the device but not yet confirmed by the read-back that follows every write —
/// it is tagged with the parameter it belongs to, because one slider serves two
/// parameters and the switcher can be used while a write is in flight.
///
/// `None` means "use whatever the device reports". Returning `None` while a
/// write is outstanding is what made the knob jump back to its old position on
/// release and then forward again a beat later.
pub fn slider_preview(
    param: SliderParam,
    drag: Option<u8>,
    pending: Option<(SliderParam, u8)>,
) -> Option<u8> {
    drag.or_else(|| pending.filter(|(p, _)| *p == param).map(|(_, v)| v))
}

/// The value text shown to the right of the switcher, exactly as the mockups
/// render it.
pub fn format_value(param: SliderParam, value: Option<u8>) -> String {
    let Some(v) = value else {
        return "--".to_string();
    };
    match param {
        SliderParam::Sidetone => {
            if v == 0 {
                "Off".to_string()
            } else {
                v.to_string()
            }
        }
        SliderParam::GameChat => match v.cmp(&10) {
            std::cmp::Ordering::Equal => "Balanced".to_string(),
            std::cmp::Ordering::Greater => format!("Chat +{}", v - 10),
            std::cmp::Ordering::Less => format!("Game +{}", 10 - v),
        },
    }
}

/// Which of the three end labels is highlighted, if any.
fn active_end_label(param: SliderParam, value: Option<u8>) -> Option<usize> {
    let v = value?;
    Some(match param {
        SliderParam::Sidetone => {
            if v == 0 {
                0
            } else if v == param.max() {
                2
            } else {
                return None;
            }
        }
        SliderParam::GameChat => match v.cmp(&10) {
            std::cmp::Ordering::Equal => 1,
            std::cmp::Ordering::Less if v == 0 => 0,
            std::cmp::Ordering::Greater if v == param.max() => 2,
            _ => return None,
        },
    })
}

pub struct Panel {
    pub primitives: Vec<Primitive>,
    pub hits: Vec<(Rect, HitTarget)>,
    /// Panel height, excluding the shadow margin.
    pub height: f32,
    /// Track geometry, retained so drag handling can map x back to a value.
    pub track: Option<TrackGeometry>,
    /// ANC level track, present only while the mode is ANC.
    pub level_track: Option<LevelTrack>,
    /// Endpoint ids in the order they were drawn, so a
    /// [`HitTarget::OutputChoice`] resolves against this paint rather than a
    /// fresh enumeration. Empty in every view but [`View::Output`].
    pub output_ids: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrackGeometry {
    pub x0: f32,
    pub x1: f32,
    pub y: f32,
    pub param: SliderParam,
}

/// The ANC level track: four positions, numbered as the vendor UI numbers them.
///
/// Deliberately a separate type from [`TrackGeometry`], because its values start
/// at 1 rather than 0 and nothing establishes which end is the stronger — the
/// ends are labelled `1` and `4`, not `LOW` and `HIGH`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LevelTrack {
    pub x0: f32,
    pub x1: f32,
    pub y: f32,
}

impl LevelTrack {
    pub fn x_for(&self, level: u8) -> f32 {
        let t = (level.clamp(ANC_MIN, ANC_MAX) - ANC_MIN) as f32 / (ANC_MAX - ANC_MIN) as f32;
        self.x0 + (self.x1 - self.x0) * t
    }
    /// Nearest level to a pointer position, clamped to the observed four.
    pub fn level_at(&self, px: f32) -> u8 {
        let span = (ANC_MAX - ANC_MIN) as f32;
        let t = ((px - self.x0) / (self.x1 - self.x0)).clamp(0.0, 1.0);
        ANC_MIN + (t * span).round() as u8
    }
}

/// Which segment of the noise row is active, or `None` when the device's mode
/// is unknown or is a byte this project has no evidence for.
pub fn active_noise_segment(noise: Option<NoiseControl>) -> Option<usize> {
    match noise?.mode {
        NoiseMode::Off => Some(0),
        NoiseMode::Anc => Some(1),
        NoiseMode::Ambient => Some(2),
        NoiseMode::Unrecognised(_) => None,
    }
}

/// The noise state as the panel words it.
pub fn format_noise(noise: Option<NoiseControl>) -> String {
    let Some(n) = noise else {
        return "--".to_string();
    };
    match n.mode {
        NoiseMode::Off => "Off".to_string(),
        NoiseMode::Anc => format!("ANC {}", n.anc_level),
        // No level: the captures show ambient ignoring byte 1.
        NoiseMode::Ambient => "Ambient".to_string(),
        NoiseMode::Unrecognised(_) => "--".to_string(),
    }
}

impl TrackGeometry {
    pub fn x_for(&self, value: u8) -> f32 {
        let max = self.param.max() as f32;
        self.x0 + (self.x1 - self.x0) * (value as f32 / max)
    }
    /// Nearest tick to a pointer position, clamped to the ends.
    pub fn value_at(&self, px: f32) -> u8 {
        let max = self.param.max() as f32;
        let t = ((px - self.x0) / (self.x1 - self.x0)).clamp(0.0, 1.0);
        (t * max).round() as u8
    }
}

impl Panel {
    pub fn hit(&self, px: f32, py: f32) -> Option<HitTarget> {
        self.hits
            .iter()
            .find(|(r, _)| r.contains(px, py))
            .map(|(_, t)| *t)
    }
}

struct Builder {
    p: Vec<Primitive>,
    hits: Vec<(Rect, HitTarget)>,
    dim: bool,
}

impl Builder {
    fn tint(&self, c: Color) -> Color {
        if self.dim {
            c.with_alpha(DISABLED_ALPHA)
        } else {
            c
        }
    }
    fn text(&mut self, rect: Rect, s: &str, size: f32, weight: u32, color: Color, align: Align) {
        self.p.push(Primitive::Text {
            rect,
            text: s.to_string(),
            size,
            weight,
            color,
            align,
            tracking: 0.0,
        });
    }
    fn caption(&mut self, rect: Rect, s: &str, color: Color, align: Align) {
        self.p.push(Primitive::Text {
            rect,
            text: s.to_string(),
            size: FS_CAPTION,
            weight: W_SEMIBOLD,
            color,
            align,
            tracking: 1.1,
        });
    }
    fn card(&mut self, rect: Rect, fill: Color, stroke: Color) {
        self.p.push(Primitive::RoundRect {
            rect,
            radius: CARD_RADIUS,
            fill: Some(fill),
            stroke: Some(stroke),
            stroke_w: 1.0,
        });
    }
}

/// Builds the panel for a given state.
pub fn build(state: &HeadsetState, view: View, param: SliderParam, preview: Option<u8>) -> Panel {
    // A device with no game/chat balance has only the sidetone slider, whatever
    // the switcher last said.
    let param = if state.features.game_chat {
        param
    } else {
        SliderParam::Sidetone
    };
    let mut b = Builder {
        p: Vec::new(),
        hits: Vec::new(),
        dim: state.connected == Some(false),
    };

    // Panel background is drawn first and sized at the end, once the content
    // height is known. A placeholder keeps its index stable.
    b.p.push(Primitive::RoundRect {
        rect: Rect::new(0.0, 0.0, PANEL_W, 0.0),
        radius: PANEL_RADIUS,
        fill: Some(bg_panel()),
        stroke: None,
        stroke_w: 0.0,
    });

    let mut y = HEADER_TOP;
    header(&mut b, state, view, &mut y);

    let mut track = None;
    let mut level_track = None;
    let mut output_ids = Vec::new();
    match view {
        View::Main => main_body(
            &mut b,
            state,
            param,
            preview,
            &mut y,
            &mut track,
            &mut level_track,
        ),
        View::Settings => settings_body(&mut b, state, &mut y),
        View::Output(slot) => output_body(&mut b, slot, &mut y, &mut output_ids),
    }

    // Footer
    y += GAP * 0.5;
    let footer = Rect::new(MARGIN, y, CONTENT_W, FOOTER_H);
    refresh_icon(&mut b, MARGIN + 9.0, footer.center_y());
    b.text(
        Rect::new(MARGIN + 22.0, y, CONTENT_W - 22.0, FOOTER_H),
        "Refresh",
        FS_BODY,
        W_REGULAR,
        text_muted(),
        Align::Left,
    );
    b.hits.push((footer, HitTarget::Refresh));
    y = footer.bottom() + 6.0;

    let height = y;
    if let Some(Primitive::RoundRect { rect, .. }) = b.p.first_mut() {
        rect.h = height;
    }

    Panel {
        primitives: b.p,
        hits: b.hits,
        height,
        track,
        level_track,
        output_ids,
    }
}

fn header(b: &mut Builder, state: &HeadsetState, view: View, y: &mut f32) {
    let connected = state.connected == Some(true);
    let dot_color = if connected {
        accent()
    } else {
        text_secondary()
    };
    b.p.push(Primitive::Circle {
        cx: MARGIN + 4.0,
        cy: *y + 11.0,
        r: 4.0,
        fill: Some(dot_color),
        stroke: None,
        stroke_w: 0.0,
    });

    b.text(
        Rect::new(MARGIN + 16.0, *y, CONTENT_W - 60.0, 20.0),
        &state.device_name(),
        FS_TITLE,
        W_BOLD,
        text_primary(),
        Align::Left,
    );

    // A silent dongle outranks the link state, because the link state is
    // exactly what a silent dongle has stopped telling us. Reporting SEARCHING
    // here is what made a wedge indistinguishable from a cold start.
    let status = if state.dongle_silent {
        // Short by necessity: this caption is letter-spaced and sits beside the
        // gear button, so a remedy does not fit here without wrapping over the
        // device name. The banner below carries it instead.
        "NOT RESPONDING".to_string()
    } else {
        match state.connected {
            Some(true) => format!("CONNECTED · {LINK_TYPE_LABEL}"),
            Some(false) => "DISCONNECTED".to_string(),
            None => "SEARCHING".to_string(),
        }
    };
    b.caption(
        Rect::new(MARGIN + 16.0, *y + 22.0, CONTENT_W - 60.0, 14.0),
        &status,
        text_secondary(),
        Align::Left,
    );

    // Gear / Back button, top right.
    let btn = Rect::new(MARGIN + CONTENT_W - 30.0, *y - 2.0, 30.0, 30.0);
    // Every view but the main one is somewhere you came from somewhere else, so
    // the header button is Back in all of them. `on_panel_press` decides where
    // back actually goes.
    let active = view != View::Main;
    b.p.push(Primitive::RoundRect {
        rect: btn,
        radius: BUTTON_RADIUS,
        fill: Some(if active {
            accent().with_alpha(0x33)
        } else {
            bg_button()
        }),
        stroke: Some(if active { accent() } else { border_card() }),
        stroke_w: 1.0,
    });
    gear_icon(
        b,
        btn.x + 15.0,
        btn.center_y(),
        if active { accent() } else { text_muted() },
    );
    b.hits.push((
        btn,
        if active {
            HitTarget::Back
        } else {
            HitTarget::Gear
        },
    ));

    *y += HEADER_H;
}

fn main_body(
    b: &mut Builder,
    state: &HeadsetState,
    param: SliderParam,
    preview: Option<u8>,
    y: &mut f32,
    track: &mut Option<TrackGeometry>,
    level_track: &mut Option<LevelTrack>,
) {
    // ---- status card -------------------------------------------------------
    let card = Rect::new(MARGIN, *y, CONTENT_W, CARD_H);
    b.card(card, bg_card(), border_card());

    let bat_y = card.y + 24.0;
    battery_icon(
        b,
        card.x + 18.0,
        bat_y + 4.0,
        state.battery,
        b.tint(text_primary()),
    );
    let pct = state
        .battery
        .map(|v| format!("{v}%"))
        .unwrap_or_else(|| "--".to_string());
    b.text(
        Rect::new(card.x + 52.0, bat_y - 10.0, 120.0, 26.0),
        &pct,
        FS_BATTERY,
        W_BOLD,
        b.tint(text_primary()),
        Align::Left,
    );
    b.caption(
        Rect::new(card.x + 53.0, bat_y + 16.0, 120.0, 14.0),
        "BATTERY",
        b.tint(text_secondary()),
        Align::Left,
    );

    // ---- mute pill ---------------------------------------------------------
    let muted = state.effectively_muted();
    let (pill_text, glyph_color) = match muted {
        Some(true) => ("MUTED", state_muted()),
        Some(false) => ("LIVE", state_live()),
        None => ("--", text_secondary()),
    };
    // Wide enough for "MUTED" at this weight. Sized to the longest label rather
    // than the shortest: at 86 px it wrapped to "MUTE / D".
    let pill_w = 100.0;
    let pill = Rect::new(card.right() - pill_w - 14.0, card.y + 18.0, pill_w, 34.0);
    let hardware_locked = state.mic_mute_hardware == Some(true);
    b.p.push(Primitive::RoundRect {
        rect: pill,
        radius: BUTTON_RADIUS,
        fill: Some(bg_panel()),
        stroke: Some(if muted == Some(true) {
            accent()
        } else {
            border_card()
        }),
        stroke_w: 1.0,
    });
    mic_icon(
        b,
        pill.x + 18.0,
        pill.center_y(),
        glyph_color,
        muted == Some(true),
    );
    b.text(
        Rect::new(pill.x + 32.0, pill.y, pill.w - 40.0, pill.h),
        pill_text,
        FS_PILL,
        W_SEMIBOLD,
        b.tint(text_primary()),
        Align::Left,
    );
    // A hardware-muted mic cannot be released from software, so the click is
    // not offered rather than offered and ignored.
    if !hardware_locked && !b.dim {
        b.hits.push((pill, HitTarget::MutePill));
    }

    *y = card.bottom() + GAP;

    if hardware_locked {
        b.text(
            Rect::new(MARGIN, *y - 6.0, CONTENT_W, 16.0),
            "Muted by the headset's own switch",
            FS_CAPTION,
            W_REGULAR,
            text_secondary(),
            Align::Right,
        );
        *y += 12.0;
    }

    // ---- switcher + value --------------------------------------------------
    // With only one slider there is nothing to switch to, so the button becomes
    // a plain label: no swap glyph, no outline, and no click target.
    let switchable = state.features.game_chat;
    let sw = Rect::new(MARGIN, *y, SWITCHER_W, SWITCHER_H);
    if switchable {
        b.p.push(Primitive::RoundRect {
            rect: sw,
            radius: BUTTON_RADIUS,
            fill: None,
            stroke: Some(b.tint(accent())),
            stroke_w: 1.5,
        });
        swap_icon(b, sw.x + 16.0, sw.center_y(), b.tint(accent()));
    }
    let label_x = if switchable { sw.x + 28.0 } else { sw.x };
    b.text(
        Rect::new(label_x, sw.y, sw.right() - label_x - 6.0, sw.h),
        param.button_label(),
        FS_BODY,
        W_SEMIBOLD,
        b.tint(text_primary()),
        Align::Left,
    );
    if switchable && !b.dim {
        b.hits.push((sw, HitTarget::Switcher));
    }

    let value = preview.or(match param {
        SliderParam::Sidetone => state.sidetone,
        SliderParam::GameChat => state.game_chat,
    });
    let mut value_text = format_value(param, value);
    // A level that was sent but cannot be read back says so, every time it is
    // drawn. Shown as a plain number it would claim more than the device told us.
    if param == SliderParam::Sidetone
        && preview.is_none()
        && state.sidetone_unconfirmed
        && value.is_some()
    {
        value_text.push_str(" · SENT");
    }
    b.text(
        Rect::new(MARGIN, sw.y, CONTENT_W, SWITCHER_H),
        &value_text,
        FS_BODY,
        W_SEMIBOLD,
        b.tint(accent_text()),
        Align::Right,
    );

    *y = sw.bottom() + 22.0;

    // ---- slider ------------------------------------------------------------
    // Measured dot centres run x 26..332 in the mockup, which includes the 8 px
    // shadow margin; panel-local that is 18..324, one pixel inside the card
    // edges at 17 and 325.
    let tx0 = MARGIN + 1.0;
    let tx1 = MARGIN + CONTENT_W - 1.0;
    let ty = *y;
    let geom = TrackGeometry {
        x0: tx0,
        x1: tx1,
        y: ty,
        param,
    };

    let filled_to = value.map(|v| geom.x_for(v));
    b.p.push(Primitive::Line {
        x0: tx0,
        y0: ty,
        x1: tx1,
        y1: ty,
        w: TRACK_LINE_W,
        color: b.tint(track_inactive()),
    });
    if let Some(fx) = filled_to {
        if fx > tx0 {
            b.p.push(Primitive::Line {
                x0: tx0,
                y0: ty,
                x1: fx,
                y1: ty,
                w: TRACK_LINE_W,
                color: b.tint(accent()),
            });
        }
    }
    for i in 0..param.ticks() {
        let x = geom.x_for(i as u8);
        let active = filled_to.map(|fx| x <= fx + 0.5).unwrap_or(false);
        b.p.push(Primitive::Circle {
            cx: x,
            cy: ty,
            r: DOT_R,
            fill: Some(b.tint(if active { accent() } else { track_inactive() })),
            stroke: None,
            stroke_w: 0.0,
        });
    }
    if let Some(v) = value {
        let kx = geom.x_for(v);
        if !b.dim {
            b.p.push(Primitive::Glow {
                cx: kx,
                cy: ty,
                r: KNOB_GLOW_R,
                color: accent().with_alpha(0x55),
            });
        }
        b.p.push(Primitive::Circle {
            cx: kx,
            cy: ty,
            r: KNOB_R,
            fill: Some(b.tint(accent())),
            stroke: None,
            stroke_w: 0.0,
        });
    }
    // Generous vertical hit area: the visible track is 2 px tall.
    let hit = Rect::new(MARGIN, ty - 14.0, CONTENT_W, 28.0);
    if !b.dim {
        b.hits.push((hit, HitTarget::SliderTrack));
    }
    *track = Some(geom);

    // End labels
    let labels = param.end_labels();
    let active_idx = active_end_label(param, value);
    let label_y = ty + 14.0;
    let aligns = [Align::Left, Align::Center, Align::Right];
    for (i, text) in labels.iter().enumerate() {
        if text.is_empty() {
            continue;
        }
        let color = if active_idx == Some(i) {
            accent_label()
        } else {
            text_secondary()
        };
        b.caption(
            Rect::new(MARGIN, label_y, CONTENT_W, 14.0),
            text,
            b.tint(color),
            aligns[i],
        );
    }
    *y = label_y + 18.0 + GAP;

    if state.features.noise {
        noise_section(b, state, y, level_track);
    }

    // ---- warning banner ----------------------------------------------------
    // A silent dongle outranks the Synapse warning. A caution that something
    // may override these settings is noise when nothing can read or write them
    // at all, and only one banner fits.
    let banner_text = if state.dongle_silent {
        Some("Dongle not responding. Unplug it and plug it back in.")
    } else if state.warn_vendor_software {
        Some("Synapse is running and may override these settings.")
    } else {
        None
    };
    if let Some(message) = banner_text {
        let banner = Rect::new(MARGIN, *y, CONTENT_W, BANNER_H);
        b.card(banner, bg_banner(), border_banner());
        warning_icon(b, banner.x + 18.0, banner.center_y(), text_muted());
        b.text(
            Rect::new(
                banner.x + 32.0,
                banner.y + 8.0,
                banner.w - 44.0,
                banner.h - 16.0,
            ),
            message,
            FS_BODY - 1.0,
            W_REGULAR,
            text_muted(),
            Align::Left,
        );
        *y = banner.bottom() + GAP * 0.5;
    }
}

/// What the Appearance row says beneath its title.
///
/// `System` names the theme it actually resolved to, so `AUTO` is not a black
/// box on a machine whose Windows is dark.
pub fn appearance_subtitle(a: Appearance) -> String {
    match a {
        Appearance::Light => "Light theme".to_string(),
        Appearance::Dark => "Dark theme".to_string(),
        Appearance::System => {
            let resolved = if crate::settings::windows_prefers_light() {
                "light"
            } else {
                "dark"
            };
            format!("Following Windows — {resolved}")
        }
    }
}

/// A row of segments, the active one filled with the accent.
///
/// Shared by the noise mode row and the Appearance row deliberately: the panel
/// teaches this pattern once and a second use costs the user nothing to learn.
/// Two copies would drift.
fn segmented(b: &mut Builder, row: Rect, segments: &[(&str, HitTarget)], active: Option<usize>) {
    b.card(row, bg_card(), border_card());
    let seg_w = row.w / segments.len() as f32;

    for (i, (label, target)) in segments.iter().enumerate() {
        let seg = Rect::new(row.x + seg_w * i as f32, row.y, seg_w, row.h);
        let is_active = active == Some(i);
        if is_active {
            // Inset so the fill sits inside the container's border rather than
            // doubling it.
            let fill = Rect::new(seg.x + 2.0, seg.y + 2.0, seg.w - 4.0, seg.h - 4.0);
            b.p.push(Primitive::RoundRect {
                rect: fill,
                radius: BUTTON_RADIUS,
                fill: Some(b.tint(accent().with_alpha(0x33))),
                stroke: Some(b.tint(accent())),
                stroke_w: 1.5,
            });
        }
        b.caption(
            Rect::new(seg.x, seg.y, seg.w, seg.h),
            label,
            b.tint(if is_active {
                accent_label()
            } else {
                text_secondary()
            }),
            Align::Center,
        );
        if !b.dim {
            b.hits.push((seg, *target));
        }
    }
}

/// The noise-control block: a caption row, a three-segment mode row, and the
/// ANC level track.
///
/// The level track is drawn in every mode but only hit-tests in ANC, so
/// switching modes does not change the panel's height and make it jump. That is
/// the same reasoning the disconnected state uses.
fn noise_section(
    b: &mut Builder,
    state: &HeadsetState,
    y: &mut f32,
    level_track: &mut Option<LevelTrack>,
) {
    let noise = state.noise;
    let active = active_noise_segment(noise);

    // ---- caption + current state ------------------------------------------
    b.caption(
        Rect::new(MARGIN, *y, CONTENT_W, 14.0),
        "NOISE CONTROL",
        b.tint(text_secondary()),
        Align::Left,
    );
    b.text(
        Rect::new(MARGIN, *y - 3.0, CONTENT_W, 16.0),
        &format_noise(noise),
        FS_BODY,
        W_SEMIBOLD,
        b.tint(accent_text()),
        Align::Right,
    );
    *y += 22.0;

    // ---- three-segment mode row -------------------------------------------
    let row = Rect::new(MARGIN, *y, CONTENT_W, SEGMENT_H);
    segmented(
        b,
        row,
        &[
            ("OFF", HitTarget::NoiseOff),
            ("ANC", HitTarget::NoiseAnc),
            ("AMBIENT", HitTarget::NoiseAmbient),
        ],
        active,
    );
    *y = row.bottom() + 20.0;

    // ---- ANC level track ---------------------------------------------------
    // Live only in ANC. In any other mode the level is retained by the device
    // but does nothing, so it is shown dimmed rather than hidden or removed.
    let live = noise.map(|n| n.mode == NoiseMode::Anc).unwrap_or(false) && !b.dim;
    let shade = |c: Color| {
        if live {
            c
        } else {
            c.with_alpha(DISABLED_ALPHA)
        }
    };

    let t = LevelTrack {
        x0: MARGIN + 1.0,
        x1: MARGIN + CONTENT_W - 1.0,
        y: *y,
    };
    // Drawn in every named mode, not just ANC: the device retains byte 1 while
    // off and while in ambient, and lands on it when ANC comes back. Hiding it
    // would claim the level is unknown while the device is reporting it. An
    // unrecognised mode byte gets nothing, because byte 1's meaning is only
    // established alongside the three modes that were observed.
    let level = noise
        .filter(|n| active_noise_segment(Some(*n)).is_some())
        .map(|n| n.anc_level);
    let filled_to = level.map(|l| t.x_for(l));

    b.p.push(Primitive::Line {
        x0: t.x0,
        y0: t.y,
        x1: t.x1,
        y1: t.y,
        w: TRACK_LINE_W,
        color: shade(track_inactive()),
    });
    if let Some(fx) = filled_to {
        if fx > t.x0 {
            b.p.push(Primitive::Line {
                x0: t.x0,
                y0: t.y,
                x1: fx,
                y1: t.y,
                w: TRACK_LINE_W,
                color: shade(accent()),
            });
        }
    }
    for l in ANC_MIN..=ANC_MAX {
        let x = t.x_for(l);
        let on = filled_to.map(|fx| x <= fx + 0.5).unwrap_or(false);
        b.p.push(Primitive::Circle {
            cx: x,
            cy: t.y,
            r: DOT_R,
            fill: Some(shade(if on { accent() } else { track_inactive() })),
            stroke: None,
            stroke_w: 0.0,
        });
    }
    if let Some(l) = level {
        let kx = t.x_for(l);
        if live {
            b.p.push(Primitive::Glow {
                cx: kx,
                cy: t.y,
                r: KNOB_GLOW_R,
                color: accent().with_alpha(0x55),
            });
        }
        b.p.push(Primitive::Circle {
            cx: kx,
            cy: t.y,
            r: KNOB_R,
            fill: Some(shade(accent())),
            stroke: None,
            stroke_w: 0.0,
        });
    }
    if live {
        b.hits.push((
            Rect::new(MARGIN, t.y - 14.0, CONTENT_W, 28.0),
            HitTarget::NoiseLevel,
        ));
        *level_track = Some(t);
    }

    // End labels are the vendor UI's own numbers. Nothing observed establishes
    // which end is the stronger cancellation, so they are not labelled LOW/HIGH.
    let label_y = t.y + 14.0;
    for (l, align) in [(ANC_MIN, Align::Left), (ANC_MAX, Align::Right)] {
        b.caption(
            Rect::new(MARGIN, label_y, CONTENT_W, 14.0),
            &l.to_string(),
            shade(if level == Some(l) {
                accent_label()
            } else {
                text_secondary()
            }),
            align,
        );
    }
    *y = label_y + 18.0 + GAP;
}

fn settings_body(b: &mut Builder, _state: &HeadsetState, y: &mut f32) {
    b.caption(
        Rect::new(MARGIN, *y, CONTENT_W, 14.0),
        "SETTINGS",
        text_secondary(),
        Align::Left,
    );
    *y += 22.0;

    // Problems are recorded one at a time, and each belongs to the feature that
    // earned it. The switch and the split are separate settings that can be on
    // independently, so a row is only ever shown its own complaint -- and only
    // while it is on, because a message about something switched off is noise.
    let switch_on = crate::settings_switch_output();
    let split_on = crate::settings_split_game_and_chat();
    let problem = crate::settings_output_problem();
    let switch_problem = problem.filter(|p| !p.is_about_split() && switch_on);
    let split_problem = problem.filter(|p| p.is_about_split() && split_on);

    toggle_row(
        b,
        y,
        "Start on Windows startup",
        "Launch the tray app when you sign in.",
        HitTarget::ToggleStartup,
        crate::settings_run_on_startup(),
        None,
    );
    toggle_row(
        b,
        y,
        "Display Synapse warning",
        "Warn when Synapse may override settings.",
        HitTarget::ToggleWarning,
        crate::settings_show_warning(),
        None,
    );
    toggle_row(
        b,
        y,
        "Switch output when off",
        "Move Windows sound elsewhere, and back.",
        HitTarget::ToggleOutputSwitch,
        switch_on,
        switch_problem,
    );
    // Always shown, and always clickable, even with the switch turned off: a
    // user who has not chosen a device yet needs somewhere to choose one, and
    // making the row appear only after the toggle is on hides the very thing
    // that makes the toggle do anything.
    device_row(b, y, Slot::Fallback);

    toggle_row(
        b,
        y,
        "Split game and chat",
        "Calls use the chat channel.",
        HitTarget::ToggleSplit,
        split_on,
        split_problem,
    );
    // These two are hidden while the split is off, unlike "Play through". The
    // toggle immediately above them is what reveals them, so nothing is out of
    // reach -- and three device rows stacked up, two of them inert, is a
    // settings page that looks like it is asking for more than it is.
    if split_on {
        device_row(b, y, Slot::Game);
        device_row(b, y, Slot::Chat);
    }

    // ---- appearance --------------------------------------------------------
    // Taller than a toggle row: it carries a segmented control rather than a
    // pill, because three states cannot be expressed by a switch.
    let card = Rect::new(MARGIN, *y, CONTENT_W, 78.0);
    b.card(card, bg_card(), border_card());
    b.text(
        Rect::new(card.x + 16.0, card.y + 10.0, card.w - 32.0, 20.0),
        "Appearance",
        FS_BODY,
        W_SEMIBOLD,
        text_primary(),
        Align::Left,
    );
    b.text(
        Rect::new(card.x + 16.0, card.y + 28.0, card.w - 32.0, 18.0),
        &appearance_subtitle(crate::settings::appearance()),
        FS_DESCRIPTION,
        W_REGULAR,
        text_secondary(),
        Align::Left,
    );
    let active = match crate::settings::appearance() {
        Appearance::System => 0,
        Appearance::Dark => 1,
        Appearance::Light => 2,
    };
    segmented(
        b,
        Rect::new(card.x + 12.0, card.y + 46.0, card.w - 24.0, 26.0),
        &[
            ("AUTO", HitTarget::AppearanceSystem),
            ("DARK", HitTarget::AppearanceDark),
            ("LIGHT", HitTarget::AppearanceLight),
        ],
        Some(active),
    );
    *y = card.bottom() + 12.0;

    // Back button
    let back = Rect::new(MARGIN, *y + 4.0, 88.0, 34.0);
    b.p.push(Primitive::RoundRect {
        rect: back,
        radius: BUTTON_RADIUS,
        fill: Some(bg_card()),
        stroke: Some(border_card()),
        stroke_w: 1.0,
    });
    back_icon(b, back.x + 18.0, back.center_y(), text_primary());
    b.text(
        Rect::new(back.x + 30.0, back.y, back.w - 34.0, back.h),
        "Back",
        FS_BODY,
        W_SEMIBOLD,
        text_primary(),
        Align::Left,
    );
    b.hits.push((back, HitTarget::Back));
    *y = back.bottom() + GAP * 0.5;
}

/// One settings row: a title, a line of description, and a switch.
///
/// `problem` replaces the description when the feature this row governs failed.
/// The failure happens with nobody watching -- the user's evidence is that
/// their sound did not move -- so the row has to be able to answer "why not"
/// whenever they get round to asking.
fn toggle_row(
    b: &mut Builder,
    y: &mut f32,
    title: &str,
    desc: &str,
    target: HitTarget,
    on: bool,
    problem: Option<Problem>,
) {
    let card = Rect::new(MARGIN, *y, CONTENT_W, 66.0);
    b.card(card, bg_card(), border_card());
    // Titles are kept short enough to stay on one line. The box is vertically
    // centred, so a wrapped title grows upward into the row above it and
    // downward into its own description -- the same trap the description box's
    // width is sized against.
    b.text(
        Rect::new(card.x + 16.0, card.y + 12.0, card.w - 76.0, 18.0),
        title,
        FS_BODY + 1.0,
        W_SEMIBOLD,
        text_primary(),
        Align::Left,
    );
    // Sized to hold the longer description on one line.
    //
    // A failure borrows the muted-microphone red rather than introducing a
    // colour: theme.rs sampled its palette from the mockups and says so, and
    // inventing an amber here would be inventing a design decision nobody
    // made. That red already means "this is not doing what you think".
    b.text(
        Rect::new(card.x + 16.0, card.y + 34.0, card.w - 62.0, 20.0),
        problem.map(|p| p.short()).unwrap_or(desc),
        FS_DESCRIPTION,
        W_REGULAR,
        if problem.is_some() {
            state_muted()
        } else {
            text_secondary()
        },
        Align::Left,
    );
    toggle(
        b,
        Rect::new(card.right() - 58.0, card.center_y() - 11.0, 42.0, 22.0),
        on,
    );
    b.hits.push((card, target));
    *y = card.bottom() + 12.0;
}

/// One settings row naming the device chosen for `slot`, opening the picker.
fn device_row(b: &mut Builder, y: &mut f32, slot: Slot) {
    let card = Rect::new(MARGIN, *y, CONTENT_W, 62.0);
    b.card(card, bg_card(), border_card());
    b.text(
        Rect::new(card.x + 16.0, card.y + 10.0, card.w - 48.0, 18.0),
        slot_title(slot),
        FS_BODY,
        W_SEMIBOLD,
        text_primary(),
        Align::Left,
    );
    let chosen = crate::settings_output_choice(slot);
    b.text(
        Rect::new(card.x + 16.0, card.y + 30.0, card.w - 48.0, 20.0),
        &output_choice_subtitle(slot, chosen.as_ref().map(|(n, p)| (n.as_str(), *p))),
        FS_DESCRIPTION,
        W_REGULAR,
        // An absent device is stated in the ordinary secondary colour rather
        // than an alarm colour: it is a fact about the machine right now, not
        // an error the user made.
        text_secondary(),
        Align::Left,
    );
    chevron_icon(b, card.right() - 22.0, card.center_y(), text_muted());
    b.hits.push((card, HitTarget::OpenOutputPicker(slot)));
    *y = card.bottom() + 12.0;
}

/// What a slot's settings row is called.
fn slot_title(slot: Slot) -> &'static str {
    match slot {
        Slot::Fallback => "Play through",
        Slot::Game => "Game channel",
        Slot::Chat => "Chat channel",
    }
}

/// The picker's heading, and the one place a slot says what it is *for*.
///
/// It goes here rather than in the row title because the title shares its line
/// with a chevron and the device name sits directly under it; "which Windows
/// role does this take" has nowhere to go in that row, and the picker is where
/// somebody is actually deciding.
fn slot_caption(slot: Slot) -> &'static str {
    match slot {
        Slot::Fallback => "PLAY THROUGH WHEN POWERED OFF",
        Slot::Game => "GAME CHANNEL — ORDINARY SOUND",
        Slot::Chat => "CHAT CHANNEL — CALLS",
    }
}

/// What a device row says beneath its title.
///
/// `chosen` is the stored device's name and whether it is present on the
/// machine right now. A stored device that is currently absent is named rather
/// than hidden: "Speakers (Realtek Audio)" being unplugged is worth seeing, and
/// falling back to the prompt would suggest the setting was never made. The
/// prompt names the slot, because "Choose a device" three times over does not
/// say which of the three is still unanswered.
pub fn output_choice_subtitle(slot: Slot, chosen: Option<(&str, bool)>) -> String {
    match chosen {
        Some((name, true)) => name.to_string(),
        Some((name, false)) => format!("{name} — not connected"),
        None => match slot {
            Slot::Fallback => "Choose a device".to_string(),
            Slot::Game => "Choose the game channel".to_string(),
            Slot::Chat => "Choose the chat channel".to_string(),
        },
    }
}

/// The output picker: every active playback device, with the chosen one marked.
///
/// A list rather than a native dropdown because the panel is a layered window
/// that hides as soon as it loses activation — opening a menu owned by another
/// window would close the very panel the menu belongs to.
fn output_body(b: &mut Builder, slot: Slot, y: &mut f32, ids: &mut Vec<String>) {
    b.caption(
        Rect::new(MARGIN, *y, CONTENT_W, 14.0),
        slot_caption(slot),
        text_secondary(),
        Align::Left,
    );
    *y += 22.0;

    let devices = crate::output_devices();
    let chosen_id = crate::settings_output_choice_id(slot);

    if devices.is_empty() {
        let card = Rect::new(MARGIN, *y, CONTENT_W, 44.0);
        b.card(card, bg_card(), border_card());
        b.text(
            Rect::new(card.x + 16.0, card.y, card.w - 32.0, card.h),
            "No playback devices found",
            FS_BODY,
            W_REGULAR,
            text_secondary(),
            Align::Left,
        );
        *y = card.bottom() + 12.0;
    }

    // Every device, never a truncated list: silently dropping the one the user
    // was looking for is worse than a tall panel on a machine with many.
    for (i, (id, name)) in devices.iter().enumerate() {
        let selected = chosen_id.as_deref() == Some(id.as_str());
        let card = Rect::new(MARGIN, *y, CONTENT_W, 44.0);
        b.p.push(Primitive::RoundRect {
            rect: card,
            radius: CARD_RADIUS,
            fill: Some(if selected {
                accent().with_alpha(0x22)
            } else {
                bg_card()
            }),
            stroke: Some(if selected { accent() } else { border_card() }),
            stroke_w: 1.0,
        });
        b.text(
            Rect::new(card.x + 16.0, card.y, card.w - 52.0, card.h),
            name,
            FS_BODY,
            if selected { W_SEMIBOLD } else { W_REGULAR },
            text_primary(),
            Align::Left,
        );
        if selected {
            check_icon(b, card.right() - 24.0, card.center_y(), accent());
        }
        b.hits.push((card, HitTarget::OutputChoice(i)));
        ids.push(id.clone());
        *y = card.bottom() + 8.0;
    }

    *y += 4.0;
    let back = Rect::new(MARGIN, *y, 88.0, 34.0);
    b.p.push(Primitive::RoundRect {
        rect: back,
        radius: BUTTON_RADIUS,
        fill: Some(bg_card()),
        stroke: Some(border_card()),
        stroke_w: 1.0,
    });
    back_icon(b, back.x + 18.0, back.center_y(), text_primary());
    b.text(
        Rect::new(back.x + 30.0, back.y, back.w - 34.0, back.h),
        "Back",
        FS_BODY,
        W_SEMIBOLD,
        text_primary(),
        Align::Left,
    );
    b.hits.push((back, HitTarget::Back));
    *y = back.bottom() + GAP * 0.5;
}

fn toggle(b: &mut Builder, r: Rect, on: bool) {
    b.p.push(Primitive::RoundRect {
        rect: r,
        radius: r.h / 2.0,
        fill: Some(if on {
            accent().with_alpha(0xAA)
        } else {
            track_inactive()
        }),
        stroke: None,
        stroke_w: 0.0,
    });
    let knob_x = if on {
        r.right() - r.h / 2.0
    } else {
        r.x + r.h / 2.0
    };
    b.p.push(Primitive::Circle {
        cx: knob_x,
        cy: r.center_y(),
        r: r.h / 2.0 - 3.0,
        fill: Some(if on { toggle_knob() } else { text_secondary() }),
        stroke: None,
        stroke_w: 0.0,
    });
}

// ------------------------------------------------------------------- icons ---
// Drawn as primitives rather than an icon font, so they render identically
// regardless of which fonts are installed.

fn gear_icon(b: &mut Builder, cx: f32, cy: f32, c: Color) {
    b.p.push(Primitive::Circle {
        cx,
        cy,
        r: 4.2,
        fill: None,
        stroke: Some(c),
        stroke_w: 1.4,
    });
    for i in 0..6 {
        let a = std::f32::consts::PI * 2.0 * (i as f32 / 6.0);
        let (s, co) = a.sin_cos();
        b.p.push(Primitive::Line {
            x0: cx + co * 5.0,
            y0: cy + s * 5.0,
            x1: cx + co * 7.2,
            y1: cy + s * 7.2,
            w: 1.6,
            color: c,
        });
    }
}

fn mic_icon(b: &mut Builder, cx: f32, cy: f32, c: Color, muted: bool) {
    // Capsule body. Wider and shorter than a first guess: drawn tall and narrow
    // at this size it reads as an arrow rather than a microphone.
    b.p.push(Primitive::RoundRect {
        rect: Rect::new(cx - 3.2, cy - 7.5, 6.4, 9.0),
        radius: 3.2,
        fill: Some(c),
        stroke: None,
        stroke_w: 0.0,
    });
    // Cradle: a U under the body, drawn as three segments.
    b.p.push(Primitive::Path {
        points: vec![
            (cx - 6.0, cy - 0.5),
            (cx - 6.0, cy + 2.0),
            (cx - 3.0, cy + 4.6),
            (cx + 3.0, cy + 4.6),
            (cx + 6.0, cy + 2.0),
            (cx + 6.0, cy - 0.5),
        ],
        closed: false,
        fill: None,
        stroke: Some(c),
        stroke_w: 1.5,
    });
    b.p.push(Primitive::Line {
        x0: cx,
        y0: cy + 4.6,
        x1: cx,
        y1: cy + 7.5,
        w: 1.5,
        color: c,
    });
    if muted {
        b.p.push(Primitive::Line {
            x0: cx - 7.5,
            y0: cy + 8.0,
            x1: cx + 7.5,
            y1: cy - 8.0,
            w: 1.8,
            color: c,
        });
    }
}

fn battery_icon(b: &mut Builder, cx: f32, cy: f32, pct: Option<u8>, c: Color) {
    let body = Rect::new(cx - 12.0, cy - 7.0, 22.0, 13.0);
    b.p.push(Primitive::RoundRect {
        rect: body,
        radius: 3.5,
        fill: None,
        stroke: Some(c),
        stroke_w: 1.4,
    });
    b.p.push(Primitive::RoundRect {
        rect: Rect::new(body.right() + 1.5, cy - 3.0, 2.2, 6.0),
        radius: 1.0,
        fill: Some(c),
        stroke: None,
        stroke_w: 0.0,
    });
    if let Some(p) = pct {
        let inner = body.w - 5.0;
        let w = (inner * (p as f32 / 100.0)).max(1.5);
        b.p.push(Primitive::RoundRect {
            rect: Rect::new(body.x + 2.5, body.y + 2.5, w, body.h - 5.0),
            radius: 1.6,
            fill: Some(accent()),
            stroke: None,
            stroke_w: 0.0,
        });
    }
}

fn swap_icon(b: &mut Builder, cx: f32, cy: f32, c: Color) {
    b.p.push(Primitive::Path {
        points: vec![(cx - 5.0, cy - 2.5), (cx + 5.0, cy - 2.5)],
        closed: false,
        fill: None,
        stroke: Some(c),
        stroke_w: 1.4,
    });
    b.p.push(Primitive::Path {
        points: vec![(cx + 2.5, cy - 5.0), (cx + 5.0, cy - 2.5), (cx + 2.5, cy)],
        closed: false,
        fill: None,
        stroke: Some(c),
        stroke_w: 1.4,
    });
    b.p.push(Primitive::Path {
        points: vec![(cx + 5.0, cy + 2.5), (cx - 5.0, cy + 2.5)],
        closed: false,
        fill: None,
        stroke: Some(c),
        stroke_w: 1.4,
    });
    b.p.push(Primitive::Path {
        points: vec![(cx - 2.5, cy), (cx - 5.0, cy + 2.5), (cx - 2.5, cy + 5.0)],
        closed: false,
        fill: None,
        stroke: Some(c),
        stroke_w: 1.4,
    });
}

fn warning_icon(b: &mut Builder, cx: f32, cy: f32, c: Color) {
    b.p.push(Primitive::Path {
        points: vec![(cx, cy - 7.0), (cx + 7.5, cy + 6.0), (cx - 7.5, cy + 6.0)],
        closed: true,
        fill: None,
        stroke: Some(c),
        stroke_w: 1.3,
    });
    b.p.push(Primitive::Line {
        x0: cx,
        y0: cy - 2.5,
        x1: cx,
        y1: cy + 1.5,
        w: 1.3,
        color: c,
    });
}

fn refresh_icon(b: &mut Builder, cx: f32, cy: f32) {
    let c = text_muted();
    b.p.push(Primitive::Path {
        points: vec![
            (cx + 6.0, cy - 1.0),
            (cx + 4.0, cy - 5.0),
            (cx - 1.0, cy - 6.5),
            (cx - 5.5, cy - 3.5),
            (cx - 6.0, cy + 1.5),
            (cx - 3.0, cy + 5.5),
            (cx + 2.0, cy + 6.0),
        ],
        closed: false,
        fill: None,
        stroke: Some(c),
        stroke_w: 1.4,
    });
    b.p.push(Primitive::Path {
        points: vec![
            (cx + 2.5, cy - 4.5),
            (cx + 6.5, cy - 1.5),
            (cx + 3.0, cy + 1.5),
        ],
        closed: false,
        fill: None,
        stroke: Some(c),
        stroke_w: 1.4,
    });
}

/// The "opens another screen" chevron, pointing the way the panel will move.
fn chevron_icon(b: &mut Builder, cx: f32, cy: f32, c: Color) {
    b.p.push(Primitive::Path {
        points: vec![(cx - 2.5, cy - 4.5), (cx + 2.5, cy), (cx - 2.5, cy + 4.5)],
        closed: false,
        fill: None,
        stroke: Some(c),
        stroke_w: 1.6,
    });
}

fn check_icon(b: &mut Builder, cx: f32, cy: f32, c: Color) {
    b.p.push(Primitive::Path {
        points: vec![(cx - 5.0, cy), (cx - 1.5, cy + 3.5), (cx + 5.0, cy - 4.0)],
        closed: false,
        fill: None,
        stroke: Some(c),
        stroke_w: 1.8,
    });
}

fn back_icon(b: &mut Builder, cx: f32, cy: f32, c: Color) {
    b.p.push(Primitive::Path {
        points: vec![(cx + 3.0, cy - 4.5), (cx - 2.0, cy), (cx + 3.0, cy + 4.5)],
        closed: false,
        fill: None,
        stroke: Some(c),
        stroke_w: 1.6,
    });
    b.p.push(Primitive::Line {
        x0: cx - 2.0,
        y0: cy,
        x1: cx + 6.0,
        y1: cy,
        w: 1.6,
        color: c,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connected() -> HeadsetState {
        HeadsetState {
            device_name: Some("BlackShark V3 Pro PS HID".into()),
            connected: Some(true),
            battery: Some(49),
            sidetone: Some(0),
            game_chat: Some(10),
            noise: Some(NoiseControl {
                mode: NoiseMode::Anc,
                anc_level: 3,
            }),
            mic_mute_hardware: Some(false),
            mic_mute_os: Some(false),
            warn_vendor_software: true,
            dongle_silent: false,
            model: Some("BlackShark V3 Pro"),
            features: crate::state::Features::default(),
            sidetone_unconfirmed: false,
        }
    }

    /// The V2 Pro as the worker leaves it: no game/chat, no noise control,
    /// and a sidetone level the device cannot report.
    fn v2_pro() -> HeadsetState {
        HeadsetState {
            device_name: Some("Razer BlackShark V2 Pro 2.4".into()),
            model: Some("BlackShark V2 Pro"),
            features: crate::state::Features {
                game_chat: false,
                noise: false,
                sidetone_readable: false,
            },
            game_chat: None,
            noise: None,
            sidetone: None,
            ..connected()
        }
    }

    fn texts(p: &Panel) -> Vec<String> {
        p.primitives
            .iter()
            .filter_map(|prim| match prim {
                Primitive::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_device_without_game_chat_offers_no_switcher() {
        // Asked for game/chat, which the V2 Pro does not have: the panel draws
        // sidetone and nothing to switch to.
        let p = build(&v2_pro(), View::Main, SliderParam::GameChat, None);
        let hits = targets(&p);
        assert!(!hits.contains(&HitTarget::Switcher), "{hits:?}");
        assert!(hits.contains(&HitTarget::SliderTrack));
        assert_eq!(p.track.map(|t| t.param), Some(SliderParam::Sidetone));
        assert!(texts(&p).iter().any(|t| t == "Sidetone"), "{:?}", texts(&p));
    }

    #[test]
    fn a_device_without_noise_control_draws_no_noise_row() {
        let p = build(&v2_pro(), View::Main, SliderParam::Sidetone, None);
        let hits = targets(&p);
        for t in [
            HitTarget::NoiseOff,
            HitTarget::NoiseAnc,
            HitTarget::NoiseAmbient,
            HitTarget::NoiseLevel,
        ] {
            assert!(!hits.contains(&t), "{t:?} drawn for a device without it");
        }
        assert!(p.level_track.is_none());
        assert!(
            p.height < build(&connected(), View::Main, SliderParam::Sidetone, None).height,
            "the panel should shrink rather than leave a gap"
        );
    }

    #[test]
    fn an_unconfirmed_sidetone_level_says_it_was_sent() {
        let mut s = v2_pro();
        s.sidetone = Some(13);
        s.sidetone_unconfirmed = true;
        let p = build(&s, View::Main, SliderParam::Sidetone, None);
        let t = texts(&p);
        assert!(t.iter().any(|t| t.ends_with(" · SENT")), "{t:?}");
    }

    #[test]
    fn an_unknown_sidetone_level_is_a_gap_not_a_number() {
        let p = build(&v2_pro(), View::Main, SliderParam::Sidetone, None);
        assert!(texts(&p).iter().any(|t| t == "--"), "{:?}", texts(&p));
    }

    #[test]
    fn a_confirmed_level_carries_no_marker() {
        let p = build(&connected(), View::Main, SliderParam::Sidetone, None);
        assert!(!texts(&p).iter().any(|t| t.contains("SENT")));
    }

    #[test]
    fn tick_counts_follow_the_parameter_range() {
        assert_eq!(SliderParam::Sidetone.ticks(), 16);
        assert_eq!(SliderParam::GameChat.ticks(), 21);
    }

    #[test]
    fn value_and_position_round_trip_at_every_tick() {
        for param in [SliderParam::Sidetone, SliderParam::GameChat] {
            let g = TrackGeometry {
                x0: 26.0,
                x1: 332.0,
                y: 100.0,
                param,
            };
            for v in 0..=param.max() {
                assert_eq!(g.value_at(g.x_for(v)), v, "{param:?} value {v}");
            }
        }
    }

    #[test]
    fn the_track_spans_the_measured_width() {
        let p = build(&connected(), View::Main, SliderParam::GameChat, None);
        let g = p.track.expect("main view has a track");
        // The mockup's dot centres are at x 26 and x 332, but those are
        // screenshot coordinates and include the 8 px shadow margin. Layout
        // works panel-local, so the same points are 18 and 324. Getting this
        // wrong once is what this assertion exists to prevent.
        assert!((g.x0 - 18.0).abs() < 1.0, "x0 was {}", g.x0);
        assert!((g.x1 - 324.0).abs() < 1.0, "x1 was {}", g.x1);
        assert!(
            (g.x1 - g.x0 - 306.0).abs() < 1.0,
            "the measured track is 306 px wide, got {}",
            g.x1 - g.x0
        );
    }

    #[test]
    fn positions_clamp_outside_the_track() {
        let g = TrackGeometry {
            x0: 26.0,
            x1: 332.0,
            y: 0.0,
            param: SliderParam::GameChat,
        };
        assert_eq!(g.value_at(-500.0), 0);
        assert_eq!(g.value_at(9999.0), 20);
    }

    #[test]
    fn value_text_matches_the_mockups_exactly() {
        use SliderParam::*;
        assert_eq!(format_value(GameChat, Some(10)), "Balanced");
        assert_eq!(format_value(GameChat, Some(17)), "Chat +7");
        assert_eq!(format_value(GameChat, Some(3)), "Game +7");
        assert_eq!(format_value(Sidetone, Some(0)), "Off");
        assert_eq!(format_value(Sidetone, Some(14)), "14");
    }

    #[test]
    fn a_released_slider_keeps_showing_where_it_was_released() {
        // The bug: releasing the knob cleared the preview, so the panel fell
        // back to the device's value - which is still the old one, because the
        // write has not even been sent yet. The knob visibly jumped back to
        // where it started and only reached the released position when the
        // read-back landed, 250 ms per exchange later.
        assert_eq!(
            slider_preview(
                SliderParam::Sidetone,
                None,
                Some((SliderParam::Sidetone, 12))
            ),
            Some(12),
            "a committed value must stay on screen until the device answers"
        );
    }

    #[test]
    fn a_live_drag_wins_over_a_value_still_being_confirmed() {
        // Grabbing the knob again before the previous write is confirmed: the
        // hand beats the stale commitment.
        assert_eq!(
            slider_preview(
                SliderParam::GameChat,
                Some(3),
                Some((SliderParam::GameChat, 17))
            ),
            Some(3)
        );
    }

    #[test]
    fn a_pending_value_is_not_shown_against_the_other_parameter() {
        // One slider serves two parameters, and the switcher can be pressed
        // while a write is in flight. Showing a pending sidetone value on the
        // game/chat track would draw a number the device never reported for it.
        assert_eq!(
            slider_preview(
                SliderParam::GameChat,
                None,
                Some((SliderParam::Sidetone, 12))
            ),
            None
        );
    }

    #[test]
    fn with_nothing_in_flight_the_device_value_is_used() {
        assert_eq!(slider_preview(SliderParam::Sidetone, None, None), None);
    }

    #[test]
    fn an_unknown_value_never_renders_as_a_number() {
        assert_eq!(format_value(SliderParam::Sidetone, None), "--");
        assert_eq!(format_value(SliderParam::GameChat, None), "--");
    }

    fn in_mode(mode: NoiseMode, anc_level: u8) -> HeadsetState {
        HeadsetState {
            noise: Some(NoiseControl { mode, anc_level }),
            ..connected()
        }
    }

    fn targets(p: &Panel) -> Vec<HitTarget> {
        p.hits.iter().map(|(_, t)| *t).collect()
    }

    #[test]
    fn the_noise_row_offers_all_three_modes_as_separate_regions() {
        let p = build(&connected(), View::Main, SliderParam::GameChat, None);
        let segs: Vec<Rect> = [
            HitTarget::NoiseOff,
            HitTarget::NoiseAnc,
            HitTarget::NoiseAmbient,
        ]
        .iter()
        .map(|want| {
            p.hits
                .iter()
                .find(|(_, t)| t == want)
                .unwrap_or_else(|| panic!("{want:?} is not reachable"))
                .0
        })
        .collect();

        // Left to right, touching but never overlapping, spanning the content.
        assert!(segs[0].right() <= segs[1].x, "off overlaps anc");
        assert!(segs[1].right() <= segs[2].x, "anc overlaps ambient");
        assert!((segs[0].x - MARGIN).abs() < 1.0);
        assert!((segs[2].right() - (MARGIN + CONTENT_W)).abs() < 1.0);
    }

    #[test]
    fn the_active_segment_follows_the_device_mode() {
        let at = |m| {
            active_noise_segment(Some(NoiseControl {
                mode: m,
                anc_level: 3,
            }))
        };
        assert_eq!(at(NoiseMode::Off), Some(0));
        assert_eq!(at(NoiseMode::Anc), Some(1));
        assert_eq!(at(NoiseMode::Ambient), Some(2));
        // A mode byte we have no evidence for must not light a segment that
        // claims the device is in a state we cannot name.
        assert_eq!(at(NoiseMode::Unrecognised(0x02)), None);
        assert_eq!(active_noise_segment(None), None);
    }

    #[test]
    fn the_level_track_is_interactive_only_in_anc_mode() {
        let anc = build(
            &in_mode(NoiseMode::Anc, 3),
            View::Main,
            SliderParam::GameChat,
            None,
        );
        assert!(targets(&anc).contains(&HitTarget::NoiseLevel));
        assert!(anc.level_track.is_some());

        for mode in [
            NoiseMode::Off,
            NoiseMode::Ambient,
            NoiseMode::Unrecognised(2),
        ] {
            let p = build(&in_mode(mode, 3), View::Main, SliderParam::GameChat, None);
            assert!(
                !targets(&p).contains(&HitTarget::NoiseLevel),
                "{mode:?} has no level to set"
            );
        }
    }

    #[test]
    fn level_positions_round_trip_at_every_step() {
        let t = LevelTrack {
            x0: 18.0,
            x1: 324.0,
            y: 0.0,
        };
        for level in 1..=4u8 {
            assert_eq!(t.level_at(t.x_for(level)), level, "level {level}");
        }
    }

    #[test]
    fn a_click_between_dots_snaps_to_the_nearest_level() {
        // The common case: the track is 4 dots wide and a click almost never
        // lands exactly on one. Truncating instead of rounding would bias every
        // click downward, which a round-trip over exact tick positions cannot
        // detect.
        let t = LevelTrack {
            x0: 0.0,
            x1: 300.0,
            y: 0.0,
        };
        // Dots are 100 px apart on this track.
        assert_eq!(t.level_at(t.x_for(2) + 60.0), 3, "60% of the way to 3");
        assert_eq!(t.level_at(t.x_for(2) + 40.0), 2, "40% of the way to 3");
        assert_eq!(t.level_at(t.x_for(3) - 60.0), 2);
    }

    #[test]
    fn level_positions_clamp_to_the_observed_four() {
        let t = LevelTrack {
            x0: 18.0,
            x1: 324.0,
            y: 0.0,
        };
        assert_eq!(t.level_at(-500.0), 1);
        assert_eq!(t.level_at(9999.0), 4);
    }

    #[test]
    fn noise_text_never_claims_more_than_was_observed() {
        let at = |m, l| {
            format_noise(Some(NoiseControl {
                mode: m,
                anc_level: l,
            }))
        };
        assert_eq!(at(NoiseMode::Off, 4), "Off");
        assert_eq!(at(NoiseMode::Anc, 3), "ANC 3");
        // Ambient has no level, so naming one here would state a fact the
        // captures contradict.
        assert_eq!(at(NoiseMode::Ambient, 4), "Ambient");
        assert_eq!(at(NoiseMode::Unrecognised(0x02), 4), "--");
        assert_eq!(format_noise(None), "--");
    }

    /// x of the knob on the level row. Scoped by row, because the game/chat
    /// slider's knob has the same radius and would otherwise be picked up.
    fn knob_x(p: &Panel, row_y: f32) -> Option<f32> {
        p.primitives.iter().find_map(|prim| match prim {
            Primitive::Circle { cx, cy, r, .. }
                if (*r - KNOB_R).abs() < 0.01 && (*cy - row_y).abs() < 0.01 =>
            {
                Some(*cx)
            }
            _ => None,
        })
    }

    /// The level row's y, taken from the one mode that exposes its geometry.
    /// The section is laid out identically in every mode by design, which is
    /// what stops the panel jumping when the mode changes.
    fn level_row_y() -> f32 {
        build(
            &in_mode(NoiseMode::Anc, 2),
            View::Main,
            SliderParam::GameChat,
            None,
        )
        .level_track
        .expect("anc exposes the track")
        .y
    }

    #[test]
    fn the_retained_level_stays_visible_in_every_mode() {
        // The device keeps byte 1 while off and while in ambient, and lands on
        // it when ANC comes back. Hiding it would make the panel claim the
        // level is unknown when the device is telling us exactly what it is.
        let anc = build(
            &in_mode(NoiseMode::Anc, 2),
            View::Main,
            SliderParam::GameChat,
            None,
        );
        let want = anc.level_track.unwrap().x_for(2);
        let row = level_row_y();

        for mode in [NoiseMode::Off, NoiseMode::Ambient] {
            let p = build(&in_mode(mode, 2), View::Main, SliderParam::GameChat, None);
            let x = knob_x(&p, row).unwrap_or_else(|| panic!("{mode:?} drew no level knob"));
            assert!((x - want).abs() < 0.01, "{mode:?} knob at {x}, want {want}");
        }
    }

    #[test]
    fn an_unrecognised_mode_draws_no_level_at_all() {
        // Byte 1's meaning is only established for the three observed modes.
        let p = build(
            &in_mode(NoiseMode::Unrecognised(0x02), 2),
            View::Main,
            SliderParam::GameChat,
            None,
        );
        assert_eq!(knob_x(&p, level_row_y()), None);
    }

    #[test]
    fn the_settings_view_offers_all_three_appearance_choices() {
        let p = build(&connected(), View::Settings, SliderParam::GameChat, None);
        let ts = targets(&p);
        for want in [
            HitTarget::AppearanceSystem,
            HitTarget::AppearanceDark,
            HitTarget::AppearanceLight,
        ] {
            assert!(ts.contains(&want), "{want:?} is not reachable");
        }
    }

    #[test]
    fn the_appearance_segments_do_not_overlap() {
        let p = build(&connected(), View::Settings, SliderParam::GameChat, None);
        let seg = |t: HitTarget| p.hits.iter().find(|(_, h)| *h == t).expect("present").0;
        let (a, b, c) = (
            seg(HitTarget::AppearanceSystem),
            seg(HitTarget::AppearanceDark),
            seg(HitTarget::AppearanceLight),
        );
        assert!(a.right() <= b.x, "system overlaps dark");
        assert!(b.right() <= c.x, "dark overlaps light");
    }

    #[test]
    fn the_appearance_subtitle_says_what_actually_resolved() {
        // AUTO must not be a black box: a user whose Windows is dark and who
        // picks AUTO should be able to see why the panel stayed dark.
        use headset_protocol as _;
        assert_eq!(appearance_subtitle(Appearance::Light), "Light theme");
        assert_eq!(appearance_subtitle(Appearance::Dark), "Dark theme");
        let s = appearance_subtitle(Appearance::System);
        assert!(s.starts_with("Following Windows"), "{s}");
        assert!(s.ends_with("light") || s.ends_with("dark"), "{s}");
    }

    #[test]
    fn a_disconnected_headset_offers_no_noise_controls() {
        let off = HeadsetState {
            connected: Some(false),
            noise: None,
            ..connected()
        };
        let p = build(&off, View::Main, SliderParam::GameChat, None);
        for t in [
            HitTarget::NoiseOff,
            HitTarget::NoiseAnc,
            HitTarget::NoiseAmbient,
            HitTarget::NoiseLevel,
        ] {
            assert!(!targets(&p).contains(&t), "{t:?} must not hit-test");
        }
    }

    #[test]
    fn every_interactive_target_is_hittable_in_the_main_view() {
        let p = build(&connected(), View::Main, SliderParam::GameChat, None);
        for want in [
            HitTarget::Gear,
            HitTarget::MutePill,
            HitTarget::Switcher,
            HitTarget::SliderTrack,
            HitTarget::Refresh,
        ] {
            let found = p.hits.iter().any(|(_, t)| *t == want);
            assert!(found, "{want:?} has no hit region");
        }
    }

    #[test]
    fn clicking_the_knob_position_hits_the_track() {
        let p = build(&connected(), View::Main, SliderParam::GameChat, None);
        let g = p.track.unwrap();
        assert_eq!(p.hit(g.x_for(10), g.y), Some(HitTarget::SliderTrack));
    }

    #[test]
    fn a_disconnected_panel_offers_no_controls() {
        let mut s = connected();
        s.connected = Some(false);
        s.battery = None;
        s.sidetone = None;
        s.game_chat = None;
        let p = build(&s, View::Main, SliderParam::Sidetone, None);
        for banned in [
            HitTarget::MutePill,
            HitTarget::Switcher,
            HitTarget::SliderTrack,
        ] {
            assert!(
                !p.hits.iter().any(|(_, t)| *t == banned),
                "{banned:?} must not be clickable while disconnected"
            );
        }
        // Gear and Refresh stay available: both work without the headset.
        assert!(p.hits.iter().any(|(_, t)| *t == HitTarget::Gear));
        assert!(p.hits.iter().any(|(_, t)| *t == HitTarget::Refresh));
    }

    #[test]
    fn a_hardware_muted_mic_is_not_clickable() {
        let mut s = connected();
        s.mic_mute_hardware = Some(true);
        let p = build(&s, View::Main, SliderParam::Sidetone, None);
        assert!(
            !p.hits.iter().any(|(_, t)| *t == HitTarget::MutePill),
            "software cannot release a hardware switch, so it must not offer to"
        );
    }

    #[test]
    fn the_banner_appears_only_when_warranted() {
        let with = build(&connected(), View::Main, SliderParam::Sidetone, None);
        let mut s = connected();
        s.warn_vendor_software = false;
        let without = build(&s, View::Main, SliderParam::Sidetone, None);
        assert!(
            with.height > without.height,
            "the banner should make the panel taller"
        );
    }

    #[test]
    fn settings_view_exposes_both_toggles_and_back() {
        let p = build(&connected(), View::Settings, SliderParam::Sidetone, None);
        for want in [
            HitTarget::ToggleStartup,
            HitTarget::ToggleWarning,
            HitTarget::Back,
        ] {
            assert!(p.hits.iter().any(|(_, t)| *t == want), "{want:?} missing");
        }
        assert!(p.track.is_none(), "settings has no slider");
    }

    #[test]
    fn settings_offers_the_output_switch_and_a_way_to_choose_a_device() {
        let p = build(&connected(), View::Settings, SliderParam::Sidetone, None);
        for want in [
            HitTarget::ToggleOutputSwitch,
            HitTarget::ToggleSplit,
            HitTarget::OpenOutputPicker(Slot::Fallback),
        ] {
            assert!(p.hits.iter().any(|(_, t)| *t == want), "{want:?} missing");
        }
    }

    #[test]
    fn the_picker_row_is_reachable_even_with_the_switch_turned_off() {
        // The row is what makes the toggle mean anything, so hiding it until
        // the toggle is on would leave a switch with nothing to switch to.
        let p = build(&connected(), View::Settings, SliderParam::Sidetone, None);
        let row = p
            .hits
            .iter()
            .find(|(_, t)| *t == HitTarget::OpenOutputPicker(Slot::Fallback))
            .expect("the picker row must always be present")
            .0;
        assert!(row.w > 0.0 && row.h > 0.0);
    }

    /// The channel rows follow the split toggle, and the toggle is what reveals
    /// them -- so whichever way the machine running this has it set, the two
    /// have to agree. A pair of rows shown with the split off would be inert
    /// controls; a split turned on with no rows would be a setting that cannot
    /// be finished.
    #[test]
    fn the_channel_rows_are_shown_exactly_when_the_split_is_on() {
        let p = build(&connected(), View::Settings, SliderParam::Sidetone, None);
        let shown = |slot| {
            p.hits
                .iter()
                .any(|(_, t)| *t == HitTarget::OpenOutputPicker(slot))
        };
        let split_on = crate::settings_split_game_and_chat();
        assert_eq!(shown(Slot::Game), split_on);
        assert_eq!(shown(Slot::Chat), split_on);
    }

    /// The three slots are chosen from one picker drawing one list, so the only
    /// thing telling a user which choice they are making is the wording. Two
    /// slots sharing a title or a heading would make that guesswork.
    #[test]
    fn the_three_slots_are_told_apart_wherever_they_are_named() {
        let slots = [Slot::Fallback, Slot::Game, Slot::Chat];
        let titles: std::collections::BTreeSet<&str> =
            slots.iter().map(|s| slot_title(*s)).collect();
        let captions: std::collections::BTreeSet<&str> =
            slots.iter().map(|s| slot_caption(*s)).collect();
        assert_eq!(titles.len(), slots.len(), "{titles:?}");
        assert_eq!(captions.len(), slots.len(), "{captions:?}");
        // The heading is where a channel says which Windows role it takes, so
        // it has to at least name the channel.
        assert!(slot_caption(Slot::Game).contains("GAME"));
        assert!(slot_caption(Slot::Chat).contains("CHAT"));
    }

    #[test]
    fn the_output_subtitle_distinguishes_unset_from_unplugged() {
        // Three different facts, and collapsing any two of them misleads: no
        // choice made, a choice that is live, and a choice that is currently
        // absent from the machine.
        assert_eq!(
            output_choice_subtitle(Slot::Fallback, None),
            "Choose a device"
        );
        assert_eq!(
            output_choice_subtitle(Slot::Fallback, Some(("Speakers (Realtek(R) Audio)", true))),
            "Speakers (Realtek(R) Audio)"
        );
        let gone =
            output_choice_subtitle(Slot::Fallback, Some(("Speakers (Realtek(R) Audio)", false)));
        assert!(gone.starts_with("Speakers (Realtek(R) Audio)"), "{gone}");
        assert!(gone.contains("not connected"), "{gone}");

        // An unanswered channel says which channel it is waiting on: three
        // rows all reading "Choose a device" would not.
        let prompts: std::collections::BTreeSet<String> = [Slot::Fallback, Slot::Game, Slot::Chat]
            .iter()
            .map(|s| output_choice_subtitle(*s, None))
            .collect();
        assert_eq!(prompts.len(), 3, "{prompts:?}");
    }

    #[test]
    fn the_picker_view_lists_ids_in_the_order_it_drew_them() {
        // Off Windows the device list is empty, so this pins the invariant that
        // holds either way: one id recorded per drawn choice, in the same order.
        // A mismatch is how a click selects a device the user did not see.
        let p = build(
            &connected(),
            View::Output(Slot::Fallback),
            SliderParam::Sidetone,
            None,
        );
        let choices: Vec<usize> = p
            .hits
            .iter()
            .filter_map(|(_, t)| match t {
                HitTarget::OutputChoice(i) => Some(*i),
                _ => None,
            })
            .collect();
        assert_eq!(choices.len(), p.output_ids.len());
        assert!(
            choices.iter().enumerate().all(|(n, i)| n == *i),
            "choice indices must be positional: {choices:?}"
        );
    }

    #[test]
    fn the_picker_view_can_always_be_left() {
        // A view with a list that may be empty still needs a way out.
        let p = build(
            &connected(),
            View::Output(Slot::Fallback),
            SliderParam::Sidetone,
            None,
        );
        assert!(p.hits.iter().any(|(_, t)| *t == HitTarget::Back));
        assert!(p.track.is_none(), "the picker has no slider");
    }

    #[test]
    fn only_the_main_view_offers_the_gear() {
        // Settings and the picker are both places you arrive at, so both show
        // Back. Offering Gear inside Settings would be a button to where you are.
        let main = build(&connected(), View::Main, SliderParam::Sidetone, None);
        assert!(main.hits.iter().any(|(_, t)| *t == HitTarget::Gear));
        for view in [View::Settings, View::Output(Slot::Fallback)] {
            let p = build(&connected(), view, SliderParam::Sidetone, None);
            assert!(
                !p.hits.iter().any(|(_, t)| *t == HitTarget::Gear),
                "{view:?} must not offer the gear"
            );
        }
    }

    #[test]
    fn a_silent_dongle_is_not_reported_as_searching() {
        // The whole point of the state: SEARCHING is what the panel shows
        // while starting up, so leaving a wedged dongle on it gave the user no
        // way to tell a cold start from a dongle that had stopped answering.
        let mut s = connected();
        s.connected = None;
        s.dongle_silent = true;
        let p = build(&s, View::Main, SliderParam::GameChat, None);
        let text: Vec<&str> = p
            .primitives
            .iter()
            .filter_map(|prim| match prim {
                Primitive::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            !text.contains(&"SEARCHING"),
            "a silent dongle must not read as SEARCHING: {text:?}"
        );
        assert!(
            text.iter()
                .any(|t| t.contains("Unplug it and plug it back in")),
            "the panel should name the remedy: {text:?}"
        );
    }

    #[test]
    fn a_silent_dongle_outranks_a_stale_connected_reading() {
        // `connected` may still hold the last value read before the dongle went
        // quiet. Showing CONNECTED over a channel that answers nothing is the
        // worst of the options.
        let mut s = connected();
        s.dongle_silent = true;
        let p = build(&s, View::Main, SliderParam::GameChat, None);
        let has_connected = p.primitives.iter().any(
            |prim| matches!(prim, Primitive::Text { text, .. } if text.starts_with("CONNECTED")),
        );
        assert!(!has_connected, "a silent dongle must not read as CONNECTED");
    }

    #[test]
    fn preview_overrides_the_device_value_while_dragging() {
        let p = build(&connected(), View::Main, SliderParam::GameChat, Some(17));
        let has_text = p
            .primitives
            .iter()
            .any(|prim| matches!(prim, Primitive::Text { text, .. } if text == "Chat +7"));
        assert!(
            has_text,
            "the dragged value should be shown, not the stored one"
        );
    }

    #[test]
    fn the_panel_is_the_measured_width() {
        let p = build(&connected(), View::Main, SliderParam::Sidetone, None);
        match p.primitives.first() {
            Some(Primitive::RoundRect { rect, .. }) => {
                assert_eq!(rect.w, 342.0);
                assert_eq!(rect.h, p.height);
            }
            other => panic!("expected the panel background first, got {other:?}"),
        }
    }
}
