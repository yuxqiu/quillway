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

use super::no_ctrl_typing::no_ctrl_typing;
use super::notice;
use super::style::{Palette, RING};
use super::{App, EngineState, Field, INPUT_ID, Message, Phase, Popup, SOURCE_ID};

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
        sections.push(hairline(pal));
        sections.push(self.body(p, pal));
        if matches!(p.phase(), Phase::Composing | Phase::Reviewing) && !self.needs_install() {
            sections.push(hairline(pal));
            sections.push(self.chips(pal));
        }
        sections.push(hairline(pal));
        sections.push(self.footer(p, pal));
        // Last, so nothing above moves when it comes or goes.
        if let Some(n) = self.notice(p) {
            sections.push(hairline(pal));
            sections.push(notice::tray(n, p.details_open, pal, PAD_X));
        }

        let panel = container(column(sections)).width(Length::Fill).style(pal.panel());
        let width = f32::from(u16::try_from(self.config.ui.width).unwrap_or(u16::MAX));
        let ring = container(panel).padding(RING).width(width).style(pal.ring(self.shimmer()));
        let measured = sensor(ring)
            .on_show(move |size| Message::Resized(id, size))
            .on_resize(move |size| Message::Resized(id, size));

        container(
            scrollable(measured)
                .direction(scrollable::Direction::Vertical(scrollable::Scrollbar::hidden()))
                .height(Length::Fill),
        )
        .padding(f32::from(u16::try_from(self.margin()).unwrap_or(0)))
        .into()
    }

    fn input_row<'a>(&'a self, p: &'a Popup, pal: Palette) -> Element<'a, Message> {
        let (placeholder, editable) = match p.phase() {
            Phase::Composing if self.needs_install() => ("No model installed", false),
            Phase::Composing => ("Describe your change…", true),
            Phase::Generating => ("Writing…", false),
            Phase::Reviewing => ("Refine: make it warmer…   (↵ on empty copies)", true),
        };
        let mut input = text_input(placeholder, &p.input)
            .id(INPUT_ID)
            .size(18)
            .padding(Padding { top: 15.0, bottom: 15.0, left: f32::from(PAD_X), right: f32::from(PAD_X) })
            .style(pal.input());
        // ↵ is a shortcut (`Shortcut::Enter`): a text box without `on_input` drops it.
        if editable {
            input = input.on_input(Message::Input);
        }
        // Ctrl+1 runs a preset; it mustn't also type "1" here.
        no_ctrl_typing(input)
    }

    fn body<'a>(&'a self, p: &'a Popup, pal: Palette) -> Element<'a, Message> {
        if self.needs_install() {
            return container(self.install_card(pal)).padding([12, PAD_X]).width(Length::Fill).into();
        }
        let content: Element<'a, Message> = match p.phase() {
            Phase::Composing => editor(&p.source, "Type or paste the text to rewrite…", SOURCE_MAX_HEIGHT, pal),
            // No diff while editing: the edits go into the text itself.
            Phase::Reviewing if p.editing() => editor(&p.draft_editor, "", BODY_MAX_HEIGHT, pal),
            Phase::Generating => self.streaming_text().map_or_else(
                || text("…").size(15).color(pal.faint).into(),
                |t| body_scroll(text(t).size(15).color(pal.text).into()),
            ),
            Phase::Reviewing => {
                let d = p.drafts.last().expect("reviewing has a draft");
                if p.show_diff() {
                    body_scroll(diff_view(p.original(), &d.text, pal))
                } else {
                    body_scroll(text(d.text.as_str()).size(15).color(pal.text).into())
                }
            }
        };
        container(content).padding([12, PAD_X]).width(Length::Fill).into()
    }

    /// The download's progress, or the offer to install (again, after a failure,
    /// whose message is in the tray).
    fn install_card(&self, pal: Palette) -> Element<'_, Message> {
        let Some(entry) = self.active.entry else {
            // A missing custom file: the tray names it; nothing here can fix it.
            let hint = "Point `model.active` at an existing file, or pick a model with `quillway models use`.";
            return text(hint).size(13).color(pal.dim).into();
        };
        if let Some(i) = self.installing(entry).filter(|i| i.error.is_none()) {
            let progress = format!(
                "Downloading {}… {} / {} ({}%){}",
                i.entry.name,
                human(i.done),
                human(i.entry.size),
                i.done * 100 / i.entry.size.max(1),
                i.rate.describe().map_or_else(String::new, |r| format!(" · {r}"))
            );
            return text(progress).size(14).color(pal.dim).into();
        }
        // A non-OSI license is shown before ↵, which then accepts it (DECISIONS #9).
        let size = human(entry.size);
        let license = entry.license_warning();
        let action = if license.is_some() {
            format!("↵  Accept the license and install {} ({size})", entry.name)
        } else {
            format!("↵  Install {} ({size}, {})", entry.name, entry.license)
        };
        let mut card = column![].spacing(6);
        if let Some(w) = license {
            card = card.push(text(w).size(13).color(pal.dim));
        }
        card.push(mouse_area(text(action).size(15).color(pal.text)).on_press(Message::InstallStart)).into()
    }

    fn chips(&self, pal: Palette) -> Element<'_, Message> {
        let chips = self.config.presets().iter().enumerate().map(|(i, preset)| {
            let key = if i < 9 { format!("^{}", i + 1) } else { String::new() };
            let label = row![text(key).size(11).color(pal.faint), text(preset.name.as_str()).size(13).color(pal.text)]
                .spacing(6);
            mouse_area(container(label).padding([5, 10]).style(pal.chip())).on_press(Message::Preset(i)).into()
        });
        container(row(chips).spacing(8).wrap().vertical_spacing(8)).padding([12, PAD_X]).width(Length::Fill).into()
    }

    fn footer<'a>(&'a self, p: &'a Popup, pal: Palette) -> Element<'a, Message> {
        let loading = self.engine_state == EngineState::Starting;
        let mut left = match p.phase() {
            Phase::Composing if loading => format!("{} · loading…", self.model_label()),
            Phase::Composing => {
                let n = p.source.text().chars().count();
                format!("{} · {} · {} chars", self.model_label(), p.origin.label(), n)
            }
            Phase::Generating => {
                format!("{} · {}", self.model_label(), p.generation.as_ref().map_or("", |g| g.label.as_str()))
            }
            Phase::Reviewing => {
                let d = p.drafts.last().expect("reviewing has a draft");
                let n = p.drafts.len();
                let steps = if n > 1 { format!(" ({n})") } else { String::new() };
                let incomplete = if d.incomplete { " (incomplete)" } else { "" };
                format!("{}{incomplete}{} · {}", d.label, steps, d.stats)
            }
        };
        // While generating or reviewing, the draft's details stay; the model loading is a note.
        if loading && p.phase() != Phase::Composing {
            left.push_str(" · model loading…");
        }
        let hints = match p.phase() {
            // Downloading: ↵ has nothing to do until it finishes or fails.
            _ if self.needs_install()
                && self.active.entry.and_then(|e| self.installing(e)).is_some_and(|i| i.error.is_none()) =>
            {
                "esc close"
            }
            _ if self.needs_install() && self.active.entry.is_some() => "↵ install   esc close",
            // A missing custom model file: nothing to install, run or switch to.
            _ if self.needs_install() => "esc close",
            // In the text box, ↵ is a new line and the Ctrl shortcuts are off.
            Phase::Composing if p.field == Field::Source => "⇥ switch box   esc close",
            Phase::Composing => "↵ run   ^1–9 preset   ⇥ switch box   esc close",
            Phase::Generating => "esc stop",
            Phase::Reviewing if p.editing() => "⇥ done editing   esc close",
            Phase::Reviewing if p.drafts.last().is_some_and(super::Draft::edited) => {
                "↵ copy   ⇥ edit   ^D diff   ^Z undo   esc"
            }
            Phase::Reviewing => "↵ copy   ⇥ edit   ^D diff   ^R retry   ^Z undo   esc",
        };
        container(
            row![
                text(left).size(12).color(pal.dim),
                Space::new().width(Length::Fill),
                text(hints).size(12).color(pal.faint)
            ]
            .spacing(12),
        )
        .padding([10, PAD_X])
        .width(Length::Fill)
        .into()
    }
}

/// The text box: the source while composing, the latest draft while editing it.
fn editor<'a>(
    content: &'a text_editor::Content,
    placeholder: &'a str,
    max_height: f32,
    pal: Palette,
) -> Element<'a, Message> {
    text_editor(content)
        .id(SOURCE_ID)
        .placeholder(placeholder)
        .on_action(Message::Edit)
        .size(14)
        .padding(0)
        .max_height(max_height)
        .style(pal.editor())
        // Tab switches boxes instead of inserting a tab. Keys pressed with Ctrl insert nothing:
        // iced still reports the key's text under Ctrl, so Ctrl+1 would otherwise insert "1".
        .key_binding(|k| match k.key {
            Key::Named(Named::Tab) => None,
            _ if k.modifiers.control() => {
                text_editor::Binding::from_key_press(k).filter(|b| !matches!(b, text_editor::Binding::Insert(_)))
            }
            _ => text_editor::Binding::from_key_press(k),
        })
        .into()
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
