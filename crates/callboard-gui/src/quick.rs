//! Quick open (Ctrl+K): find a feed, board, or layout by name and open it
//! (DESIGN.md §6.1).
use crate::{
    app::Action,
    backend::Target,
    theme::{self, Palette, icon},
    workspace::LayoutKey,
};
use eframe::egui;

pub const SHORTCUT: egui::KeyboardShortcut =
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::K);

/// At most this many matches are listed.
const SHOWN: usize = 12;

/// The palette's state while it is open.
#[derive(Debug, Default)]
pub struct QuickOpen {
    pub text: String,
    /// Index into the current matches.
    pub selected: usize,
}

/// Something quick open can open.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub kind: &'static str,
    pub title: String,
    /// Also matched, such as a feed's name beside its title.
    pub alias: Option<String>,
    /// "on canvas", "active", or nothing.
    pub note: Option<&'static str>,
    pub action: Action,
}

impl Choice {
    pub fn target(kind: &'static str, title: String, target: Target, placed: bool) -> Self {
        let alias = match &target {
            Target::Feed(name) if *name != title => Some(name.clone()),
            _ => None,
        };
        Self {
            kind,
            title,
            alias,
            note: placed.then_some("on canvas"),
            action: Action::Show(target),
        }
    }

    pub fn layout(key: LayoutKey, active: bool) -> Self {
        Self {
            kind: "Layout",
            title: key.label().to_owned(),
            alias: None,
            note: active.then_some("active"),
            action: Action::Switch(key),
        }
    }
}

/// How well `query` matches `text`, higher is better; `None` when it does
/// not. Case-insensitive: a prefix beats a word start, which beats a match
/// anywhere, which beats the query's characters in order with gaps.
pub fn score(query: &str, text: &str) -> Option<u32> {
    let query = query.trim().to_lowercase();
    let text = text.to_lowercase();
    if query.is_empty() {
        return Some(0);
    }
    if text.starts_with(&query) {
        return Some(4000);
    }
    if let Some(at) = text.find(&query) {
        let word_start = text[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        return Some(if word_start { 3000 } else { 2000 } - at.min(999) as u32);
    }
    // Characters in order: fewer skipped characters rank higher.
    let mut rest = text.chars();
    let mut skipped = 0u32;
    for wanted in query.chars() {
        loop {
            let c = rest.next()?;
            if c == wanted {
                break;
            }
            skipped += 1;
        }
    }
    Some(1000u32.saturating_sub(skipped))
}

/// The choices matching `query`, best first; ties keep the given order.
pub fn matches<'a>(query: &str, choices: &'a [Choice]) -> Vec<&'a Choice> {
    let mut found: Vec<(u32, &Choice)> = choices
        .iter()
        .filter_map(|choice| {
            let best = std::iter::once(&choice.title)
                .chain(&choice.alias)
                .filter_map(|text| score(query, text))
                .max()?;
            Some((best, choice))
        })
        .collect();
    found.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    found.into_iter().map(|(_, c)| c).take(SHOWN).collect()
}

/// Draw the palette. Returns whether it stays open; a chosen entry's action
/// is pushed onto `actions`.
pub fn show(
    ui: &mut egui::Ui,
    state: &mut QuickOpen,
    choices: &[Choice],
    actions: &mut Vec<Action>,
) -> bool {
    let found = matches(&state.text, choices);
    state.selected = state.selected.min(found.len().saturating_sub(1));
    let (up, down, enter, escape) = ui.input_mut(|i| {
        (
            i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
            i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
            i.key_pressed(egui::Key::Enter),
            i.consume_key(egui::Modifiers::NONE, egui::Key::Escape),
        )
    });
    if up {
        state.selected = state.selected.saturating_sub(1);
    }
    if down && state.selected + 1 < found.len() {
        state.selected += 1;
    }
    let mut chosen = enter.then_some(state.selected);
    let window = egui::Window::new("Quick open")
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .fixed_size(egui::vec2(520.0, 0.0))
        .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 90.0))
        .show(ui.ctx(), |ui| {
            let p = Palette::of(ui.visuals());
            let label = ui.label(theme::eyebrow(ui, "Open a feed, board, or layout"));
            label.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Label,
                    true,
                    "Open a feed, board, or layout",
                )
            });
            let field = ui
                .add(
                    egui::TextEdit::singleline(&mut state.text)
                        .hint_text("Type a name")
                        .font(egui::FontId::proportional(15.0))
                        .margin(egui::Margin::symmetric(10, 8))
                        .desired_width(f32::INFINITY),
                )
                .labelled_by(label.id);
            if field.changed() {
                state.selected = 0;
            }
            field.request_focus();
            ui.add_space(4.0);
            if found.is_empty() {
                ui.label(egui::RichText::new("No matches").color(p.faint));
            }
            ui.spacing_mut().item_spacing.y = 2.0;
            for (i, choice) in found.iter().enumerate() {
                let name = match choice.note {
                    Some(note) => format!("{}: {} ({note})", choice.kind, choice.title),
                    None => format!("{}: {}", choice.kind, choice.title),
                };
                let glyph = match choice.kind {
                    "Feed" => icon::RSS_SIMPLE,
                    "Board" => icon::KANBAN,
                    "Archive" => icon::ARCHIVE,
                    _ => icon::SQUARES_FOUR,
                };
                let response = theme::list_row(ui, i == state.selected, |ui| {
                    ui.label(theme::glyph(glyph).color(p.muted));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let kind = match choice.note {
                            Some(note) => format!("{} · {note}", choice.kind),
                            None => choice.kind.to_string(),
                        };
                        theme::row_text(ui, egui::RichText::new(kind).size(12.0).color(p.faint));
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            theme::row_text(ui, &choice.title);
                        });
                    });
                });
                theme::name(&response, &name);
                if response.clicked() {
                    chosen = Some(i);
                }
                if i == state.selected && (up || down) {
                    response.scroll_to_me(None);
                }
            }
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("↑ ↓ to select  ·  ↵ to open  ·  Esc to close")
                    .size(11.5)
                    .color(p.faint),
            );
        });
    if let Some(choice) = chosen.and_then(|i| found.get(i)) {
        actions.push(choice.action.clone());
        return false;
    }
    let outside = window.is_some_and(|w| {
        ui.input(|i| {
            i.pointer.any_pressed()
                && i.pointer
                    .interact_pos()
                    .is_some_and(|p| !w.response.rect.contains(p))
        })
    });
    !(escape || outside)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_beat_word_starts_beat_substrings_beat_scattered_letters() {
        let prefix = score("rev", "Reviews").unwrap();
        let word = score("rev", "Code reviews").unwrap();
        let inside = score("rev", "Unreviewed").unwrap();
        let scattered = score("rvw", "Reviews").unwrap();
        assert!(prefix > word && word > inside && inside > scattered);
        assert_eq!(score("xyz", "Reviews"), None);
        assert_eq!(score("  ", "Anything"), Some(0));
        assert!(score("rvw", "Reviews") > score("rvw", "Rather a long view"));
    }

    #[test]
    fn matches_rank_by_title_or_alias_and_keep_order_on_ties() {
        let feed = Choice::target(
            "Feed",
            "myrepo — review requests".into(),
            Target::Feed("reviews".into()),
            false,
        );
        let board = Choice::target("Board", "Inbox".into(), Target::Board(2), true);
        let layout = Choice::layout(LayoutKey::Saved("Review day".into()), false);
        let choices = [feed.clone(), board.clone(), layout.clone()];
        // The feed's name "reviews" is a prefix match, like the layout's.
        assert_eq!(matches("rev", &choices), [&feed, &layout]);
        assert_eq!(matches("inb", &choices), [&board]);
        assert_eq!(matches("", &choices).len(), 3);
        assert_eq!(board.note, Some("on canvas"));
    }
}
