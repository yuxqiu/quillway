//! The daemon: an `iced_layershell` program with no surface until `show`.
//!
//! One popup at a time. Its life: take the input text (recent clipboard, text
//! from the CLI, or typed into the popup) → compose → generate (streamed) →
//! review / refine / edit → copy → hide.

mod style;
mod view;

use std::os::unix::net::UnixListener;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, Stream, StreamExt, stream};
use iced::keyboard::{self, Key, key::Code, key::Named, key::Physical};
use iced::widget::text_editor;
use iced::{Event, Size, Subscription, Task, event, task, window};
use iced_layershell::reexport::{Anchor, KeyboardInteractivity, Layer, NewLayerShellSettings, OutputOption};
use iced_layershell::settings::{LayerShellSettings, Settings, StartMode};
use iced_layershell::to_layer_message;
use quillway_core::catalog::Entry;
use quillway_core::config::{Config, DEFAULT_TEMPERATURE};
use quillway_core::ipc::{Input, Request, Response};
use quillway_engine::download;
use quillway_engine::{Active, Chunk, Engine, EngineState, Rewrite, Timing, models};

use crate::ipc::{self, Reply};

const INPUT_ID: &str = "quillway-input";
const SOURCE_ID: &str = "quillway-source";
const MAX_SURFACE_HEIGHT: u32 = 760;
/// Panel height assumed when a popup opens, until the sensor reports the real one.
const INITIAL_PANEL_HEIGHT: u32 = 196;
/// Animation frame interval (fade-in, shimmer).
const FRAME: Duration = Duration::from_millis(16);
/// One turn of the shimmer ring while generating.
const SHIMMER_PERIOD: Duration = Duration::from_millis(2400);
const FADE_IN: Duration = Duration::from_millis(140);
const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(2);

/// Bound before the UI starts; taken once by the IPC subscription.
static LISTENER: Mutex<Option<UnixListener>> = Mutex::new(None);

pub fn run() -> anyhow::Result<()> {
    *LISTENER.lock().expect("listener lock") = Some(ipc::bind()?);
    let config = Config::load_user()?;
    let active = models::active(&config)?;
    let watch = start_clipboard_watch();
    // `Font::with_name` needs a `'static` name; this runs once per process.
    let default_font =
        config.ui.font.as_ref().map_or_else(iced::Font::default, |f| iced::Font::with_name(f.clone().leak()));
    iced_layershell::daemon(
        move || App::boot(config.clone(), active.clone(), watch.clone()),
        namespace,
        App::update,
        App::view,
    )
    .subscription(App::subscription)
    .style(|app: &App, _| iced::theme::Style {
        background_color: iced::Color::TRANSPARENT,
        text_color: app.palette.text,
    })
    .settings(Settings {
        layer_settings: LayerShellSettings { start_mode: StartMode::Background, ..Default::default() },
        default_font,
        antialiasing: true,
        ..Default::default()
    })
    .run()
    .map_err(|e| anyhow::anyhow!("{e}"))
}

fn start_clipboard_watch() -> Option<quillway_wl::ClipboardWatch> {
    quillway_wl::ClipboardWatch::start()
        .inspect_err(|e| eprintln!("quillway: {e:#}; the clipboard will always count as recent"))
        .ok()
}

fn namespace() -> String {
    "quillway".into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    Clipboard,
    /// Piped to `quillway show|toggle --stdin`.
    Stdin,
    /// Nothing recent to start from: the user types or pastes the text.
    Typed,
}

impl Origin {
    const fn label(self) -> &'static str {
        match self {
            Self::Clipboard => "clipboard",
            Self::Stdin => "stdin",
            Self::Typed => "typed",
        }
    }
}

/// Which box has the keyboard: the instruction, or the text (the source while
/// composing, the latest draft while reviewing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Instruction,
    Source,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Composing,
    Generating,
    Reviewing,
}

struct Draft {
    text: String,
    label: String,
    stats: String,
    show_diff: bool,
    /// Generation failed part-way; this is what arrived.
    incomplete: bool,
    /// What produced it; `None` for a hand edit, which has nothing to retry.
    request: Option<Rewrite>,
}

impl Draft {
    const fn edited(&self) -> bool {
        self.request.is_none()
    }
}

struct Generation {
    id: u64,
    handle: task::Handle,
    raw: String,
    label: String,
    show_diff: bool,
    /// A retry: replaces the latest draft when it finishes (kept if cancelled).
    retry: bool,
    request: Rewrite,
    /// The instruction box's text, if this was started from it; given back if it fails without a draft.
    typed: Option<String>,
    timing: Timing,
}

impl Generation {
    fn into_draft(self, text: String, stats: String, incomplete: bool) -> Draft {
        Draft { text, label: self.label, stats, show_diff: self.show_diff, incomplete, request: Some(self.request) }
    }
}

struct Popup {
    id: window::Id,
    /// The text to rewrite, editable while there are no drafts.
    source: text_editor::Content,
    origin: Origin,
    field: Field,
    input: String,
    drafts: Vec<Draft>,
    /// The latest draft, while it is being edited by hand.
    draft_editor: text_editor::Content,
    generation: Option<Generation>,
    /// Set by Ctrl+D; until then each draft shows its preset's default.
    diff_choice: Option<bool>,
    error: Option<String>,
    /// The keyboard was given to `field`'s box, on the first resize.
    focus_given: bool,
    size: (u32, u32),
    opened: Instant,
}

impl Popup {
    const fn phase(&self) -> Phase {
        if self.generation.is_some() {
            Phase::Generating
        } else if self.drafts.is_empty() {
            Phase::Composing
        } else {
            Phase::Reviewing
        }
    }

    /// Text the next preset/refinement applies to: the latest draft, else the input.
    fn base(&self) -> String {
        self.drafts.last().map_or_else(|| self.source.text(), |d| d.text.clone())
    }

    /// The latest draft has the keyboard (Tab while reviewing).
    fn editing(&self) -> bool {
        self.phase() == Phase::Reviewing && self.field == Field::Source
    }

    /// Hand edits go into an "Edited" draft, so Ctrl+Z restores the model's text.
    fn keep_edit(&mut self) {
        self.error = None; // it was about the text before this edit
        let text = self.draft_editor.text();
        let Some(last) = self.drafts.last_mut() else { return };
        if last.edited() {
            last.text = text;
            // Edited back to the text before it: drop the step, so Ctrl+Z isn't a no-op.
            if let [.., before, edited] = self.drafts.as_slice()
                && before.text == edited.text
            {
                self.drafts.pop();
            }
            return;
        }
        if last.text == text {
            return;
        }
        let draft = Draft {
            text,
            label: "Edited".into(),
            stats: "by hand".into(),
            show_diff: last.show_diff,
            incomplete: false,
            request: None,
        };
        self.drafts.push(draft);
    }

    /// End `g` with `error`: keep what arrived as an incomplete draft (unless a
    /// retry failed: its old draft stays), else give back the typed instruction.
    fn fail(&mut self, g: Generation, error: String) {
        let partial = g.raw.trim().to_owned();
        if !partial.is_empty() && !g.retry {
            self.drafts.push(g.into_draft(partial, "stopped early".into(), true));
        } else if let Some(typed) = g.typed {
            self.input = typed;
        }
        self.error = Some(error);
    }

    /// Whether the latest draft is shown as a word diff: Ctrl+D's choice, else its preset's default.
    fn show_diff(&self) -> bool {
        self.diff_choice.or_else(|| self.drafts.last().map(|d| d.show_diff)).unwrap_or(false)
    }

    /// The diff base: the text the first draft was made from.
    fn original(&self) -> &str {
        self.drafts.first().and_then(|d| d.request.as_ref()).map_or("", |r| r.text.as_str())
    }
}

struct Install {
    entry: &'static Entry,
    done: u64,
    rate: download::Rate,
    error: Option<String>,
}

impl Install {
    fn new(entry: &'static Entry) -> Self {
        Self { entry, done: 0, rate: download::Rate::default(), error: None }
    }
}

struct App {
    config: Config,
    engine: Engine,
    engine_state: EngineState,
    active: Active,
    palette: style::Palette,
    popup: Option<Popup>,
    install: Option<Install>,
    now: Instant,
    next_gen: u64,
    /// `None` if the compositor can't report clipboard changes.
    watch: Option<quillway_wl::ClipboardWatch>,
    /// The current clipboard read; older results are ignored after cancellation.
    capturing: Option<u64>,
    next_capture: u64,
    /// The key of the last shortcut, until it is released. layershellev delivers auto-repeat
    /// as fresh presses (`repeat: false`), so a press of the held key is a repeat.
    held: Option<Physical>,
}

#[derive(Debug, Clone, Copy)]
enum Shortcut {
    Escape,
    Tab,
    Diff,
    Retry,
    Undo,
    Preset(usize),
}

#[derive(Debug, Clone)]
enum GenEvent {
    Chunk(Chunk),
    Error(String),
    Done,
}

#[derive(Debug, Clone)]
enum InstallEvent {
    Progress(u64),
    Done(Result<(), String>),
}

#[to_layer_message(multi)]
#[derive(Debug, Clone)]
enum Message {
    Ipc(Request, Reply),
    /// A clipboard read: its text (`None` if empty), or why it failed.
    Captured(u64, Result<Option<String>, String>),
    Input(String),
    Edit(text_editor::Action),
    Submit,
    Preset(usize),
    Gen(u64, GenEvent),
    Shortcut(Shortcut, Physical),
    KeyReleased(Physical),
    /// A mouse press, which may have moved the keyboard focus to the other box.
    Clicked(window::Id),
    /// The box that has the keyboard after a click; `None` if the click unfocused both.
    Focused(window::Id, Option<Field>),
    Resized(window::Id, Size),
    Tick(Instant),
    /// The engine's state changed (from its supervisor).
    EngineState(EngineState),
    InstallStart,
    Install(&'static Entry, InstallEvent),
    WindowClosed(window::Id),
}

impl App {
    fn boot(config: Config, active: Active, watch: Option<quillway_wl::ClipboardWatch>) -> (Self, Task<Message>) {
        let (engine, supervisor) = Engine::new(config.model.clone(), active.clone());
        let app = Self {
            active,
            palette: style::Palette::new(&config.ui),
            engine_state: EngineState::Starting,
            config,
            engine,
            popup: None,
            install: None,
            now: Instant::now(),
            next_gen: 0,
            watch,
            capturing: None,
            next_capture: 0,
            held: None,
        };
        // The supervisor brings the model server up as soon as it runs.
        (app, Task::future(supervisor.run()).discard())
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subs = vec![
            Subscription::run(ipc_stream).map(|(req, reply)| Message::Ipc(req, reply)),
            Subscription::run_with(self.engine.clone(), engine_states),
            event::listen_with(shortcut),
            event::listen_with(|event, _, id| {
                matches!(event, Event::Mouse(iced::mouse::Event::ButtonPressed(_))).then_some(Message::Clicked(id))
            }),
            window::close_events().map(Message::WindowClosed),
        ];
        if self.animating() {
            subs.push(iced::time::every(FRAME).map(Message::Tick));
        }
        Subscription::batch(subs)
    }

    fn animating(&self) -> bool {
        self.popup.as_ref().is_some_and(|p| p.generation.is_some() || p.opened.elapsed() < FADE_IN)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Ipc(req, reply) => self.ipc(req, &reply),
            Message::Captured(id, result) => self.captured(id, result),
            Message::Input(s) => self.on_input(s),
            Message::Edit(action) => self.on_edit(action),
            Message::Submit => self.submit(),
            Message::Preset(i) => self.run_preset(i),
            Message::Gen(id, ev) => self.on_gen(id, ev),
            // A held key acts once: a held Esc would stop the generation, then close the popup.
            Message::Shortcut(_, key) if self.held == Some(key) => Task::none(),
            Message::Shortcut(s, key) => {
                self.held = Some(key);
                self.on_shortcut(s)
            }
            Message::KeyReleased(key) => {
                if self.held == Some(key) {
                    self.held = None;
                }
                Task::none()
            }
            Message::Clicked(id) => self.on_click(id),
            Message::Focused(id, field) => self.on_focused(id, field),
            Message::Resized(id, size) => self.on_resize(id, size),
            Message::Tick(now) => {
                self.now = now;
                Task::none()
            }
            Message::EngineState(state) => self.on_engine_state(state),
            Message::InstallStart => self.start_install(),
            Message::Install(entry, ev) => self.on_install(entry, ev),
            Message::WindowClosed(id) => {
                if self.popup.as_ref().is_some_and(|p| p.id == id) {
                    self.abort_generation();
                    self.popup = None;
                }
                Task::none()
            }
            // Layer-shell actions are handled by iced_layershell before reaching us.
            _ => Task::none(),
        }
    }

    fn on_edit(&mut self, action: text_editor::Action) -> Task<Message> {
        let Some(p) = self.popup.as_mut() else { return Task::none() };
        if p.phase() == Phase::Composing {
            p.source.perform(action);
        } else if p.editing() {
            let edit = action.is_edit();
            p.draft_editor.perform(action);
            if edit {
                p.keep_edit();
            }
        }
        Task::none()
    }

    /// A click may have moved the keyboard to the other box: ask which box has it.
    fn on_click(&self, id: window::Id) -> Task<Message> {
        if !matches!(
            self.popup.as_ref().filter(|p| p.id == id).map(Popup::phase),
            Some(Phase::Composing | Phase::Reviewing)
        ) {
            return Task::none();
        }
        iced::widget::operation::is_focused(INPUT_ID).then(move |input| {
            if input {
                Task::done(Message::Focused(id, Some(Field::Instruction)))
            } else {
                iced::widget::operation::is_focused(SOURCE_ID)
                    .map(move |source| Message::Focused(id, source.then_some(Field::Source)))
            }
        })
    }

    /// Keep `field` in step with clicks, so Tab and the shortcuts act on the focused box;
    /// a click on neither box gives the keyboard back to `field`'s.
    fn on_focused(&mut self, id: window::Id, field: Option<Field>) -> Task<Message> {
        let Some(p) = self.popup.as_mut().filter(|p| p.id == id && p.generation.is_none()) else {
            return Task::none();
        };
        match field {
            Some(f) => {
                p.field = f;
                Task::none()
            }
            None => focus(p.field),
        }
    }

    fn on_engine_state(&mut self, state: EngineState) -> Task<Message> {
        self.engine_state = state;
        // Switched to a missing model while typing in the text box: the install card has none.
        match self.popup.as_mut() {
            Some(p) if self.engine_state == EngineState::Missing && p.phase() == Phase::Composing => {
                p.field = Field::Instruction;
                focus(Field::Instruction)
            }
            _ => Task::none(),
        }
    }

    fn ipc(&mut self, req: Request, reply: &Reply) -> Task<Message> {
        match req {
            Request::Toggle { input: Input::Text(_) } | Request::Show { input: Input::Text(_) }
                if self.popup.is_some() =>
            {
                // Replacing the open popup could discard a draft; don't drop the text silently either.
                reply.send(Response::Error {
                    message: "the popup is already open; close it (Esc) and send again".into(),
                });
                Task::none()
            }
            Request::Toggle { .. } if self.popup.is_some() => {
                reply.send(Response::Ok);
                self.hide()
            }
            Request::Show { .. } if self.popup.is_some() => {
                reply.send(Response::Ok);
                Task::none()
            }
            Request::Toggle { input: Input::Clipboard } if self.capturing.is_some() => {
                // Pressed again while the clipboard is still being read: cancel.
                reply.send(Response::Ok);
                self.capturing = None;
                Task::none()
            }
            Request::Toggle { input } | Request::Show { input } => {
                reply.send(Response::Ok);
                match input {
                    // Text sent explicitly wins over a clipboard read still in flight.
                    Input::Text(t) => {
                        self.capturing = None;
                        self.open(&t, Origin::Stdin, None)
                    }
                    Input::Clipboard if self.capturing.is_some() => Task::none(),
                    Input::Clipboard if self.clipboard_recent() => {
                        let id = self.next_capture;
                        self.next_capture += 1;
                        self.capturing = Some(id);
                        Task::perform(read_clipboard(), move |result| Message::Captured(id, result))
                    }
                    Input::Clipboard => self.open("", Origin::Typed, None),
                }
            }
            Request::Hide => {
                reply.send(Response::Ok);
                self.capturing = None;
                self.hide()
            }
            Request::Reload => match Config::load_user().and_then(|config| Ok((models::active(&config)?, config))) {
                Ok((active, config)) => {
                    self.active = active;
                    self.palette = style::Palette::new(&config.ui);
                    self.config = config.clone();
                    // The server this generation streams from is about to stop: say so, not "incomplete response".
                    let refocus = if let Some(p) = self.popup.as_mut()
                        && let Some(g) = p.generation.take()
                    {
                        g.handle.abort();
                        p.fail(g, "Stopped: the model server is restarting for a reload. Run it again.".into());
                        focus(p.field) // a click while writing may have taken it
                    } else {
                        Task::none()
                    };
                    // Sent now, so reloads apply in the order they arrived; the reply waits
                    // until the new server is up, so a model that fails to start is reported.
                    let done = self.engine.reload(config.model, self.active.clone());
                    let reply = reply.clone();
                    let answer = Task::future(async move {
                        reply.send(done.await.map_or_else(|message| Response::Error { message }, |()| Response::Ok));
                    });
                    Task::batch([answer.discard(), refocus])
                }
                Err(e) => {
                    reply.send(Response::Error { message: format!("{e:#}") });
                    Task::none()
                }
            },
            Request::Status => {
                // From the engine itself: the app's copy arrives as a message, maybe after this request.
                let engine = self.engine.state().borrow().describe();
                reply.send(Response::Status { visible: self.popup.is_some(), model: self.model_label(), engine });
                Task::none()
            }
            Request::Connect => {
                let (engine, reply) = (self.engine.clone(), reply.clone());
                Task::future(async move {
                    reply.send(match engine.client().await {
                        Ok(c) => Response::Server(c.endpoint().clone()),
                        Err(e) => Response::Error { message: format!("{e:#}") },
                    });
                })
                .discard()
            }
            Request::Quit => {
                reply.send(Response::Ok);
                iced::exit()
            }
        }
    }

    const fn margin(&self) -> u32 {
        if self.config.ui.client_shadow { style::SHADOW_MARGIN } else { 0 }
    }

    fn surface_size(&self, panel_height: u32) -> (u32, u32) {
        let m = self.margin();
        (self.config.ui.width + 2 * m, (panel_height + 2 * m).min(MAX_SURFACE_HEIGHT))
    }

    /// Copied within `behavior.recent_secs`. Without a watcher we can't tell, so yes.
    fn clipboard_recent(&mut self) -> bool {
        if self.watch.as_ref().is_some_and(|w| !w.is_alive()) {
            // It stopped (e.g. the compositor ended it): start another. Copies made
            // meanwhile went unseen, so this time the clipboard counts as recent.
            self.watch = start_clipboard_watch();
            return true;
        }
        let window = Duration::from_secs(self.config.behavior.recent_secs);
        self.watch.as_ref().is_none_or(|w| w.last_change().is_some_and(|t| t.elapsed() < window))
    }

    fn captured(&mut self, id: u64, result: Result<Option<String>, String>) -> Task<Message> {
        if self.capturing != Some(id) || self.popup.is_some() {
            return Task::none(); // cancelled by a second toggle
        }
        self.capturing = None;
        match result {
            Ok(Some(text)) => self.open(&text, Origin::Clipboard, None),
            Ok(None) => self.open("", Origin::Typed, None),
            Err(e) => self.open("", Origin::Typed, Some(e)),
        }
    }

    fn open(&mut self, text: &str, origin: Origin, error: Option<String>) -> Task<Message> {
        // The model may have been installed since we last looked (copied in, or by a CLI
        // that couldn't reach us); start it instead of offering the install card.
        if self.engine_state == EngineState::Missing && self.active.is_installed() {
            self.engine.start();
        }
        // A key held as the last popup closed was released elsewhere.
        self.held = None;
        let id = window::Id::unique();
        let size = self.surface_size(INITIAL_PANEL_HEIGHT);
        let top = i32::try_from(self.config.ui.top_margin.saturating_sub(self.margin())).unwrap_or(i32::MAX);
        self.now = Instant::now();
        self.popup = Some(Popup {
            id,
            // The install card has no text box; ↵ there goes to the instruction box.
            field: if text.trim().is_empty() && !self.model_missing() { Field::Source } else { Field::Instruction },
            source: editor_content(text),
            origin,
            input: String::new(),
            drafts: Vec::new(),
            draft_editor: text_editor::Content::new(),
            generation: None,
            diff_choice: None,
            error,
            focus_given: false,
            size,
            opened: Instant::now(),
        });
        Task::done(Message::NewLayerShell {
            settings: NewLayerShellSettings {
                size: Some(size),
                layer: Layer::Overlay,
                anchor: Anchor::Top,
                exclusive_zone: None,
                margin: Some((top, 0, 0, 0)),
                keyboard_interactivity: KeyboardInteractivity::Exclusive,
                output_option: OutputOption::Active,
                events_transparent: false,
                namespace: Some(namespace()),
            },
            id,
        })
    }

    fn hide(&mut self) -> Task<Message> {
        self.abort_generation();
        match self.popup.take() {
            Some(p) => window::close(p.id),
            None => Task::none(),
        }
    }

    fn abort_generation(&mut self) {
        if let Some(g) = self.popup.as_mut().and_then(|p| p.generation.take()) {
            g.handle.abort();
        }
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "layout heights are small and positive"
    )]
    fn on_resize(&mut self, id: window::Id, panel: Size) -> Task<Message> {
        let mut tasks = Vec::new();
        let want = self.surface_size(panel.height.ceil() as u32);
        let Some(p) = self.popup.as_mut().filter(|p| p.id == id) else { return Task::none() };
        if !p.focus_given {
            p.focus_given = true;
            tasks.push(focus(p.field));
        }
        if p.size != want {
            p.size = want;
            tasks.push(Task::done(Message::SizeChange { id: p.id, size: want }));
        }
        Task::batch(tasks)
    }

    fn on_input(&mut self, s: String) -> Task<Message> {
        let Some(p) = self.popup.as_mut() else { return Task::none() };
        if p.generation.is_some() {
            return Task::none();
        }
        p.input = s;
        Task::none()
    }

    fn submit(&mut self) -> Task<Message> {
        if self.needs_install() {
            return self.start_install();
        }
        let Some(p) = self.popup.as_ref() else { return Task::none() };
        let instruction = p.input.trim().to_owned();
        match (p.phase(), instruction.is_empty()) {
            (Phase::Reviewing, true) => self.copy(),
            (Phase::Composing, false) => self.generate("Custom".into(), instruction, DEFAULT_TEMPERATURE, false, true),
            (Phase::Reviewing, false) => self.generate("Refine".into(), instruction, DEFAULT_TEMPERATURE, false, true),
            (Phase::Composing, true) | (Phase::Generating, _) => Task::none(),
        }
    }

    fn run_preset(&mut self, i: usize) -> Task<Message> {
        let Some(preset) = self.config.presets().get(i).cloned() else { return Task::none() };
        if self.needs_install() || self.popup.as_ref().is_none_or(|p| p.generation.is_some()) {
            return Task::none();
        }
        self.generate(preset.name, preset.instruction, preset.temperature, preset.show_diff, false)
    }

    /// `from_input`: the instruction is the instruction box's text, which is cleared while it runs.
    fn generate(
        &mut self,
        label: String,
        instruction: String,
        temperature: f32,
        show_diff: bool,
        from_input: bool,
    ) -> Task<Message> {
        let Some(p) = self.popup.as_mut() else { return Task::none() };
        let text = p.base();
        if text.trim().is_empty() {
            p.error = Some(if p.drafts.is_empty() {
                "Nothing to rewrite: type or paste the text into the box (Tab switches boxes).".into()
            } else {
                "Nothing to rewrite: the text is empty. Ctrl+Z restores the previous draft.".into()
            });
            return Task::none();
        }
        if !from_input {
            // A preset clears the instruction box. That also drops the digit a Ctrl+digit key
            // typed there: iced's `text_input` inserts it even with Ctrl held.
            p.input.clear();
        }
        // The client counts tokens and rejects text too long for the context.
        let request = Rewrite { instruction, text, temperature };
        self.start(label, request, show_diff, false, from_input)
    }

    fn start(
        &mut self,
        label: String,
        request: Rewrite,
        show_diff: bool,
        retry: bool,
        from_input: bool,
    ) -> Task<Message> {
        let id = self.next_gen;
        self.next_gen += 1;
        let Some(p) = self.popup.as_mut() else { return Task::none() };
        let (task, handle) =
            Task::run(stream_rewrite(self.engine.clone(), request.clone()), move |ev| Message::Gen(id, ev)).abortable();
        p.error = None;
        // The new draft is reviewed from the instruction box, even if this started while editing.
        p.field = Field::Instruction;
        p.generation = Some(Generation {
            id,
            handle,
            raw: String::new(),
            label,
            show_diff,
            retry,
            request,
            typed: from_input.then(|| std::mem::take(&mut p.input)),
            timing: Timing::start(),
        });
        Task::batch([task, focus(Field::Instruction)])
    }

    fn on_gen(&mut self, id: u64, ev: GenEvent) -> Task<Message> {
        let Some(p) = self.popup.as_mut() else { return Task::none() };
        let Some(g) = p.generation.as_mut().filter(|g| g.id == id) else { return Task::none() };
        match ev {
            GenEvent::Chunk(chunk) => {
                g.timing.record(&chunk);
                if let Chunk::Text(t) = chunk {
                    g.raw.push_str(&t);
                }
            }
            GenEvent::Error(e) => {
                let g = p.generation.take().expect("matched above");
                p.fail(g, e);
            }
            GenEvent::Done => {
                let g = p.generation.take().expect("matched above");
                let text = g.raw.trim().to_owned();
                if text.trim().is_empty() {
                    if let Some(typed) = g.typed {
                        p.input = typed;
                    }
                    p.error = Some("The model returned nothing. Run it again, or try another preset.".into());
                } else {
                    let secs = g.timing.started().elapsed().as_secs_f64();
                    let rate = g.timing.rate();
                    if g.retry {
                        p.drafts.pop();
                    }
                    p.drafts.push(g.into_draft(text, format!("{secs:.1}s · {rate:.0} tok/s"), false));
                }
            }
        }
        // A click while writing may have taken the keyboard from the disabled box: give it back.
        if p.generation.is_none() { focus(p.field) } else { Task::none() }
    }

    fn on_shortcut(&mut self, s: Shortcut) -> Task<Message> {
        let installing = self.needs_install();
        let Some(p) = self.popup.as_mut() else { return Task::none() };
        match (s, p.phase()) {
            (Shortcut::Escape, Phase::Generating) => {
                self.abort_generation();
                // iced's text_input also took the Esc and dropped its focus.
                focus(Field::Instruction)
            }
            (Shortcut::Escape, _) => self.hide(),
            // The install card has no text box to move to; ↵ must keep working.
            (Shortcut::Tab, _) if installing => Task::none(),
            (Shortcut::Tab, Phase::Composing | Phase::Reviewing) => {
                p.field = match p.field {
                    Field::Instruction => Field::Source,
                    Field::Source => Field::Instruction,
                };
                if p.editing() {
                    p.draft_editor = editor_content(&p.base());
                }
                focus(p.field)
            }
            // The Ctrl shortcuts work from the instruction box; the text box keeps its keys.
            _ if p.field == Field::Source => Task::none(),
            (Shortcut::Preset(i), Phase::Composing | Phase::Reviewing) => self.run_preset(i),
            (Shortcut::Diff, Phase::Reviewing) => {
                p.diff_choice = Some(!p.show_diff());
                Task::none()
            }
            (Shortcut::Retry, Phase::Reviewing) => {
                let Some(d) = p.drafts.last() else { return Task::none() };
                let Some(request) = d.request.clone() else { return Task::none() };
                let (label, show_diff) = (d.label.clone(), d.show_diff);
                self.start(label, request, show_diff, true, false)
            }
            (Shortcut::Undo, Phase::Reviewing) => {
                p.drafts.pop();
                p.error = None;
                Task::none()
            }
            _ => Task::none(),
        }
    }

    fn copy(&mut self) -> Task<Message> {
        let Some(p) = self.popup.as_mut() else { return Task::none() };
        let Some(d) = p.drafts.last() else { return Task::none() };
        // Only a hand edit can be empty; copying it would just clear the clipboard.
        if d.text.trim().is_empty() {
            p.error = Some("Nothing to copy: the text is empty. Ctrl+Z restores the previous draft.".into());
            return Task::none();
        }
        match quillway_wl::copy(&d.text) {
            // Closing at once is the confirmation; the user is waiting to paste.
            Ok(()) => self.hide(),
            Err(e) => {
                p.error = Some(format!("{e:#}"));
                Task::none()
            }
        }
    }

    fn start_install(&mut self) -> Task<Message> {
        // A license notice is shown on the install card, so ↵ there is an informed yes (DECISIONS #9).
        let Some(entry) = self.active.entry else { return Task::none() };
        if self.installing(entry).is_some_and(|i| i.error.is_none()) {
            return Task::none();
        }
        // A download of a model a reload switched away from finishes on its own; its events are ignored.
        self.install = Some(Install::new(entry));
        Task::run(install_stream(entry), move |ev| Message::Install(entry, ev))
    }

    /// The install of `entry`, if that's the one the card tracks.
    fn installing(&self, entry: &Entry) -> Option<&Install> {
        self.install.as_ref().filter(|i| i.entry.id == entry.id)
    }

    fn on_install(&mut self, entry: &'static Entry, ev: InstallEvent) -> Task<Message> {
        let Some(i) = self.install.as_mut().filter(|i| i.entry.id == entry.id) else { return Task::none() };
        match ev {
            InstallEvent::Progress(done) => {
                i.done = done;
                i.rate.update(done, i.entry.size);
                Task::none()
            }
            InstallEvent::Done(Ok(())) => {
                self.install = None;
                self.engine.start();
                Task::none()
            }
            InstallEvent::Done(Err(e)) => {
                i.error = Some(e);
                Task::none()
            }
        }
    }

    /// The install card replaces the composer only before anything was generated.
    fn needs_install(&self) -> bool {
        self.model_missing() && self.popup.as_ref().is_some_and(|p| p.phase() == Phase::Composing)
    }

    /// The engine said the model is missing, and it still is: after an install or a
    /// reload to an installed model, the card goes before the engine's next state arrives.
    fn model_missing(&self) -> bool {
        self.engine_state == EngineState::Missing && !self.active.is_installed()
    }

    /// Name shown in the footer and `status`.
    fn model_label(&self) -> String {
        // The config requires `endpoint_model` with `endpoint`.
        let endpoint_model = self.config.model.endpoint.as_ref().and(self.config.model.endpoint_model.as_ref());
        endpoint_model.unwrap_or(&self.active.name).clone()
    }

    fn fade(&self) -> f32 {
        let Some(p) = &self.popup else { return 1.0 };
        let t = self.now.saturating_duration_since(p.opened).as_secs_f32() / FADE_IN.as_secs_f32();
        let t = t.clamp(0.0, 1.0);
        1.0 - (1.0 - t).powi(3)
    }

    /// Rotation of the shimmer ring, in turns, while generating.
    fn shimmer(&self) -> Option<f32> {
        let g = self.popup.as_ref()?.generation.as_ref()?;
        let elapsed = self.now.saturating_duration_since(g.timing.started());
        Some((elapsed.as_secs_f32() / SHIMMER_PERIOD.as_secs_f32()).fract())
    }

    fn streaming_text(&self) -> Option<&str> {
        Some(self.popup.as_ref()?.generation.as_ref()?.raw.trim_start()).filter(|t| !t.is_empty())
    }
}

fn shortcut(event: Event, _status: event::Status, _window: window::Id) -> Option<Message> {
    let (key, physical_key, modifiers) = match event {
        Event::Keyboard(keyboard::Event::KeyPressed { key, physical_key, modifiers, .. }) => {
            (key, physical_key, modifiers)
        }
        Event::Keyboard(keyboard::Event::KeyReleased { physical_key, .. }) => {
            return Some(Message::KeyReleased(physical_key));
        }
        _ => return None,
    };
    let s = match key.as_ref() {
        Key::Named(Named::Escape) => Shortcut::Escape,
        Key::Named(Named::Tab) => Shortcut::Tab,
        _ if !modifiers.control() => return None,
        // The character first, so remapped and virtual keyboards behave as they type.
        _ => match key.to_latin(physical_key) {
            Some('d') => Shortcut::Diff,
            Some('r') => Shortcut::Retry,
            Some('z') => Shortcut::Undo,
            Some(c @ '1'..='9') => Shortcut::Preset(c as usize - '1' as usize),
            Some(c) if c.is_ascii_alphanumeric() => return None,
            // Symbols on the digit row (AZERTY's `&é"'(…`): go by the key's position.
            _ => Shortcut::Preset(preset_index(physical_key)?),
        },
    };
    Some(Message::Shortcut(s, physical_key))
}

/// Ctrl+1–9 picks presets 0–8.
const fn preset_index(key: Physical) -> Option<usize> {
    let Physical::Code(code) = key else { return None };
    Some(match code {
        Code::Digit1 => 0,
        Code::Digit2 => 1,
        Code::Digit3 => 2,
        Code::Digit4 => 3,
        Code::Digit5 => 4,
        Code::Digit6 => 5,
        Code::Digit7 => 6,
        Code::Digit8 => 7,
        Code::Digit9 => 8,
        _ => return None,
    })
}

/// The engine's state now, then each change.
/// The engine's state now, then each change; if its supervisor stops, `Failed` once.
fn engine_states(engine: &Engine) -> impl Stream<Item = Message> + use<> {
    stream::unfold((Some(engine.state()), true), |(state, first)| async move {
        let mut state = state?;
        if !first && state.changed().await.is_err() {
            return Some((Message::EngineState(EngineState::Failed("the engine stopped".into())), (None, false)));
        }
        let now = state.borrow_and_update().clone();
        Some((Message::EngineState(now), (Some(state), false)))
    })
}

fn ipc_stream() -> impl Stream<Item = (Request, Reply)> {
    let listener = LISTENER.lock().expect("listener lock").take().expect("IPC subscription started once");
    ipc::serve(listener)
}

/// With the cursor at the end, ready to add to the text.
fn editor_content(text: &str) -> text_editor::Content {
    let mut content = text_editor::Content::with_text(text);
    content.perform(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
    content
}

fn focus(field: Field) -> Task<Message> {
    iced::widget::operation::focus(match field {
        Field::Instruction => INPUT_ID,
        Field::Source => SOURCE_ID,
    })
}

/// The clipboard text (`None` if empty), or why it couldn't be read.
async fn read_clipboard() -> Result<Option<String>, String> {
    // The app that owns the clipboard sends the data; a hung one must not block the popup.
    match tokio::time::timeout(CLIPBOARD_TIMEOUT, tokio::task::spawn_blocking(quillway_wl::read)).await {
        Ok(Ok(read)) => read.map_err(|e| format!("{e:#}")),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("The app that owns the clipboard didn't respond; type or paste the text instead.".into()),
    }
}

fn stream_rewrite(engine: Engine, req: Rewrite) -> impl Stream<Item = GenEvent> {
    stream::once(async move { engine.client().await?.stream(&req).await })
        .flat_map(|r| match r {
            Ok(s) => s.map(|c| c.map_or_else(|e| GenEvent::Error(format!("{e:#}")), GenEvent::Chunk)).boxed(),
            Err(e) => stream::once(async move { GenEvent::Error(format!("{e:#}")) }).boxed(),
        })
        .chain(stream::once(async { GenEvent::Done }))
}

fn install_stream(entry: &'static Entry) -> impl Stream<Item = InstallEvent> {
    iced::stream::channel(32, async move |mut out: iced::futures::channel::mpsc::Sender<InstallEvent>| {
        let mut progress = out.clone();
        let (mut last, mut sent_at) = (0u64, None::<Instant>);
        let r = models::install(entry, |p| {
            // `done` drops back to 0 when the server ignores the resume range.
            let due = sent_at.is_none_or(|t| t.elapsed() >= download::PROGRESS_INTERVAL);
            if due || p.done < last || p.done == p.total {
                (last, sent_at) = (p.done, Some(Instant::now()));
                let _ = progress.try_send(InstallEvent::Progress(p.done));
            }
        })
        .await;
        let _ = out.send(InstallEvent::Done(r.map_err(|e| format!("{e:#}")))).await;
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply() -> Reply {
        let (tx, _rx) = tokio::sync::oneshot::channel();
        Reply::new(tx)
    }

    fn text(t: &str) -> GenEvent {
        GenEvent::Chunk(Chunk::Text(t.into()))
    }

    fn boot(config: Config) -> App {
        let active = models::active(&config).unwrap();
        App::boot(config, active, None).0
    }

    fn endpoint_config() -> Config {
        let mut config = Config::default();
        config.model.endpoint = Some("http://127.0.0.1:1/v1".into());
        config.model.endpoint_model = Some("m".into());
        config
    }

    fn generating_app() -> App {
        let mut app = boot(endpoint_config());
        let _ = app.update(Message::Ipc(Request::Show { input: Input::Text("their here".into()) }, reply()));
        let _ = app.update(Message::Preset(0));
        assert_eq!(app.popup.as_ref().unwrap().phase(), Phase::Generating);
        app
    }

    fn reviewing_app() -> App {
        let mut app = generating_app();
        let _ = app.update(Message::Gen(0, text("They're here.")));
        let _ = app.update(Message::Gen(0, GenEvent::Done));
        assert_eq!(app.popup.as_ref().unwrap().phase(), Phase::Reviewing);
        app
    }

    fn type_char(app: &mut App, c: char) {
        let _ = app.update(Message::Edit(text_editor::Action::Edit(text_editor::Edit::Insert(c))));
    }

    #[test]
    fn tab_in_review_edits_the_draft_and_undo_restores_the_model_text() {
        let mut app = reviewing_app();
        let _ = app.on_shortcut(Shortcut::Tab);
        assert!(app.popup.as_ref().unwrap().editing());
        type_char(&mut app, ' ');
        type_char(&mut app, ':');
        // Whole-draft shortcuts belong to the editor while it has the keyboard.
        for s in [Shortcut::Undo, Shortcut::Retry, Shortcut::Diff, Shortcut::Preset(1)] {
            let _ = app.on_shortcut(s);
        }
        let p = app.popup.as_ref().unwrap();
        assert!(p.generation.is_none() && p.editing());
        assert_eq!(p.drafts.len(), 2, "consecutive edits make one draft");
        assert_eq!((p.drafts[1].text.as_str(), p.drafts[1].label.as_str()), ("They're here. :", "Edited"));

        let _ = app.on_shortcut(Shortcut::Tab);
        let _ = app.on_shortcut(Shortcut::Retry); // nothing to retry for a hand edit
        assert!(app.popup.as_ref().unwrap().generation.is_none());
        let _ = app.on_shortcut(Shortcut::Undo);
        let p = app.popup.as_ref().unwrap();
        assert_eq!(p.drafts.len(), 1);
        assert_eq!(p.base(), "They're here.");
    }

    #[test]
    fn diff_has_its_own_key_and_is_off_while_editing() {
        let mut app = reviewing_app();
        let shown = app.popup.as_ref().unwrap().show_diff();
        let _ = app.on_shortcut(Shortcut::Diff);
        assert_eq!(app.popup.as_ref().unwrap().show_diff(), !shown);
        let _ = app.on_shortcut(Shortcut::Tab);
        let _ = app.on_shortcut(Shortcut::Diff);
        assert_eq!(app.popup.as_ref().unwrap().show_diff(), !shown, "ignored while editing");
        let _ = app.on_shortcut(Shortcut::Tab);
        assert_eq!(app.popup.as_ref().unwrap().show_diff(), !shown, "Tab no longer toggles the diff");
    }

    #[test]
    fn a_rewrite_started_while_editing_is_reviewed_from_the_instruction() {
        let mut app = reviewing_app();
        let _ = app.on_shortcut(Shortcut::Tab);
        type_char(&mut app, '!');
        let _ = app.update(Message::Preset(0));
        let p = app.popup.as_ref().unwrap();
        assert_eq!(p.generation.as_ref().unwrap().request.text, "They're here.!");
        let _ = app.update(Message::Gen(1, text("They're here!")));
        let _ = app.update(Message::Gen(1, GenEvent::Done));
        assert!(!app.popup.as_ref().unwrap().editing());
    }

    #[test]
    fn clicking_the_instruction_while_editing_stops_editing() {
        let mut app = reviewing_app();
        let id = app.popup.as_ref().unwrap().id;
        let _ = app.on_shortcut(Shortcut::Tab);
        let _ = app.update(Message::Focused(id, None)); // a click on neither box
        assert!(app.popup.as_ref().unwrap().editing());
        let _ = app.update(Message::Focused(id, Some(Field::Instruction)));
        assert!(!app.popup.as_ref().unwrap().editing());
        let _ = app.on_shortcut(Shortcut::Undo); // a whole-draft shortcut works again
        assert_eq!(app.popup.as_ref().unwrap().phase(), Phase::Composing);
    }

    #[test]
    fn a_late_focus_result_cannot_change_a_new_popup() {
        let mut app = reviewing_app();
        let old = app.popup.as_ref().unwrap().id;
        let _ = app.update(Message::Ipc(Request::Hide, reply()));
        let _ = app.update(Message::Ipc(Request::Show { input: Input::Text("new text".into()) }, reply()));
        let current = app.popup.as_ref().unwrap().id;
        assert_ne!(old, current);
        let _ = app.update(Message::Focused(old, Some(Field::Source)));
        assert_eq!(app.popup.as_ref().unwrap().field, Field::Instruction);
    }

    #[test]
    fn a_late_resize_cannot_focus_or_resize_a_new_popup() {
        let mut app = boot(endpoint_config());
        let _ = app.update(Message::Ipc(Request::Show { input: Input::Text("old".into()) }, reply()));
        let old = app.popup.as_ref().unwrap().id;
        let _ = app.update(Message::Ipc(Request::Hide, reply()));
        let _ = app.update(Message::Ipc(Request::Show { input: Input::Text("new".into()) }, reply()));
        let (current, initial_size) = {
            let p = app.popup.as_ref().unwrap();
            (p.id, p.size)
        };
        let _ = app.update(Message::Resized(old, Size::new(680.0, 320.0)));
        let p = app.popup.as_ref().unwrap();
        assert!(!p.focus_given);
        assert_eq!(p.size, initial_size);
        let _ = app.update(Message::Resized(current, Size::new(680.0, 320.0)));
        assert!(app.popup.as_ref().unwrap().focus_given);
    }

    #[test]
    fn an_emptied_draft_is_not_copied() {
        let mut app = reviewing_app();
        let _ = app.on_shortcut(Shortcut::Tab);
        let _ = app.update(Message::Edit(text_editor::Action::SelectAll));
        let _ = app.update(Message::Edit(text_editor::Action::Edit(text_editor::Edit::Backspace)));
        let _ = app.on_shortcut(Shortcut::Tab);
        let _ = app.update(Message::Submit);
        let p = app.popup.as_ref().expect("still open");
        assert!(p.error.as_deref().is_some_and(|e| e.starts_with("Nothing to copy")));
        let _ = app.on_shortcut(Shortcut::Undo);
        assert_eq!(app.popup.as_ref().unwrap().error, None, "the error was about the undone text");
    }

    fn press(key: Key, code: Code, modifiers: keyboard::Modifiers, repeat: bool) -> Option<Message> {
        let event = Event::Keyboard(keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: Physical::Code(code),
            location: keyboard::Location::Standard,
            modifiers,
            text: None,
            repeat,
        });
        shortcut(event, event::Status::Ignored, window::Id::unique())
    }

    fn ctrl(key: &str, code: Code) -> Option<Message> {
        press(Key::Character(key.into()), code, keyboard::Modifiers::CTRL, false)
    }

    #[test]
    fn a_held_key_acts_once() {
        let esc = |repeat| press(Key::Named(Named::Escape), Code::Escape, keyboard::Modifiers::empty(), repeat);
        assert!(matches!(esc(false), Some(Message::Shortcut(Shortcut::Escape, _))));
        let held = Physical::Code(Code::Escape);
        let mut app = generating_app();
        let _ = app.update(Message::Shortcut(Shortcut::Escape, held));
        let _ = app.update(Message::Shortcut(Shortcut::Escape, held)); // auto-repeat, reported as a press
        assert_eq!(app.popup.as_ref().map(Popup::phase), Some(Phase::Composing), "stopped, not closed");
        let _ = app.update(Message::KeyReleased(held));
        let _ = app.update(Message::Shortcut(Shortcut::Escape, held));
        assert!(app.popup.is_none(), "a second press closes");
    }

    #[test]
    fn shortcuts_follow_the_key_position_on_other_layouts() {
        let is = |m: Option<Message>, want: Shortcut| matches!(m, Some(Message::Shortcut(s, _)) if std::mem::discriminant(&s) == std::mem::discriminant(&want));
        assert!(is(ctrl("d", Code::KeyD), Shortcut::Diff));
        assert!(is(ctrl("в", Code::KeyD), Shortcut::Diff)); // Russian
        assert!(is(ctrl("я", Code::KeyZ), Shortcut::Undo));
        assert!(matches!(ctrl("&", Code::Digit1), Some(Message::Shortcut(Shortcut::Preset(0), _)))); // AZERTY
        assert!(matches!(ctrl("9", Code::Digit9), Some(Message::Shortcut(Shortcut::Preset(8), _))));
        assert!(ctrl("c", Code::KeyC).is_none(), "Ctrl+C belongs to the text boxes");
        assert!(ctrl("c", Code::Digit2).is_none(), "the character wins over the position");
        assert!(matches!(ctrl("1", Code::Digit2), Some(Message::Shortcut(Shortcut::Preset(0), _))));
        assert!(ctrl("0", Code::Digit0).is_none());
    }

    #[test]
    fn presets_run_with_ctrl_digits_from_the_instruction_box_only() {
        let mut app = boot(endpoint_config());
        let _ = app.update(Message::Ipc(Request::Show { input: Input::Text("their here".into()) }, reply()));
        let _ = app.update(Message::Input("3".into())); // a lone digit is just text now
        assert_eq!(app.popup.as_ref().unwrap().input, "3");
        let _ = app.on_shortcut(Shortcut::Tab);
        let _ = app.on_shortcut(Shortcut::Preset(0));
        assert_eq!(app.popup.as_ref().unwrap().phase(), Phase::Composing, "ignored in the text box");
        let _ = app.on_shortcut(Shortcut::Tab);
        let _ = app.on_shortcut(Shortcut::Preset(0));
        assert_eq!(app.popup.as_ref().unwrap().generation.as_ref().unwrap().label, "Proofread");
    }

    #[test]
    fn a_chosen_diff_setting_outlives_preset_defaults() {
        let mut app = reviewing_app(); // Proofread shows the diff by default
        assert!(app.popup.as_ref().unwrap().show_diff());
        let _ = app.on_shortcut(Shortcut::Diff);
        let _ = app.on_shortcut(Shortcut::Preset(0));
        let _ = app.update(Message::Gen(1, text("They are here.")));
        let _ = app.update(Message::Gen(1, GenEvent::Done));
        assert!(!app.popup.as_ref().unwrap().show_diff(), "a new draft keeps the choice");
        let _ = app.on_shortcut(Shortcut::Undo);
        assert!(!app.popup.as_ref().unwrap().show_diff(), "so does undo");
    }

    #[test]
    fn error_keeps_partial_output_as_an_incomplete_draft() {
        let mut app = generating_app();
        let _ = app.update(Message::Gen(0, text("They're here, and")));
        let _ = app.update(Message::Gen(0, GenEvent::Error("connection reset".into())));
        let p = app.popup.as_ref().unwrap();
        assert_eq!(p.phase(), Phase::Reviewing);
        assert_eq!(p.drafts[0].text, "They're here, and");
        assert_eq!((p.drafts[0].label.as_str(), p.drafts[0].incomplete), ("Proofread", true));
        assert_eq!(p.error.as_deref(), Some("connection reset"));
    }

    #[test]
    fn retry_of_an_incomplete_draft_replaces_it_with_a_complete_one() {
        let mut app = generating_app();
        let _ = app.update(Message::Gen(0, text("They're")));
        let _ = app.update(Message::Gen(0, GenEvent::Error("connection reset".into())));
        let _ = app.on_shortcut(Shortcut::Retry);
        let _ = app.update(Message::Gen(1, text("They're here.")));
        let _ = app.update(Message::Gen(1, GenEvent::Done));
        let p = app.popup.as_ref().unwrap();
        assert_eq!(p.drafts.len(), 1);
        assert_eq!((p.drafts[0].label.as_str(), p.drafts[0].incomplete), ("Proofread", false));
        assert_eq!(p.error, None);
    }

    #[test]
    fn stdin_text_replaces_a_pending_clipboard_read() {
        let mut app = boot(Config::default());
        let _ = app.update(Message::Ipc(Request::Show { input: Input::Clipboard }, reply()));
        let _ = app.update(Message::Ipc(Request::Show { input: Input::Text("from editor".into()) }, reply()));
        assert_eq!(app.popup.as_ref().unwrap().source.text(), "from editor");
        let _ = app.update(Message::Captured(0, Ok(Some("old clipboard".into()))));
        assert_eq!(app.popup.as_ref().unwrap().source.text(), "from editor");
    }

    #[test]
    fn stdin_text_for_an_open_popup_is_refused_not_dropped() {
        let mut app = boot(Config::default());
        let _ = app.update(Message::Ipc(Request::Show { input: Input::Text("first".into()) }, reply()));
        for req in [
            Request::Show { input: Input::Text("second".into()) },
            Request::Toggle { input: Input::Text("third".into()) },
        ] {
            let (tx, mut rx) = tokio::sync::oneshot::channel();
            let _ = app.update(Message::Ipc(req, Reply::new(tx)));
            assert!(matches!(rx.try_recv(), Ok(Response::Error { .. })));
            assert_eq!(app.popup.as_ref().unwrap().source.text(), "first");
        }
    }

    #[test]
    fn a_failed_custom_instruction_is_given_back() {
        let mut app = boot(endpoint_config());
        let _ = app.update(Message::Ipc(Request::Show { input: Input::Text("their here".into()) }, reply()));
        let _ = app.update(Message::Input("make it formal".into()));
        let _ = app.update(Message::Submit);
        assert_eq!(app.popup.as_ref().unwrap().input, "", "the box is cleared while writing");
        let _ = app.update(Message::Gen(0, GenEvent::Error("too long".into())));
        let p = app.popup.as_ref().unwrap();
        assert_eq!((p.phase(), p.input.as_str()), (Phase::Composing, "make it formal"));
    }

    #[test]
    fn a_retry_sends_the_same_request_at_the_same_temperature() {
        let mut app = reviewing_app(); // Proofread, temperature 0.2
        for id in 1..=3 {
            let _ = app.on_shortcut(Shortcut::Retry);
            let sent = &app.popup.as_ref().unwrap().generation.as_ref().unwrap().request;
            assert!((sent.temperature - 0.2).abs() < f32::EPSILON, "retry {id}: {}", sent.temperature);
            let _ = app.update(Message::Gen(id, text("They're here.")));
            let _ = app.update(Message::Gen(id, GenEvent::Done));
        }
        assert_eq!(app.popup.as_ref().unwrap().drafts.len(), 1, "each retry replaces the draft");
    }

    #[test]
    fn a_license_model_installs_from_the_popup() {
        let mut config = Config::default();
        config.model.active = Some("lfm2.5-1.2b".into());
        let mut app = boot(config);
        let _ = app.update(Message::EngineState(EngineState::Missing)); // as the engine reports it
        let _ = app.update(Message::Ipc(Request::Show { input: Input::Text("hi".into()) }, reply()));
        assert!(app.needs_install());
        let _ = app.update(Message::Submit); // ↵ on the card that shows the license notice
        let install = app.install.as_ref().expect("install started");
        assert_eq!((install.entry.id.as_str(), install.error.as_deref()), ("lfm2.5-1.2b", None));
    }

    #[test]
    fn a_preset_clears_the_instruction_box() {
        let mut app = boot(endpoint_config());
        let _ = app.update(Message::Ipc(Request::Show { input: Input::Text("their here".into()) }, reply()));
        // Ctrl+1 in the instruction box also types "1" there (iced's text_input).
        let _ = app.update(Message::Input("1".into()));
        let _ = app.update(Message::Preset(0));
        let _ = app.update(Message::Gen(0, text("They're here.")));
        let _ = app.update(Message::Gen(0, GenEvent::Done));
        let p = app.popup.as_ref().unwrap();
        assert_eq!((p.phase(), p.input.as_str()), (Phase::Reviewing, ""));
    }

    #[test]
    fn a_reload_stops_a_rewrite_and_says_why() {
        let mut app = generating_app();
        let _ = app.update(Message::Gen(0, text("They're")));
        let _ = app.update(Message::Ipc(Request::Reload, reply()));
        let p = app.popup.as_ref().unwrap();
        assert!(p.generation.is_none());
        assert!(p.error.as_deref().is_some_and(|e| e.contains("restarting for a reload")), "{:?}", p.error);
        assert!(p.drafts[0].incomplete, "the partial text is kept");
    }

    #[test]
    fn tab_on_the_install_card_keeps_the_keyboard_on_the_instruction() {
        let path = std::env::temp_dir().join(format!("quillway-missing-{}.gguf", std::process::id()));
        let mut config = Config::default();
        config.model.active = Some(format!("custom:{}", path.display()));
        let mut app = boot(config);
        let _ = app.update(Message::EngineState(EngineState::Missing));
        let _ = app.update(Message::Ipc(Request::Show { input: Input::Text("hi".into()) }, reply()));
        assert!(app.needs_install());
        let _ = app.on_shortcut(Shortcut::Tab);
        assert_eq!(app.popup.as_ref().unwrap().field, Field::Instruction);
    }

    #[test]
    fn the_install_card_opened_empty_keeps_the_keyboard_on_the_instruction() {
        let path = std::env::temp_dir().join(format!("quillway-missing-{}.gguf", std::process::id()));
        let mut config = Config::default();
        config.model.active = Some(format!("custom:{}", path.display()));
        let mut app = boot(config);
        let _ = app.update(Message::EngineState(EngineState::Missing));
        let _ = app.update(Message::Ipc(Request::Show { input: Input::Text(String::new()) }, reply()));
        assert!(app.needs_install());
        assert_eq!(app.popup.as_ref().unwrap().field, Field::Instruction);
    }

    #[test]
    fn install_events_for_another_model_are_ignored() {
        let mut app = boot(endpoint_config());
        let qwen = quillway_core::catalog::get("qwen3.5-2b").unwrap();
        let gemma = quillway_core::catalog::get("gemma-4-e4b").unwrap();
        app.install = Some(Install::new(qwen));
        let _ = app.update(Message::Install(gemma, InstallEvent::Progress(42)));
        assert_eq!(app.install.as_ref().unwrap().done, 0);
        let _ = app.update(Message::Install(qwen, InstallEvent::Progress(42)));
        assert_eq!(app.install.as_ref().unwrap().done, 42);
    }

    #[test]
    fn editing_back_to_the_model_text_drops_the_edited_step() {
        let mut app = reviewing_app();
        let _ = app.on_shortcut(Shortcut::Tab);
        type_char(&mut app, '!');
        assert_eq!(app.popup.as_ref().unwrap().drafts.len(), 2);
        let _ = app.update(Message::Edit(text_editor::Action::Edit(text_editor::Edit::Backspace)));
        assert_eq!(app.popup.as_ref().unwrap().drafts.len(), 1, "back to the model's text");
    }

    #[test]
    fn a_preset_refused_for_empty_text_keeps_the_typed_instruction() {
        let mut app = boot(endpoint_config());
        let _ = app.update(Message::Ipc(Request::Show { input: Input::Text(String::new()) }, reply()));
        let _ = app.update(Message::Input("make it formal".into()));
        let _ = app.update(Message::Preset(0));
        let p = app.popup.as_ref().unwrap();
        assert!(p.error.as_deref().is_some_and(|e| e.starts_with("Nothing to rewrite")));
        assert_eq!(p.input, "make it formal");
    }

    #[test]
    fn the_install_card_goes_once_the_model_exists() {
        let path = std::env::temp_dir().join(format!("quillway-card-{}.gguf", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut config = Config::default();
        config.model.active = Some(format!("custom:{}", path.display()));
        let mut app = boot(config);
        let _ = app.update(Message::EngineState(EngineState::Missing));
        let _ = app.update(Message::Ipc(Request::Show { input: Input::Text("hi".into()) }, reply()));
        assert!(app.needs_install());
        std::fs::write(&path, b"gguf").unwrap(); // installed; the engine's next state is still on its way
        let installed = !app.needs_install();
        std::fs::remove_file(&path).unwrap();
        assert!(installed);
    }

    #[test]
    fn piped_text_is_labelled_stdin() {
        let mut app = boot(endpoint_config());
        let _ = app.update(Message::Ipc(Request::Show { input: Input::Text("hi".into()) }, reply()));
        assert_eq!(app.popup.as_ref().unwrap().origin, Origin::Stdin);
    }

    #[test]
    fn error_before_any_output_adds_no_draft() {
        let mut app = generating_app();
        let _ = app.update(Message::Gen(0, GenEvent::Error("too long".into())));
        let p = app.popup.as_ref().unwrap();
        assert_eq!(p.phase(), Phase::Composing);
        assert_eq!(p.error.as_deref(), Some("too long"));
    }

    #[test]
    fn escape_stops_generation_without_closing_the_popup() {
        let mut app = generating_app();
        let _ = app.on_shortcut(Shortcut::Escape);
        assert_eq!(app.popup.as_ref().unwrap().phase(), Phase::Composing);
        let _ = app.update(Message::Gen(0, text("late response")));
        assert_eq!(app.popup.as_ref().unwrap().phase(), Phase::Composing);
    }

    #[test]
    fn hide_during_clipboard_read_keeps_popup_closed() {
        let mut app = boot(Config::default());
        let _ = app.update(Message::Ipc(Request::Show { input: Input::Clipboard }, reply()));
        let _ = app.update(Message::Ipc(Request::Hide, reply()));
        let _ = app.update(Message::Captured(0, Ok(Some("old clipboard".into()))));
        assert!(app.popup.is_none());
    }

    #[test]
    fn cancelled_clipboard_read_cannot_supply_a_new_show() {
        let mut app = boot(Config::default());
        let _ = app.update(Message::Ipc(Request::Show { input: Input::Clipboard }, reply()));
        let _ = app.update(Message::Ipc(Request::Toggle { input: Input::Clipboard }, reply()));
        let _ = app.update(Message::Ipc(Request::Show { input: Input::Clipboard }, reply()));
        let _ = app.update(Message::Captured(0, Ok(Some("old clipboard".into()))));
        assert!(app.popup.is_none());
        let _ = app.update(Message::Captured(1, Ok(Some("new clipboard".into()))));
        assert_eq!(app.popup.as_ref().unwrap().source.text(), "new clipboard");
    }
}
