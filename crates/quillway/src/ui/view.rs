//! Layout. The panel sits in an unbounded scrollable so the sensor reports its
//! natural height, which `App::on_resize` turns into the surface height.

use iced::keyboard::{Key, key::Named};
use iced::widget::{
    Space, column, container, mouse_area, rich_text, row, rule, scrollable, sensor, span, text, text::Span,
    text_editor, text_input,
};
use iced::{Element, Length, Padding, window};
use quillway_core::diff::{self, Change};
use quillway_engine::download::human;

use super::style::{Palette, RING};
use super::{App, EngineState, INPUT_ID, Message, Phase, Popup, SOURCE_ID};

const BODY_MAX_HEIGHT: f32 = 380.0;
const SOURCE_MAX_HEIGHT: f32 = 170.0;
const PAD_X: u16 = 18;

impl App {
    pub(super) fn view(&self, id: window::Id) -> Element<'_, Message> {
        let Some(p) = self.popup.as_ref().filter(|p| p.id == id) else {
            return Space::new().into();
        };
        let pal = self.palette.faded(self.fade());

        let mut sections: Vec<Element<'_, Message>> = vec![self.input_row(p, pal)];
        if let Some(body) = self.body(p, pal) {
            sections.push(hairline(pal));
            sections.push(body);
        }
        if matches!(p.phase(), Phase::Composing | Phase::Reviewing) && self.engine_state != EngineState::Missing {
            sections.push(hairline(pal));
            sections.push(self.chips(pal));
        }
        sections.push(hairline(pal));
        sections.push(self.footer(p, pal));

        let panel = container(column(sections)).width(Length::Fill).style(pal.panel());
        let ring = container(panel).padding(RING).width(self.config.ui.width as f32).style(pal.ring(self.shimmer()));
        let measured = sensor(ring).on_show(Message::Resized).on_resize(Message::Resized);

        container(
            scrollable(measured)
                .direction(scrollable::Direction::Vertical(scrollable::Scrollbar::hidden()))
                .height(Length::Fill),
        )
        .padding(self.margin() as u16)
        .into()
    }

    fn input_row<'a>(&'a self, p: &'a Popup, pal: Palette) -> Element<'a, Message> {
        let (placeholder, editable) = match p.phase() {
            Phase::Composing if self.engine_state == EngineState::Missing => ("No model installed", false),
            Phase::Composing => ("Describe your change…", true),
            Phase::Generating => ("Writing…", false),
            Phase::Reviewing => ("Refine: make it warmer…   (↵ on empty copies)", true),
            Phase::Copied => ("", false),
        };
        let mut input = text_input(placeholder, &p.input)
            .id(INPUT_ID)
            .size(18)
            .padding(Padding { top: 15.0, bottom: 15.0, left: PAD_X as f32, right: PAD_X as f32 })
            .style(pal.input());
        if editable {
            input = input.on_input(Message::Input).on_submit(Message::Submit);
        }
        if self.engine_state == EngineState::Missing {
            input = input.on_submit(Message::Submit);
        }
        input.into()
    }

    fn body<'a>(&'a self, p: &'a Popup, pal: Palette) -> Option<Element<'a, Message>> {
        if self.engine_state == EngineState::Missing {
            return Some(self.install_card(pal));
        }
        let content: Element<'a, Message> = match p.phase() {
            Phase::Composing => text_editor(&p.source)
                .id(SOURCE_ID)
                .placeholder("Type or paste the text to rewrite…")
                .on_action(Message::Edit)
                .size(14)
                .padding(0)
                .max_height(SOURCE_MAX_HEIGHT)
                .style(pal.editor())
                // Tab switches boxes instead of inserting a tab.
                .key_binding(|k| match k.key {
                    Key::Named(Named::Tab) => None,
                    _ => text_editor::Binding::from_key_press(k),
                })
                .into(),
            Phase::Generating => match self.streaming_text() {
                Some(t) => body_scroll(text(t).size(15).color(pal.text).into()),
                None => text("…").size(15).color(pal.faint).into(),
            },
            Phase::Reviewing | Phase::Copied => {
                let d = p.drafts.last().expect("reviewing has a draft");
                let color = if p.phase() == Phase::Copied { pal.dim } else { pal.text };
                if p.show_diff {
                    body_scroll(diff_view(&p.original, &d.text, pal))
                } else {
                    body_scroll(text(d.text.as_str()).size(15).color(color).into())
                }
            }
        };
        let mut col = column![content].spacing(8);
        if let Some(e) = &p.error {
            col = col.push(text(e.as_str()).size(13).color(pal.error));
        }
        if let EngineState::Failed(e) = &self.engine_state {
            col = col.push(text(format!("Model server failed: {}", preview(e, 200))).size(13).color(pal.error));
        }
        Some(container(col).padding([12, PAD_X]).width(Length::Fill).into())
    }

    fn install_card(&self, pal: Palette) -> Element<'_, Message> {
        let entry = quillway_core::catalog::find(&self.active.id).unwrap_or_else(quillway_core::catalog::default_entry);
        let line: Element<'_, Message> = match &self.install {
            Some(i) if i.error.is_some() => text(i.error.clone().unwrap_or_default()).size(13).color(pal.error).into(),
            Some(i) => text(format!(
                "Downloading {}… {} / {} ({}%)",
                i.entry.name,
                human(i.done),
                human(i.entry.size),
                i.done * 100 / i.entry.size.max(1)
            ))
            .size(14)
            .color(pal.dim)
            .into(),
            None => mouse_area(
                text(format!("↵  Install {} ({}, {})", entry.name, human(entry.size), entry.license))
                    .size(15)
                    .color(pal.text),
            )
            .on_press(Message::InstallStart)
            .into(),
        };
        container(column![line].spacing(6)).padding([14, PAD_X]).width(Length::Fill).into()
    }

    fn chips(&self, pal: Palette) -> Element<'_, Message> {
        let chips = self.presets.iter().enumerate().map(|(i, preset)| {
            let key = if i < 9 { format!("{}", i + 1) } else { String::new() };
            let label = row![text(key).size(11).color(pal.faint), text(preset.name.as_str()).size(13).color(pal.text)]
                .spacing(6);
            mouse_area(container(label).padding([5, 10]).style(pal.chip(false))).on_press(Message::Preset(i)).into()
        });
        container(row(chips).spacing(8).wrap().vertical_spacing(8)).padding([12, PAD_X]).width(Length::Fill).into()
    }

    fn footer<'a>(&'a self, p: &'a Popup, pal: Palette) -> Element<'a, Message> {
        let left = match (p.phase(), &self.engine_state) {
            (_, EngineState::Starting) => format!("{} · loading…", self.active.name),
            (Phase::Generating, _) => {
                format!("{} · {}", self.active.name, p.generation.as_ref().map(|g| g.label.as_str()).unwrap_or(""))
            }
            (Phase::Reviewing | Phase::Copied, _) => {
                let d = p.drafts.last().expect("reviewing has a draft");
                let n = p.drafts.len();
                let steps = if n > 1 { format!(" ({n})") } else { String::new() };
                format!("{}{} · {}", d.label, steps, d.stats)
            }
            (Phase::Composing, _) => {
                let n = p.source.text().chars().count();
                format!("{} · {} · {} chars", self.active.name, p.origin.label(), n)
            }
        };
        let hints = match p.phase() {
            _ if self.engine_state == EngineState::Missing => "↵ install   esc close",
            Phase::Composing => "↵ run   1–9 preset   ⇥ switch box   esc close",
            Phase::Generating => "esc stop",
            Phase::Reviewing => "↵ copy   ⇥ diff   ^R retry   ^Z undo   esc",
            Phase::Copied => "✓ Copied",
        };
        let hint_color = if p.phase() == Phase::Copied { pal.added } else { pal.faint };
        container(
            row![
                text(left).size(12).color(pal.dim),
                Space::new().width(Length::Fill),
                text(hints).size(12).color(hint_color)
            ]
            .spacing(12),
        )
        .padding([10, PAD_X])
        .width(Length::Fill)
        .into()
    }
}

fn hairline<'a>(pal: Palette) -> Element<'a, Message> {
    rule::horizontal(1).style(pal.hairline()).into()
}

fn body_scroll(content: Element<'_, Message>) -> Element<'_, Message> {
    container(
        scrollable(content).direction(scrollable::Direction::Vertical(
            scrollable::Scrollbar::new().width(4).scroller_width(4).margin(0),
        )),
    )
    .max_height(BODY_MAX_HEIGHT)
    .into()
}

fn diff_view<'a>(old: &str, new: &str, pal: Palette) -> Element<'a, Message> {
    let diff = diff::word_diff(old, new);
    let spans: Vec<Span<'a, ()>> = diff
        .iter()
        .enumerate()
        .map(|(i, s)| match s.change {
            Change::Same => span(s.text.clone()).color(pal.text),
            Change::Added => span(s.text.clone()).color(pal.added).background(pal.added_bg),
            Change::Removed => {
                // Keep "~~old~~ new" apart when a replacement follows directly.
                let replaced = diff.get(i + 1).is_some_and(|n| n.change == Change::Added);
                let mut t = s.text.clone();
                if replaced && !t.ends_with(char::is_whitespace) {
                    t.push(' ');
                }
                span(t).color(pal.removed).strikethrough(true)
            }
        })
        .collect();
    rich_text(spans).size(15).into()
}

/// First `max` characters on one line.
fn preview(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        let mut t: String = flat.chars().take(max).collect();
        t.push('…');
        t
    }
}
