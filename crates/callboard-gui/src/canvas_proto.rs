//! Phase 0 prototype (docs/canvas-plan.md): overlapping, movable, resizable,
//! collapsible cards on a pannable canvas. Throwaway; not used by the app.
//! Run on the desktop with `cargo run -p callboard-gui --example canvas_proto`.
use eframe::egui::{self, Pos2, Rect, Vec2};

pub const TITLE_HEIGHT: f32 = 26.0;
pub const MIN_SIZE: Vec2 = Vec2::new(220.0, 120.0);
/// Width of the invisible resize strips along the right and bottom edges.
const GRIP: f32 = 6.0;

pub struct Card {
    pub title: String,
    /// Canvas units. `rect.height()` is the expanded height, kept while collapsed.
    pub rect: Rect,
    pub collapsed: bool,
    pub rows: usize,
    /// Last scroll offset of the card's contents, for tests.
    pub scroll: Vec2,
}

impl Card {
    fn shown(&self) -> Rect {
        if self.collapsed {
            Rect::from_min_size(self.rect.min, Vec2::new(self.rect.width(), TITLE_HEIGHT))
        } else {
            self.rect
        }
    }
}

pub struct Proto {
    /// The canvas point shown at the canvas area's top-left corner.
    pub view: Pos2,
    /// Back to front: the last card is drawn on top.
    pub cards: Vec<Card>,
    /// Clicks on the sidebar button, to prove cards never cover the sidebar.
    pub sidebar_clicks: usize,
}

impl Default for Proto {
    fn default() -> Self {
        let card = |title: &str, x: f32, y: f32, w: f32, h: f32, rows| Card {
            title: title.into(),
            rect: Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, h)),
            collapsed: false,
            rows,
            scroll: Vec2::ZERO,
        };
        Self {
            view: Pos2::ZERO,
            cards: vec![
                card("reviews", 20.0, 20.0, 420.0, 400.0, 200),
                card("alerts", 300.0, 160.0, 360.0, 300.0, 12),
                card("Inbox", 700.0, 40.0, 300.0, 240.0, 5),
            ],
            sidebar_clicks: 0,
        }
    }
}

impl Proto {
    pub fn show(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("proto_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong("Canvas prototype");
                ui.label(format!("view {:.0},{:.0}", self.view.x, self.view.y));
                let order: Vec<&str> = self.cards.iter().map(|c| c.title.as_str()).collect();
                ui.label(format!("back→front: {}", order.join(", ")));
            });
        });
        egui::Panel::left("proto_sidebar")
            .default_size(200.0)
            .show(ui, |ui| {
                ui.heading("Sidebar");
                if ui.button("Sidebar button").clicked() {
                    self.sidebar_clicks += 1;
                }
                ui.label(format!("clicked {}", self.sidebar_clicks));
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(ui.visuals().extreme_bg_color))
            .show(ui, |ui| self.canvas(ui));
    }

    fn to_screen(&self, canvas: Rect, rect: Rect) -> Rect {
        rect.translate(canvas.min - self.view)
    }

    /// The topmost card whose shown rect contains `pos` (screen coordinates).
    fn topmost_at(&self, canvas: Rect, pos: Pos2) -> Option<usize> {
        if !canvas.contains(pos) {
            return None;
        }
        (0..self.cards.len())
            .rev()
            .find(|&i| self.to_screen(canvas, self.cards[i].shown()).contains(pos))
    }

    fn canvas(&mut self, ui: &mut egui::Ui) {
        let canvas = ui.max_rect();

        // A press anywhere on a card raises it, including on its buttons.
        // Read presses from the events: a quick click can press and release
        // within one frame, and then `press_origin()` is already cleared.
        let presses: Vec<Pos2> = ui.input(|i| {
            i.events
                .iter()
                .filter_map(|event| match event {
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        ..
                    } => Some(*pos),
                    _ => None,
                })
                .collect()
        });
        for pos in presses {
            if let Some(i) = self.topmost_at(canvas, pos) {
                let card = self.cards.remove(i);
                self.cards.push(card);
            }
        }
        // After raising: only this card may scroll or take the wheel.
        let pointer = ui.input(|i| i.pointer.hover_pos());
        let under = pointer.and_then(|p| self.topmost_at(canvas, p));

        // Registered first, so every card widget sits above it.
        let background = ui.interact(
            canvas,
            ui.id().with("canvas_background"),
            egui::Sense::click_and_drag(),
        );
        if background.dragged() {
            self.view -= background.drag_delta();
        }

        let count = self.cards.len();
        for i in 0..count {
            let screen = self.to_screen(canvas, self.cards[i].shown());
            if !screen.intersects(canvas) {
                continue;
            }
            let topmost_here = under == Some(i);
            self.card(ui, canvas, i, screen, topmost_here);
        }

        // Wheel over empty canvas pans; over a card, its scroll area used it.
        if under.is_none() && pointer.is_some_and(|p| canvas.contains(p)) {
            let delta = ui.input_mut(|i| std::mem::take(&mut i.smooth_scroll_delta));
            self.view -= delta;
        }
    }

    fn card(&mut self, ui: &mut egui::Ui, canvas: Rect, i: usize, screen: Rect, topmost: bool) {
        let card = &mut self.cards[i];
        let id = ui.id().with(("card", &card.title));
        let clip = screen.intersect(canvas);
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(screen).id_salt(id));
        child.set_clip_rect(clip);

        // Blocks widgets of lower cards underneath this one.
        child.interact(screen, id.with("blocker"), egui::Sense::click());

        let visuals = child.visuals().clone();
        child.painter().rect(
            screen,
            6.0,
            visuals.window_fill,
            visuals.window_stroke,
            egui::StrokeKind::Inside,
        );

        // Title bar: drag to move.
        let title_rect = Rect::from_min_size(screen.min, Vec2::new(screen.width(), TITLE_HEIGHT));
        let title = child.interact(title_rect, id.with("title"), egui::Sense::drag());
        if title.dragged() {
            card.rect = card.rect.translate(title.drag_delta());
        }
        let mut title_ui = child.new_child(
            egui::UiBuilder::new()
                .max_rect(title_rect.shrink2(Vec2::new(8.0, 3.0)))
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        title_ui.set_clip_rect(clip);
        title_ui.strong(&card.title);
        title_ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .small_button(if card.collapsed { "Expand" } else { "Collapse" })
                .clicked()
            {
                card.collapsed = !card.collapsed;
            }
        });

        if !card.collapsed {
            let body = Rect::from_min_max(
                Pos2::new(screen.min.x + 8.0, screen.min.y + TITLE_HEIGHT + 4.0),
                screen.max - Vec2::splat(8.0),
            );
            let mut body_ui = child.new_child(egui::UiBuilder::new().max_rect(body));
            body_ui.set_clip_rect(body.intersect(canvas));
            let output = egui::ScrollArea::vertical()
                .id_salt(id.with("scroll"))
                .auto_shrink([false, false])
                // Zero: a covered card neither scrolls nor consumes the wheel.
                .wheel_scroll_multiplier(Vec2::splat(if topmost { 1.0 } else { 0.0 }))
                .show(&mut body_ui, |ui| {
                    for row in 0..card.rows {
                        ui.label(format!("{} item {row}", card.title));
                    }
                });
            card.scroll = output.state.offset;

            // Resize from the right edge, bottom edge, and corner.
            let grips = [
                (
                    "right",
                    Rect::from_min_max(
                        Pos2::new(screen.max.x - GRIP, screen.min.y + TITLE_HEIGHT),
                        screen.max,
                    ),
                    Vec2::new(1.0, 0.0),
                    egui::CursorIcon::ResizeHorizontal,
                ),
                (
                    "bottom",
                    Rect::from_min_max(Pos2::new(screen.min.x, screen.max.y - GRIP), screen.max),
                    Vec2::new(0.0, 1.0),
                    egui::CursorIcon::ResizeVertical,
                ),
                (
                    "corner",
                    Rect::from_min_max(screen.max - Vec2::splat(GRIP * 2.0), screen.max),
                    Vec2::new(1.0, 1.0),
                    egui::CursorIcon::ResizeNwSe,
                ),
            ];
            for (name, rect, axes, cursor) in grips {
                let grip = child
                    .interact(rect, id.with(name), egui::Sense::drag())
                    .on_hover_cursor(cursor);
                if grip.dragged() {
                    let size = (card.rect.size() + grip.drag_delta() * axes).max(MIN_SIZE);
                    card.rect = Rect::from_min_size(card.rect.min, size);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::{Harness, kittest::Queryable};

    fn harness() -> Harness<'static, Proto> {
        Harness::builder()
            .with_size([1200.0, 700.0])
            .build_ui_state(|ui, proto: &mut Proto| proto.show(ui), Proto::default())
    }

    /// Screen rect of a card by title, as last laid out.
    fn screen(h: &Harness<'static, Proto>, title: &str) -> Rect {
        let node = h.get_by_label(title);
        let title_rect = node.rect();
        let card = h.state().cards.iter().find(|c| c.title == title).unwrap();
        Rect::from_min_size(
            Pos2::new(
                title_rect.min.x - 8.0,
                title_rect.center().y - TITLE_HEIGHT / 2.0,
            ),
            card.shown().size(),
        )
    }

    fn click(h: &mut Harness<'static, Proto>, pos: Pos2) {
        h.hover_at(pos);
        h.step();
        h.drag_at(pos);
        h.step();
        h.event(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        });
        h.step();
    }

    fn drag(h: &mut Harness<'static, Proto>, from: Pos2, to: Pos2) {
        h.hover_at(from);
        h.step();
        h.drag_at(from);
        h.step();
        for step in 1..=5 {
            h.hover_at(from + (to - from) * (step as f32 / 5.0));
            h.step();
        }
        h.drop_at(to);
        h.step();
    }

    fn wheel(h: &mut Harness<'static, Proto>, at: Pos2, delta: Vec2) {
        h.hover_at(at);
        h.step();
        h.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta,
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::NONE,
        });
        // Smooth scrolling spreads the delta over several frames.
        for _ in 0..30 {
            h.step();
        }
    }

    fn order(h: &Harness<'static, Proto>) -> Vec<String> {
        h.state().cards.iter().map(|c| c.title.clone()).collect()
    }

    #[test]
    fn a_press_raises_the_topmost_card_under_the_pointer_only() {
        let mut h = harness();
        h.step();
        let reviews = screen(&h, "reviews");
        let alerts = screen(&h, "alerts");
        let overlap = reviews.intersect(alerts).center();
        // In the overlap, alerts is on top: pressing there raises alerts, not reviews.
        click(&mut h, overlap);
        assert_eq!(order(&h), ["reviews", "Inbox", "alerts"]);
        // A visible part of reviews raises it above alerts.
        click(&mut h, reviews.min + Vec2::new(30.0, 60.0));
        assert_eq!(order(&h), ["Inbox", "alerts", "reviews"]);
    }

    #[test]
    fn a_click_pressed_and_released_within_one_frame_still_raises() {
        let mut h = harness();
        h.step();
        let pos = screen(&h, "reviews").min + Vec2::new(30.0, 60.0);
        h.hover_at(pos);
        h.step();
        for pressed in [true, false] {
            h.event(egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            });
        }
        h.step();
        assert_eq!(order(&h), ["alerts", "Inbox", "reviews"]);
    }

    #[test]
    fn title_drags_move_and_grip_drags_resize_with_a_minimum() {
        let mut h = harness();
        h.step();
        let before = h.state().cards[2].rect;
        let inbox = screen(&h, "Inbox");
        drag(
            &mut h,
            inbox.min + Vec2::new(150.0, 12.0),
            inbox.min + Vec2::new(90.0, 112.0),
        );
        let moved = h
            .state()
            .cards
            .iter()
            .find(|c| c.title == "Inbox")
            .unwrap()
            .rect;
        assert_eq!(moved.min, before.min + Vec2::new(-60.0, 100.0));
        assert_eq!(moved.size(), before.size());
        let inbox = screen(&h, "Inbox");
        let corner = inbox.max - Vec2::splat(4.0);
        drag(&mut h, corner, corner - Vec2::splat(500.0));
        let resized = h
            .state()
            .cards
            .iter()
            .find(|c| c.title == "Inbox")
            .unwrap()
            .rect;
        assert_eq!(resized.size(), MIN_SIZE);
        assert_eq!(resized.min, moved.min);
    }

    #[test]
    fn collapse_keeps_the_expanded_height() {
        let mut h = harness();
        h.step();
        // Every card has one; the last drawn belongs to the front card, Inbox.
        h.query_all_by_label("Collapse").last().unwrap().click();
        h.step();
        h.step();
        let card = h
            .state()
            .cards
            .iter()
            .find(|c| c.collapsed)
            .expect("one collapsed");
        assert_eq!(card.shown().height(), TITLE_HEIGHT);
        let height = card.rect.height();
        h.get_by_label("Expand").click();
        h.step();
        h.step();
        assert!(h.state().cards.iter().all(|c| !c.collapsed));
        assert!(h.state().cards.iter().any(|c| c.rect.height() == height));
    }

    #[test]
    fn empty_canvas_drags_and_wheels_pan_while_a_card_wheel_scrolls_it() {
        let mut h = harness();
        h.step();
        // Empty canvas: below the cards.
        let empty = Pos2::new(900.0, 600.0);
        drag(&mut h, empty, empty + Vec2::new(-100.0, -50.0));
        assert_eq!(h.state().view, Pos2::new(100.0, 50.0));
        wheel(&mut h, empty, Vec2::new(0.0, -40.0));
        let view = h.state().view;
        assert!((view.y - 90.0).abs() < 1.0, "wheel pans: {view:?}");
        // Over the long reviews list the wheel scrolls it and leaves the view.
        let reviews = screen(&h, "reviews");
        wheel(&mut h, reviews.center(), Vec2::new(0.0, -120.0));
        assert!((h.state().view.y - view.y).abs() < 0.5, "view unchanged");
        let scroll = h
            .state()
            .cards
            .iter()
            .find(|c| c.title == "reviews")
            .unwrap()
            .scroll;
        assert!(scroll.y > 50.0, "reviews scrolled: {scroll:?}");
    }

    #[test]
    fn a_covered_card_does_not_scroll_under_the_top_card() {
        let mut h = harness();
        h.step();
        let reviews = screen(&h, "reviews");
        let alerts = screen(&h, "alerts");
        // Pointer in the overlap, over alerts (12 rows: nothing to scroll).
        wheel(
            &mut h,
            reviews.intersect(alerts).center(),
            Vec2::new(0.0, -120.0),
        );
        let scroll = h
            .state()
            .cards
            .iter()
            .find(|c| c.title == "reviews")
            .unwrap()
            .scroll;
        assert_eq!(scroll.y, 0.0, "reviews is covered there");
    }

    #[test]
    fn cards_panned_under_the_sidebar_never_take_its_clicks() {
        let mut h = harness();
        h.step();
        // Pan so reviews extends left, under the sidebar.
        h.state_mut().view = Pos2::new(150.0, 0.0);
        h.step();
        h.get_by_label("Sidebar button").click();
        h.step();
        h.step();
        assert_eq!(h.state().sidebar_clicks, 1);
        assert_eq!(order(&h), ["reviews", "alerts", "Inbox"], "no card raised");
    }
}
