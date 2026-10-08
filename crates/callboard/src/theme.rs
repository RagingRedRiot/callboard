//! The look: design tokens (colors, radii, spacing, type), the bundled fonts
//! (Inter and Phosphor icons), and their application to egui's style. Every
//! color drawn by hand comes from [`Palette`], so the two themes stay in step.
use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Shadow,
    Stroke, TextStyle, Theme,
};
use std::sync::Arc;

/// Phosphor icons used in the app (regular weight), by codepoint.
pub mod icon {
    pub const ARCHIVE: &str = "\u{E00C}";
    pub const ARROW_SQUARE_OUT: &str = "\u{E5DE}";
    pub const ARROWS_CLOCKWISE: &str = "\u{E094}";
    pub const ARROWS_LEFT_RIGHT: &str = "\u{E0A0}";
    pub const CARET_DOWN: &str = "\u{E136}";
    pub const DOTS_THREE: &str = "\u{E1FE}";
    pub const EYE: &str = "\u{E220}";
    pub const KANBAN: &str = "\u{EB54}";
    pub const MAGNIFYING_GLASS: &str = "\u{E30C}";
    pub const MINUS: &str = "\u{E32A}";
    pub const MOON_STARS: &str = "\u{E58E}";
    pub const PLUS: &str = "\u{E3D4}";
    pub const RSS_SIMPLE: &str = "\u{E402}";
    pub const SQUARES_FOUR: &str = "\u{E464}";
    pub const WARNING_CIRCLE: &str = "\u{E4E2}";
    pub const X: &str = "\u{E4F6}";
}

/// Colors for one theme. Neutrals are slate, from the app icon; the single
/// accent is the icon's teal, used for focus, selection, and what is new.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// Behind the cards.
    pub canvas: Color32,
    /// The dot grid on the canvas.
    pub grid: Color32,
    /// Cards, panels, menus.
    pub surface: Color32,
    /// A step up from `surface`: fields, chips, uncolored notes.
    pub raised: Color32,
    /// Hovered rows and buttons.
    pub hover: Color32,
    /// Pressed buttons.
    pub pressed: Color32,
    pub hairline: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub faint: Color32,
    pub accent: Color32,
    /// Accent as text, readable on `surface`.
    pub accent_text: Color32,
    /// Accent behind text: badges, selection.
    pub accent_soft: Color32,
    pub danger: Color32,
    pub warning: Color32,
    pub warning_soft: Color32,
    pub success: Color32,
    pub shadow: Color32,
}

pub const LIGHT: Palette = Palette {
    canvas: Color32::from_rgb(0xEC, 0xEF, 0xF3),
    grid: Color32::from_rgb(0xC6, 0xCE, 0xD8),
    surface: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    raised: Color32::from_rgb(0xF4, 0xF6, 0xF8),
    hover: Color32::from_rgb(0xEE, 0xF1, 0xF5),
    pressed: Color32::from_rgb(0xE3, 0xE8, 0xEE),
    hairline: Color32::from_rgb(0xE1, 0xE5, 0xEB),
    text: Color32::from_rgb(0x0F, 0x17, 0x2A),
    muted: Color32::from_rgb(0x55, 0x60, 0x72),
    faint: Color32::from_rgb(0x8A, 0x94, 0xA6),
    accent: Color32::from_rgb(0x2D, 0x9C, 0xBA),
    accent_text: Color32::from_rgb(0x1A, 0x78, 0x93),
    accent_soft: Color32::from_rgb(0xDF, 0xF1, 0xF6),
    danger: Color32::from_rgb(0xC9, 0x37, 0x30),
    warning: Color32::from_rgb(0xA8, 0x6A, 0x0C),
    warning_soft: Color32::from_rgb(0xFB, 0xEE, 0xD5),
    success: Color32::from_rgb(0x2F, 0x8F, 0x5B),
    shadow: Color32::from_rgba_premultiplied(0x0F, 0x17, 0x2A, 0x1C),
};

pub const DARK: Palette = Palette {
    canvas: Color32::from_rgb(0x0B, 0x0F, 0x15),
    grid: Color32::from_rgb(0x25, 0x2F, 0x3C),
    surface: Color32::from_rgb(0x15, 0x1B, 0x24),
    raised: Color32::from_rgb(0x1C, 0x24, 0x30),
    hover: Color32::from_rgb(0x21, 0x2A, 0x37),
    pressed: Color32::from_rgb(0x2A, 0x34, 0x43),
    hairline: Color32::from_rgb(0x26, 0x30, 0x3D),
    text: Color32::from_rgb(0xE6, 0xEA, 0xF0),
    muted: Color32::from_rgb(0x9A, 0xA4, 0xB5),
    faint: Color32::from_rgb(0x6A, 0x75, 0x88),
    accent: Color32::from_rgb(0x3F, 0xB4, 0xD4),
    accent_text: Color32::from_rgb(0x6C, 0xC8, 0xE2),
    accent_soft: Color32::from_rgb(0x12, 0x34, 0x40),
    danger: Color32::from_rgb(0xEF, 0x6F, 0x68),
    warning: Color32::from_rgb(0xE3, 0xAE, 0x4A),
    warning_soft: Color32::from_rgb(0x3A, 0x2C, 0x12),
    success: Color32::from_rgb(0x5C, 0xC4, 0x8A),
    shadow: Color32::from_rgba_premultiplied(0, 0, 0, 0x60),
};

impl Palette {
    pub fn of(visuals: &egui::Visuals) -> &'static Palette {
        if visuals.dark_mode { &DARK } else { &LIGHT }
    }
}

/// Corner radii.
pub const RADIUS_SM: u8 = 4;
pub const RADIUS_MD: u8 = 6;
pub const RADIUS_LG: u8 = 10;

/// Font families beyond egui's two: Inter at heavier weights.
pub fn medium() -> FontFamily {
    FontFamily::Name("medium".into())
}
pub fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

/// Phosphor icons.
pub fn icons() -> FontFamily {
    FontFamily::Name("icons".into())
}

/// An icon as text, in the icon font.
pub fn glyph(icon: &str) -> egui::RichText {
    egui::RichText::new(icon).family(icons())
}

/// Text in Inter Medium, for titles and emphasis.
pub fn strong(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text).family(medium())
}

/// A small uppercase section label: "FEEDS", "TODOS".
pub fn eyebrow(ui: &egui::Ui, text: &str) -> egui::RichText {
    egui::RichText::new(text.to_uppercase())
        .size(10.5)
        .family(semibold())
        .extra_letter_spacing(0.6)
        .color(Palette::of(ui.visuals()).faint)
}

/// Install fonts and both themes' styles once per context. Cheap to call
/// again. Safe before the first frame; egui switches fonts at the start of
/// the next frame (see [`ready`]).
pub fn install(ctx: &egui::Context) {
    let id = egui::Id::new("callboard_theme_installed");
    if ctx.data(|d| d.get_temp::<bool>(id)).unwrap_or(false) {
        return;
    }
    ctx.data_mut(|d| d.insert_temp(id, true));
    ctx.set_fonts(fonts());
    for (theme, palette) in [(Theme::Light, &LIGHT), (Theme::Dark, &DARK)] {
        ctx.style_mut_of(theme, |style| apply(style, palette));
    }
}

/// Whether the installed fonts are in use in this frame. Until they are, the
/// extra families do not exist, so a frame that installs them draws nothing.
/// Call only during a frame.
pub fn ready(ctx: &egui::Context) -> bool {
    let ready = ctx.fonts(|f| f.definitions().families.contains_key(&icons()));
    if !ready {
        ctx.request_repaint();
    }
    ready
}

fn fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    let add = |fonts: &mut FontDefinitions, name: &str, bytes: &'static [u8]| {
        fonts
            .font_data
            .insert(name.into(), Arc::new(FontData::from_static(bytes)));
    };
    add(
        &mut fonts,
        "inter",
        include_bytes!("../assets/fonts/Inter-Regular.ttf"),
    );
    add(
        &mut fonts,
        "inter-medium",
        include_bytes!("../assets/fonts/Inter-Medium.ttf"),
    );
    add(
        &mut fonts,
        "inter-semibold",
        include_bytes!("../assets/fonts/Inter-SemiBold.ttf"),
    );
    add(
        &mut fonts,
        "phosphor",
        include_bytes!("../assets/fonts/Phosphor.ttf"),
    );
    // egui's own fonts stay as fallbacks (symbols, emoji). Icons get a family
    // of their own: Inter has glyphs in the private-use range Phosphor uses,
    // and Phosphor maps a-z to blank glyphs, so neither may stand in for the other.
    let fallbacks = fonts.families[&FontFamily::Proportional].clone();
    let family = |primary: &str| {
        let mut keys = vec![primary.to_owned()];
        keys.extend(fallbacks.iter().cloned());
        keys
    };
    fonts
        .families
        .insert(FontFamily::Proportional, family("inter"));
    fonts.families.insert(medium(), family("inter-medium"));
    fonts.families.insert(semibold(), family("inter-semibold"));
    fonts.families.insert(icons(), vec!["phosphor".to_owned()]);
    fonts
}

fn apply(style: &mut egui::Style, p: &Palette) {
    use FontFamily::{Monospace, Proportional};
    style.text_styles = [
        (TextStyle::Small, FontId::new(11.5, Proportional)),
        (TextStyle::Body, FontId::new(13.5, Proportional)),
        (TextStyle::Button, FontId::new(13.5, Proportional)),
        (TextStyle::Monospace, FontId::new(12.5, Monospace)),
        (TextStyle::Heading, FontId::new(16.0, semibold())),
    ]
    .into();

    let spacing = &mut style.spacing;
    spacing.item_spacing = egui::vec2(8.0, 6.0);
    spacing.button_padding = egui::vec2(8.0, 3.0);
    spacing.interact_size.y = 22.0;
    spacing.menu_margin = Margin::same(6);
    spacing.window_margin = Margin::same(14);
    spacing.indent = 16.0;
    spacing.icon_width = 15.0;
    spacing.icon_width_inner = 9.0;
    spacing.scroll = egui::style::ScrollStyle::thin();
    spacing.scroll.bar_width = 6.0;

    let v = &mut style.visuals;
    v.override_text_color = None;
    v.weak_text_color = Some(p.muted);
    v.hyperlink_color = p.accent_text;
    v.faint_bg_color = p.raised;
    v.extreme_bg_color = p.raised;
    v.text_edit_bg_color = Some(p.raised);
    v.code_bg_color = p.raised;
    v.warn_fg_color = p.warning;
    v.error_fg_color = p.danger;
    v.panel_fill = p.surface;
    v.window_fill = p.surface;
    v.window_stroke = Stroke::new(1.0, p.hairline);
    v.window_corner_radius = CornerRadius::same(RADIUS_LG);
    v.menu_corner_radius = CornerRadius::same(RADIUS_MD + 2);
    v.window_shadow = Shadow {
        offset: [0, 10],
        blur: 28,
        spread: 0,
        color: p.shadow,
    };
    v.popup_shadow = Shadow {
        offset: [0, 6],
        blur: 16,
        spread: 0,
        color: p.shadow,
    };
    v.selection.bg_fill = p.accent_soft;
    v.selection.stroke = Stroke::new(1.5, p.accent);
    v.indent_has_left_vline = false;
    v.striped = false;
    v.collapsing_header_frame = false;
    v.interact_cursor = Some(egui::CursorIcon::PointingHand);

    let radius = CornerRadius::same(RADIUS_MD);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.surface;
    w.noninteractive.weak_bg_fill = p.surface;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.hairline);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    // Buttons are ghosts until hovered: no fill, no outline.
    w.inactive.weak_bg_fill = Color32::TRANSPARENT;
    w.inactive.bg_fill = p.raised;
    w.inactive.bg_stroke = Stroke::NONE;
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.hovered.weak_bg_fill = p.hover;
    w.hovered.bg_fill = p.hover;
    w.hovered.bg_stroke = Stroke::NONE;
    w.hovered.fg_stroke = Stroke::new(1.5, p.text);
    w.hovered.expansion = 0.0;
    w.active.weak_bg_fill = p.pressed;
    w.active.bg_fill = p.pressed;
    w.active.bg_stroke = Stroke::NONE;
    w.active.fg_stroke = Stroke::new(1.5, p.text);
    w.active.expansion = 0.0;
    w.open.weak_bg_fill = p.hover;
    w.open.bg_fill = p.hover;
    w.open.bg_stroke = Stroke::NONE;
    w.open.fg_stroke = Stroke::new(1.0, p.text);
    for state in [
        &mut w.noninteractive,
        &mut w.inactive,
        &mut w.hovered,
        &mut w.active,
        &mut w.open,
    ] {
        state.corner_radius = radius;
    }
}

/// A borderless icon button with an accessible name and a tooltip.
pub fn icon_button(ui: &mut egui::Ui, glyph: &str, label: &str, hint: &str) -> egui::Response {
    let muted = Palette::of(ui.visuals()).muted;
    let response = ui
        .add(
            egui::Button::new(self::glyph(glyph).size(15.0).color(muted))
                .min_size(egui::vec2(24.0, 22.0)),
        )
        .on_hover_text(hint);
    name(&response, label);
    response
}

/// Give a widget whose text is an icon its accessible name.
pub fn name(response: &egui::Response, label: &str) {
    let enabled = response.enabled();
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
}

/// A small rounded pill: "new", "updated", counts.
pub fn pill(ui: &mut egui::Ui, text: &str, fill: Color32, color: Color32) -> egui::Response {
    egui::Frame::new()
        .fill(fill)
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(6, 1))
        .show(ui, |ui| {
            // Not selectable, so a pill on a clickable row never takes its clicks.
            ui.add(
                egui::Label::new(
                    egui::RichText::new(text)
                        .size(11.0)
                        .family(medium())
                        .color(color),
                )
                .selectable(false),
            )
        })
        .response
}

/// A full-width list row (sidebar, quick open): highlighted when `selected`,
/// tinted on hover with a short fade, and clickable and draggable as a whole.
/// `contents` fills it left to right.
pub fn list_row(
    ui: &mut egui::Ui,
    selected: bool,
    contents: impl FnOnce(&mut egui::Ui),
) -> egui::Response {
    let p = Palette::of(ui.visuals());
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 28.0),
        egui::Sense::click_and_drag(),
    );
    let hover =
        ui.ctx()
            .animate_bool_with_time(response.id.with("hover"), response.hovered(), 0.12);
    let fill = if selected {
        p.accent_soft
    } else {
        Color32::TRANSPARENT.lerp_to_gamma(p.hover, hover)
    };
    ui.painter()
        .rect_filled(rect, CornerRadius::same(RADIUS_MD), fill);
    let mut row = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(egui::vec2(8.0, 0.0)))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    row.spacing_mut().item_spacing.x = 8.0;
    contents(&mut row);
    response
}

/// Text painted into a row and truncated to fit, without a widget of its
/// own: the row carries the accessible name, so the text must not repeat it.
pub fn row_text(ui: &mut egui::Ui, text: impl Into<egui::WidgetText>) {
    let galley = text.into().into_galley(
        ui,
        Some(egui::TextWrapMode::Truncate),
        ui.available_width(),
        TextStyle::Body,
    );
    let (rect, _) = ui.allocate_exact_size(galley.size(), egui::Sense::hover());
    let color = ui.visuals().text_color();
    ui.painter().galley(rect.min, galley, color);
}

/// A named item color as a saturated mark (accent bars, dots), the same in
/// both themes. `None` for a name this version does not know.
pub fn color_mark(color: &str) -> Option<Color32> {
    let [r, g, b] = match color {
        "red" => [0xE5, 0x48, 0x4D],
        "orange" => [0xF0, 0x7A, 0x2A],
        "yellow" => [0xE2, 0xB0, 0x1B],
        "green" => [0x30, 0xA4, 0x6C],
        "blue" => [0x3E, 0x8B, 0xDE],
        "purple" => [0x8E, 0x4E, 0xC6],
        "pink" => [0xD6, 0x40, 0x9F],
        "gray" => [0x8B, 0x92, 0xA0],
        _ => return None,
    };
    Some(Color32::from_rgb(r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_item_color_has_a_mark() {
        for (name, _) in crate::board::COLORS {
            assert!(color_mark(name).is_some(), "{name}");
        }
        assert!(color_mark("teal").is_none());
    }

    #[test]
    fn icons_and_text_never_share_a_family() {
        let fonts = fonts();
        for family in [FontFamily::Proportional, medium(), semibold()] {
            let keys = &fonts.families[&family];
            assert!(keys[0].starts_with("inter"), "{family:?}: {keys:?}");
            assert!(
                !keys.iter().any(|k| k == "phosphor"),
                "{family:?}: {keys:?}"
            );
        }
        assert_eq!(fonts.families[&icons()], ["phosphor"]);
    }
}
