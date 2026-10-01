//! M0 spike 1: a Raycast-style popup on a wlr-layer-shell overlay surface.
//! Checks: renders a transparent rounded panel, grabs the keyboard, accepts
//! typing (incl. IME), Esc closes. Throwaway code.

use iced::widget::{column, container, row, rule, text, text_input};
use iced::{Border, Color, Element, Event, Length, Shadow, Task, Vector, event, keyboard};
use iced_layershell::application;
use iced_layershell::reexport::{Anchor, KeyboardInteractivity, Layer};
use iced_layershell::settings::{LayerShellSettings, Settings, StartMode};
use iced_layershell::to_layer_message;

const INPUT_ID: &str = "instruction";
const PANEL_W: u32 = 680;
/// Transparent room for a client-drawn shadow. `QW_MARGIN=0` makes the surface
/// exactly the panel, so a compositor rule can round, blur and shadow it.
/// Panel opacity; lower it (e.g. `QW_ALPHA=0.7`) when the compositor blurs behind us.
fn alpha() -> f32 {
    std::env::var("QW_ALPHA").ok().and_then(|v| v.parse().ok()).unwrap_or(0.94)
}

fn margin() -> u32 {
    std::env::var("QW_MARGIN").ok().and_then(|v| v.parse().ok()).unwrap_or(32)
}

const PRESETS: [&str; 7] = [
    "Proofread", "Rewrite", "Friendly", "Professional", "Concise", "Summary", "Key points",
];

pub fn main() -> Result<(), iced_layershell::Error> {
    application(boot, namespace, update, view)
        .style(|_, _| iced::theme::Style {
            background_color: Color::TRANSPARENT,
            text_color: Color::WHITE,
        })
        .subscription(|_| event::listen().map(Message::Iced))
        .settings(Settings {
            layer_settings: LayerShellSettings {
                anchor: Anchor::Top,
                layer: Layer::Overlay,
                exclusive_zone: -1,
                size: Some((PANEL_W + 2 * margin(), 211 + 2 * margin())),
                margin: (220, 0, 0, 0),
                keyboard_interactivity: KeyboardInteractivity::Exclusive,
                start_mode: StartMode::Active,
                ..Default::default()
            },
            antialiasing: true,
            ..Default::default()
        })
        .run()
}

fn namespace() -> String {
    "quillway".into()
}

#[derive(Default)]
struct State {
    input: String,
    last: Option<String>,
}

#[to_layer_message]
#[derive(Debug, Clone)]
enum Message {
    Input(String),
    Submit,
    Iced(Event),
}

fn boot() -> (State, Task<Message>) {
    (State::default(), iced::widget::operation::focus(INPUT_ID))
}

fn update(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::Input(s) => state.input = s,
        Message::Submit => {
            println!("submit: {:?}", state.input);
            state.last = Some(std::mem::take(&mut state.input));
        }
        Message::Iced(Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Escape),
            ..
        })) => return iced::exit(),
        _ => {}
    }
    Task::none()
}

fn view(state: &State) -> Element<'_, Message> {
    let dim = Color::from_rgba(1.0, 1.0, 1.0, 0.55);

    let input = text_input("Describe your change…", &state.input)
        .id(INPUT_ID)
        .on_input(Message::Input)
        .on_submit(Message::Submit)
        .size(18)
        .padding([14, 18])
        .style(|_, _| text_input::Style {
            background: Color::TRANSPARENT.into(),
            border: Border::default(),
            icon: Color::WHITE,
            placeholder: Color::from_rgba(1.0, 1.0, 1.0, 0.4),
            value: Color::WHITE,
            selection: Color::from_rgba(0.49, 0.42, 0.95, 0.5),
        });

    let captured = text(
        state
            .last
            .as_deref()
            .map(|s| format!("last submit: {s}"))
            .unwrap_or_else(|| "“their going to the park tomorow, weather permitting…”".into()),
    )
    .size(14)
    .color(dim);

    let chips = row(PRESETS.iter().enumerate().map(|(i, name)| chip(i + 1, name)))
        .spacing(8)
        .wrap();

    let footer = row![
        text("Qwen3.5 4B").size(12).color(dim),
        iced::widget::space::horizontal(),
        text("↵ run   1–9 preset   esc close").size(12).color(dim),
    ];

    let hairline = || rule::horizontal(1).style(|_| rule::Style {
        color: Color::from_rgba(1.0, 1.0, 1.0, 0.08),
        radius: 0.0.into(),
        fill_mode: rule::FillMode::Full,
        snap: true,
    });

    let panel = container(
        column![
            input,
            hairline(),
            container(captured).padding([10, 18]),
            hairline(),
            container(chips).padding([12, 18]),
            hairline(),
            container(footer).padding([10, 18]),
        ],
    )
    .width(PANEL_W)
    .height(Length::Fill)
    .style(|_| container::Style {
        background: Some(Color::from_rgba8(28, 28, 32, alpha()).into()),
        border: Border {
            color: Color::from_rgba(1.0, 1.0, 1.0, 0.08),
            width: 1.0,
            radius: 16.0.into(),
        },
        shadow: Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, 0.45),
            offset: Vector::new(0.0, 12.0),
            blur_radius: 28.0,
        },
        ..Default::default()
    });

    container(panel)
        .padding(margin() as u16)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn chip<'a>(n: usize, name: &'a str) -> Element<'a, Message> {
    container(
        row![
            text(n.to_string()).size(11).color(Color::from_rgba(1.0, 1.0, 1.0, 0.45)),
            text(name).size(13),
        ]
        .spacing(6),
    )
    .padding([5, 10])
    .style(|_| container::Style {
        background: Some(Color::from_rgba(1.0, 1.0, 1.0, 0.06).into()),
        border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 8.0.into() },
        ..Default::default()
    })
    .into()
}
