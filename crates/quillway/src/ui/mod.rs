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
use quillway_core::catalog::{self, Entry};
use quillway_core::config::{Config, Preset};
use quillway_core::ipc::{Input, Request, Response};
use quillway_core::{clean, paths, prompt};
use quillway_engine::download::{self, Job};
use quillway_engine::{Active, Engine, Rewrite, models};

use crate::ipc::{self, Reply};

const INPUT_ID: &str = "quillway-input";
const SOURCE_ID: &str = "quillway-source";
const MAX_SURFACE_HEIGHT: u32 = 760;
const FADE_IN: Duration = Duration::from_millis(140);
const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(2);

/// Bound before the UI starts; taken once by the IPC subscription.
static LISTENER: Mutex<Option<UnixListener>> = Mutex::new(None);

pub fn run() -> anyhow::Result<()> {
    *LISTENER.lock().expect("listener lock") = Some(ipc::bind()?);
    let config = Config::load(&paths::config_file())?;
    let watch = quillway_wl::ClipboardWatch::start()
        .inspect_err(|e| eprintln!("quillway: {e:#}; the clipboard will always count as recent"))
        .ok();
    // `Font::with_name` needs a `'static` name; this runs once per process.
    let default_font =
        config.ui.font.as_ref().map_or_else(iced::Font::default, |f| iced::Font::with_name(f.clone().leak()));
    iced_layershell::daemon(move || App::boot(config.clone(), watch.clone()), namespace, App::update, App::view)
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

fn namespace() -> String {
    "quillway".into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Clipboard,
    Editor,
    /// Nothing recent to start from: the user types or pastes the text.
    Typed,
}

impl Origin {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Clipboard => "clipboard",
            Self::Editor => "editor",
            Self::Typed => "typed",
        }
    }
}

/// Which box has the keyboard: the instruction, or the text (the source while
/// composing, the latest draft while reviewing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Instruction,
    Source,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineState {
    Starting,
    Ready,
    Missing,
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Composing,
    Generating,
    Reviewing,
}

pub struct Draft {
    pub text: String,
    pub label: String,
    pub stats: String,
    pub show_diff: bool,
    /// Generation failed part-way; this is what arrived.
    pub incomplete: bool,
    /// Edited by hand; holds the request of the draft it was edited from.
    pub edited: bool,
    request: Rewrite,
}

pub struct Generation {
    id: u64,
    handle: task::Handle,
    pub raw: String,
    pub label: String,
    show_diff: bool,
    /// A retry: replaces the latest draft when it finishes (kept if cancelled).
    replace: bool,
    request: Rewrite,
    started: Instant,
    first: Option<Instant>,
    deltas: usize,
}

pub struct Popup {
    id: window::Id,
    /// The text to rewrite, editable until the first generation.
    pub source: text_editor::Content,
    /// `source` as it was when the first generation started; the diff base.
    pub original: String,
    pub origin: Origin,
    pub field: Field,
    pub input: String,
    pub drafts: Vec<Draft>,
    /// The latest draft, while it is being edited by hand.
    pub draft_editor: text_editor::Content,
    pub generation: Option<Generation>,
    pub show_diff: bool,
    /// Set by Ctrl+D: the user's choice outlives the presets' defaults.
    diff_pinned: bool,
    pub error: Option<String>,
    focused: bool,
    size: (u32, u32),
    opened: Instant,
}

impl Popup {
    pub const fn phase(&self) -> Phase {
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
    pub fn editing(&self) -> bool {
        self.phase() == Phase::Reviewing && self.field == Field::Source
    }

    /// Hand edits go into an "Edited" draft, so Ctrl+Z restores the model's text.
    fn keep_edit(&mut self) {
        self.error = None; // it was about the text before this edit
        let text = self.draft_editor.text();
        let Some(last) = self.drafts.last_mut() else { return };
        if last.edited {
            last.text = text;
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
            edited: true,
            request: last.request.clone(),
        };
        self.drafts.push(draft);
    }
}

pub struct Install {
    pub entry: &'static Entry,
    pub done: u64,
    pub error: Option<String>,
}

pub struct App {
    config: Config,
    pub presets: Vec<Preset>,
    engine: Engine,
    pub engine_state: EngineState,
    pub active: Active,
    pub palette: style::Palette,
    pub popup: Option<Popup>,
    pub install: Option<Install>,
    pub now: Instant,
    next_gen: u64,
    /// The latest warm-up; results of earlier ones (superseded by a reload) are ignored.
    warm_id: u64,
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
pub enum Shortcut {
    Escape,
    Tab,
    Diff,
    Retry,
    Undo,
    Preset(usize),
}

#[derive(Debug, Clone)]
pub enum GenEvent {
    Delta(String),
    Error(String),
    Done,
}

#[derive(Debug, Clone)]
pub enum InstallEvent {
    Progress(u64),
    Done(Result<(), String>),
}

#[to_layer_message(multi)]
#[derive(Debug, Clone)]
pub enum Message {
    Ipc(Request, Reply),
    Captured {
        id: u64,
        text: Option<String>,
        error: Option<String>,
    },
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
    Resized(Size),
    Tick(Instant),
    Engine(u64, Result<(), String>),
    InstallStart,
    Install(InstallEvent),
    WindowClosed(window::Id),
}

impl App {
    fn boot(config: Config, watch: Option<quillway_wl::ClipboardWatch>) -> (Self, Task<Message>) {
        let engine = Engine::new(config.clone());
        let mut app = Self {
            presets: config.presets(),
            active: models::active(&config),
            palette: style::Palette::new(&config.ui),
            engine_state: EngineState::Starting,
            config,
            engine,
            popup: None,
            install: None,
            now: Instant::now(),
            next_gen: 0,
            warm_id: 0,
            watch,
            capturing: None,
            next_capture: 0,
            held: None,
        };
        let warm = app.warm_up();
        (app, warm)
    }

    fn warm_up(&mut self) -> Task<Message> {
        if self.config.model.endpoint.is_none() && !self.active.path.is_file() {
            self.engine_state = EngineState::Missing;
            return Task::none();
        }
        self.engine_state = EngineState::Starting;
        self.warm_id += 1;
        let id = self.warm_id;
        let engine = self.engine.clone();
        Task::perform(async move { engine.warm_up().await.map_err(|e| format!("{e:#}")) }, move |r| {
            Message::Engine(id, r)
        })
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subs = vec![
            Subscription::run(ipc_stream).map(|(req, reply)| Message::Ipc(req, reply)),
            event::listen_with(shortcut),
            event::listen_with(|event, _, id| {
                matches!(event, Event::Mouse(iced::mouse::Event::ButtonPressed(_))).then_some(Message::Clicked(id))
            }),
            window::close_events().map(Message::WindowClosed),
        ];
        if self.animating() {
            subs.push(iced::time::every(Duration::from_millis(16)).map(Message::Tick));
        }
        Subscription::batch(subs)
    }

    fn animating(&self) -> bool {
        self.popup.as_ref().is_some_and(|p| p.generation.is_some() || p.opened.elapsed() < FADE_IN)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Ipc(req, reply) => self.ipc(req, &reply),
            Message::Captured { id, text, error } => self.captured(id, text, error),
            Message::Input(s) => self.on_input(s),
            Message::Edit(action) => {
                if let Some(p) = self.popup.as_mut() {
                    if p.phase() == Phase::Composing {
                        p.source.perform(action);
                    } else if p.editing() {
                        let edit = action.is_edit();
                        p.draft_editor.perform(action);
                        if edit {
                            p.keep_edit();
                        }
                    }
                }
                Task::none()
            }
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
            Message::Clicked(id) => match self.popup.as_ref().filter(|p| p.id == id).map(Popup::phase) {
                Some(Phase::Composing | Phase::Reviewing) => {
                    iced::widget::operation::is_focused(INPUT_ID).then(move |input| {
                        if input {
                            Task::done(Message::Focused(id, Some(Field::Instruction)))
                        } else {
                            iced::widget::operation::is_focused(SOURCE_ID)
                                .map(move |source| Message::Focused(id, source.then_some(Field::Source)))
                        }
                    })
                }
                _ => Task::none(),
            },
            Message::Focused(id, field) => {
                // Keep `field` in step with clicks, so Tab and the shortcuts act on the focused box;
                // a click on neither box gives the keyboard back to `field`'s.
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
            Message::Resized(size) => self.on_resize(size),
            Message::Tick(now) => {
                self.now = now;
                Task::none()
            }
            Message::Engine(id, _) if id != self.warm_id => Task::none(),
            Message::Engine(_, r) => {
                self.engine_state = match r {
                    Ok(()) => EngineState::Ready,
                    Err(_) if self.config.model.endpoint.is_none() && !self.active.path.is_file() => {
                        EngineState::Missing
                    }
                    Err(e) => {
                        eprintln!("quillway: engine: {e}");
                        EngineState::Failed(e)
                    }
                };
                Task::none()
            }
            Message::InstallStart => self.start_install(),
            Message::Install(ev) => self.on_install(ev),
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
                        self.open(t, Origin::Editor, None)
                    }
                    Input::Clipboard if self.capturing.is_some() => Task::none(),
                    Input::Clipboard if self.clipboard_recent() => {
                        let id = self.next_capture;
                        self.next_capture += 1;
                        self.capturing = Some(id);
                        Task::perform(read_clipboard(), move |(text, error)| Message::Captured { id, text, error })
                    }
                    Input::Clipboard => self.open(String::new(), Origin::Typed, None),
                }
            }
            Request::Hide => {
                reply.send(Response::Ok);
                self.capturing = None;
                self.hide()
            }
            Request::Reload => match Config::load(&paths::config_file()) {
                Ok(config) => {
                    self.presets = config.presets();
                    self.active = models::active(&config);
                    self.palette = style::Palette::new(&config.ui);
                    self.config = config.clone();
                    let engine = self.engine.clone();
                    let reply = reply.clone();
                    let reload = Task::perform(
                        async move {
                            engine.reload(config).await;
                            reply.send(Response::Ok);
                        },
                        |()| Message::Tick(Instant::now()),
                    );
                    reload.chain(self.warm_up())
                }
                Err(e) => {
                    reply.send(Response::Error { message: format!("{e:#}") });
                    Task::none()
                }
            },
            Request::Status => {
                let engine = match &self.engine_state {
                    EngineState::Starting => "starting".to_owned(),
                    EngineState::Ready => "ready".to_owned(),
                    EngineState::Missing => "model not installed".to_owned(),
                    EngineState::Failed(e) => format!("failed: {e}"),
                };
                reply.send(Response::Status { visible: self.popup.is_some(), model: self.model_label(), engine });
                Task::none()
            }
            Request::Connect => {
                let (engine, reply, sampling) = (self.engine.clone(), reply.clone(), self.active.sampling);
                Task::perform(
                    async move {
                        reply.send(match engine.client().await {
                            Ok(c) => Response::Server {
                                base: c.base().to_owned(),
                                api_key: c.api_key().map(str::to_owned),
                                model: c.model().to_owned(),
                                llama: c.is_llama(),
                                context: c.context(),
                                sampling,
                            },
                            Err(e) => Response::Error { message: format!("{e:#}") },
                        });
                    },
                    |()| Message::Tick(Instant::now()),
                )
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
    fn clipboard_recent(&self) -> bool {
        let window = Duration::from_secs(self.config.behavior.recent_secs);
        self.watch.as_ref().is_none_or(|w| w.last_change().is_some_and(|t| t.elapsed() < window))
    }

    fn captured(&mut self, id: u64, text: Option<String>, error: Option<String>) -> Task<Message> {
        if self.capturing != Some(id) || self.popup.is_some() {
            return Task::none(); // cancelled by a second toggle
        }
        self.capturing = None;
        match text {
            Some(t) => self.open(t, Origin::Clipboard, error),
            _ => self.open(String::new(), Origin::Typed, error),
        }
    }

    fn open(&mut self, text: String, origin: Origin, error: Option<String>) -> Task<Message> {
        // The model may have been installed since we last looked (copied in, or by a CLI
        // that couldn't reach us); start it instead of offering the install card.
        let warm = if self.engine_state == EngineState::Missing && self.active.path.is_file() {
            self.warm_up()
        } else {
            Task::none()
        };
        // A key held as the last popup closed was released elsewhere.
        self.held = None;
        let id = window::Id::unique();
        let size = self.surface_size(196);
        let top = i32::try_from(self.config.ui.top_margin.saturating_sub(self.margin())).unwrap_or(i32::MAX);
        self.now = Instant::now();
        self.popup = Some(Popup {
            id,
            field: if text.trim().is_empty() { Field::Source } else { Field::Instruction },
            source: editor_content(&text),
            original: text,
            origin,
            input: String::new(),
            drafts: Vec::new(),
            draft_editor: text_editor::Content::new(),
            generation: None,
            show_diff: false,
            diff_pinned: false,
            error,
            focused: false,
            size,
            opened: Instant::now(),
        });
        let open = Task::done(Message::NewLayerShell {
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
        });
        Task::batch([warm, open])
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
    fn on_resize(&mut self, panel: Size) -> Task<Message> {
        let mut tasks = Vec::new();
        let want = self.surface_size(panel.height.ceil() as u32);
        let Some(p) = self.popup.as_mut() else { return Task::none() };
        if !p.focused {
            p.focused = true;
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
        let Some(p) = self.popup.as_ref() else { return Task::none() };
        if self.needs_install(p) {
            return self.start_install();
        }
        let Some(p) = self.popup.as_mut() else { return Task::none() };
        match p.phase() {
            Phase::Composing => {
                let instruction = p.input.trim().to_owned();
                if instruction.is_empty() {
                    return Task::none();
                }
                self.generate("Custom".into(), instruction, 0.7, false)
            }
            Phase::Reviewing => {
                let instruction = p.input.trim().to_owned();
                if instruction.is_empty() {
                    self.copy()
                } else {
                    self.generate("Refine".into(), instruction, 0.7, false)
                }
            }
            Phase::Generating => Task::none(),
        }
    }

    fn run_preset(&mut self, i: usize) -> Task<Message> {
        let Some(preset) = self.presets.get(i).cloned() else { return Task::none() };
        if self.popup.as_ref().is_none_or(|p| p.generation.is_some() || self.needs_install(p)) {
            return Task::none();
        }
        self.generate(preset.name, preset.instruction, preset.temperature.unwrap_or(0.7), preset.show_diff)
    }

    fn generate(&mut self, label: String, instruction: String, temperature: f32, show_diff: bool) -> Task<Message> {
        let sampling = self.active.sampling;
        let Some(p) = self.popup.as_mut() else { return Task::none() };
        let text = p.base();
        if text.trim().is_empty() {
            p.error = Some("Nothing to rewrite: type or paste the text into the box (Tab switches boxes).".into());
            return Task::none();
        }
        if text.contains(prompt::STOP) {
            p.error = Some(format!("The text contains `{}`, which Quillway uses as a delimiter.", prompt::STOP));
            return Task::none();
        }
        // The client counts tokens and rejects text too long for the context.
        let request = Rewrite { instruction, max_tokens: None, text, temperature, sampling };
        if p.drafts.is_empty() {
            p.original.clone_from(&request.text);
        }
        self.start(label, request, show_diff, false)
    }

    fn start(&mut self, label: String, request: Rewrite, show_diff: bool, replace: bool) -> Task<Message> {
        let id = self.next_gen;
        self.next_gen += 1;
        let Some(p) = self.popup.as_mut() else { return Task::none() };
        let (task, handle) =
            Task::run(stream_rewrite(self.engine.clone(), request.clone()), move |ev| Message::Gen(id, ev)).abortable();
        p.error = None;
        p.input.clear();
        // The new draft is reviewed from the instruction box, even if this started while editing.
        p.field = Field::Instruction;
        p.generation = Some(Generation {
            id,
            handle,
            raw: String::new(),
            label,
            show_diff,
            replace,
            request,
            started: Instant::now(),
            first: None,
            deltas: 0,
        });
        Task::batch([task, focus(Field::Instruction)])
    }

    fn on_gen(&mut self, id: u64, ev: GenEvent) -> Task<Message> {
        let Some(p) = self.popup.as_mut() else { return Task::none() };
        let Some(g) = p.generation.as_mut().filter(|g| g.id == id) else { return Task::none() };
        match ev {
            GenEvent::Delta(d) => {
                // Text is flowing, so whatever failed before has recovered.
                if matches!(self.engine_state, EngineState::Failed(_) | EngineState::Starting) {
                    self.engine_state = EngineState::Ready;
                }
                g.first.get_or_insert_with(Instant::now);
                g.deltas += 1;
                g.raw.push_str(&d);
            }
            GenEvent::Error(e) => {
                let g = p.generation.take().expect("matched above");
                let partial = clean::clean(&g.raw, &g.request.text, false).trim_end().to_owned();
                // Keep what arrived, marked, unless a retry failed (its old draft stays).
                if !partial.trim().is_empty() && !g.replace {
                    if !p.diff_pinned {
                        p.show_diff = g.show_diff;
                    }
                    p.drafts.push(Draft {
                        text: partial,
                        label: g.label,
                        stats: "stopped early".into(),
                        show_diff: g.show_diff,
                        incomplete: true,
                        edited: false,
                        request: g.request,
                    });
                }
                p.error = Some(e);
            }
            GenEvent::Done => {
                let g = p.generation.take().expect("matched above");
                let text = clean::clean(&g.raw, &g.request.text, true);
                if text.trim().is_empty() {
                    p.error = Some("The model returned nothing. Run it again, or try another preset.".into());
                    return Task::none();
                }
                let secs = g.started.elapsed().as_secs_f64();
                let gen_secs = g.first.map_or(secs, |f| f.elapsed().as_secs_f64()).max(1e-3);
                let rate = f64::from(u32::try_from(g.deltas.saturating_sub(1)).unwrap_or(u32::MAX)) / gen_secs;
                if !p.diff_pinned {
                    p.show_diff = g.show_diff;
                }
                if g.replace {
                    p.drafts.pop();
                }
                p.drafts.push(Draft {
                    text,
                    label: g.label,
                    stats: format!("{secs:.1}s · {rate:.0} tok/s"),
                    show_diff: g.show_diff,
                    incomplete: false,
                    edited: false,
                    request: g.request,
                });
            }
        }
        Task::none()
    }

    fn on_shortcut(&mut self, s: Shortcut) -> Task<Message> {
        let Some(p) = self.popup.as_mut() else { return Task::none() };
        match (s, p.phase()) {
            (Shortcut::Escape, Phase::Generating) => {
                self.abort_generation();
                Task::none()
            }
            (Shortcut::Escape, _) => self.hide(),
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
                p.show_diff = !p.show_diff;
                p.diff_pinned = true;
                Task::none()
            }
            // An edited draft has no request of its own to retry.
            (Shortcut::Retry, Phase::Reviewing) if p.drafts.last().is_some_and(|d| !d.edited) => {
                let d = p.drafts.last().expect("reviewing has a draft");
                let mut request = d.request.clone();
                request.temperature = (request.temperature + 0.3).min(1.2);
                let (label, show_diff) = (d.label.clone(), d.show_diff);
                self.start(label, request, show_diff, true)
            }
            (Shortcut::Undo, Phase::Reviewing) => {
                p.drafts.pop();
                p.error = None;
                if !p.diff_pinned {
                    p.show_diff = p.drafts.last().is_some_and(|d| d.show_diff);
                }
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
        if self.install.as_ref().is_some_and(|i| i.error.is_none()) {
            return Task::none();
        }
        let entry = if self.active.catalog {
            catalog::find(&self.active.id).unwrap_or_else(catalog::default_entry)
        } else {
            return Task::none();
        };
        if entry.license_notice {
            // Non-OSI licenses need an explicit, informed yes: use the CLI.
            self.install = Some(Install {
                entry,
                done: 0,
                error: Some(format!(
                    "{} needs license confirmation: run `quillway models install {}`",
                    entry.name, entry.id
                )),
            });
            return Task::none();
        }
        self.install = Some(Install { entry, done: 0, error: None });
        Task::run(install_stream(entry), Message::Install)
    }

    fn on_install(&mut self, ev: InstallEvent) -> Task<Message> {
        let Some(i) = self.install.as_mut() else { return Task::none() };
        match ev {
            InstallEvent::Progress(done) => {
                i.done = done;
                Task::none()
            }
            InstallEvent::Done(Ok(())) => {
                self.install = None;
                self.warm_up()
            }
            InstallEvent::Done(Err(e)) => {
                i.error = Some(e);
                Task::none()
            }
        }
    }

    /// The install card replaces the composer only before anything was generated.
    pub fn needs_install(&self, p: &Popup) -> bool {
        self.engine_state == EngineState::Missing && p.phase() == Phase::Composing
    }

    /// Name shown in the footer and `status`.
    pub fn model_label(&self) -> String {
        match &self.config.model.endpoint {
            Some(_) => self.config.model.endpoint_model.clone().unwrap_or_else(|| "endpoint".into()),
            None => self.active.name.clone(),
        }
    }

    pub fn fade(&self) -> f32 {
        let Some(p) = &self.popup else { return 1.0 };
        let t = self.now.saturating_duration_since(p.opened).as_secs_f32() / FADE_IN.as_secs_f32();
        let t = t.clamp(0.0, 1.0);
        1.0 - (1.0 - t).powi(3)
    }

    /// Rotation of the shimmer ring, in turns, while generating.
    pub fn shimmer(&self) -> Option<f32> {
        let g = self.popup.as_ref()?.generation.as_ref()?;
        Some((self.now.saturating_duration_since(g.started).as_secs_f32() / 2.4).fract())
    }

    pub fn streaming_text(&self) -> Option<String> {
        let g = self.popup.as_ref()?.generation.as_ref()?;
        clean::ready(&g.raw, false).then(|| clean::clean(&g.raw, &g.request.text, false))
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

/// Returns (text, error).
async fn read_clipboard() -> (Option<String>, Option<String>) {
    // The app that owns the clipboard sends the data; a hung one must not block the popup.
    match tokio::time::timeout(CLIPBOARD_TIMEOUT, tokio::task::spawn_blocking(quillway_wl::read)).await {
        Ok(Ok(Ok(text))) => (text, None),
        Ok(Ok(Err(e))) => (None, Some(format!("{e:#}"))),
        Ok(Err(e)) => (None, Some(e.to_string())),
        Err(_) => {
            (None, Some("The app that owns the clipboard didn't respond; type or paste the text instead.".into()))
        }
    }
}

fn stream_rewrite(engine: Engine, req: Rewrite) -> impl Stream<Item = GenEvent> {
    stream::once(async move {
        match engine.client().await {
            Ok(c) => c.stream(&req).await,
            Err(e) => Err(e),
        }
    })
    .flat_map(|r| match r {
        Ok(s) => s.map(|d| d.map_or_else(|e| GenEvent::Error(format!("{e:#}")), GenEvent::Delta)).boxed(),
        Err(e) => stream::once(async move { GenEvent::Error(format!("{e:#}")) }).boxed(),
    })
    .chain(stream::once(async { GenEvent::Done }))
}

fn install_stream(entry: &'static Entry) -> impl Stream<Item = InstallEvent> {
    iced::stream::channel(32, async move |mut out: iced::futures::channel::mpsc::Sender<InstallEvent>| {
        let dest = entry.path_in(&paths::models_dir());
        let url = entry.url();
        let mut progress = out.clone();
        let mut last = 0u64;
        let r = download::download(Job { url: &url, dest: &dest, size: entry.size, sha256: &entry.sha256 }, |p| {
            // `done` drops back to 0 when the server ignores the resume range.
            if p.done < last || p.done - last >= 8 << 20 || p.done == p.total {
                last = p.done;
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

    fn endpoint_config() -> Config {
        let mut config = Config::default();
        config.model.endpoint = Some("http://127.0.0.1:1/v1".into());
        config.model.endpoint_model = Some("m".into());
        config
    }

    fn generating_app() -> App {
        let (mut app, _) = App::boot(endpoint_config(), None);
        let _ = app.update(Message::Ipc(Request::Show { input: Input::Text("their here".into()) }, reply()));
        let _ = app.update(Message::Preset(0));
        assert_eq!(app.popup.as_ref().unwrap().phase(), Phase::Generating);
        app
    }

    fn reviewing_app() -> App {
        let mut app = generating_app();
        let _ = app.update(Message::Gen(0, GenEvent::Delta("They're here.".into())));
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
        let shown = app.popup.as_ref().unwrap().show_diff;
        let _ = app.on_shortcut(Shortcut::Diff);
        assert_eq!(app.popup.as_ref().unwrap().show_diff, !shown);
        let _ = app.on_shortcut(Shortcut::Tab);
        let _ = app.on_shortcut(Shortcut::Diff);
        assert_eq!(app.popup.as_ref().unwrap().show_diff, !shown, "ignored while editing");
        let _ = app.on_shortcut(Shortcut::Tab);
        assert_eq!(app.popup.as_ref().unwrap().show_diff, !shown, "Tab no longer toggles the diff");
    }

    #[test]
    fn a_rewrite_started_while_editing_is_reviewed_from_the_instruction() {
        let mut app = reviewing_app();
        let _ = app.on_shortcut(Shortcut::Tab);
        type_char(&mut app, '!');
        let _ = app.update(Message::Preset(0));
        let p = app.popup.as_ref().unwrap();
        assert_eq!(p.generation.as_ref().unwrap().request.text, "They're here.!");
        let _ = app.update(Message::Gen(1, GenEvent::Delta("They're here!".into())));
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
        let (mut app, _) = App::boot(endpoint_config(), None);
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
        assert!(app.popup.as_ref().unwrap().show_diff);
        let _ = app.on_shortcut(Shortcut::Diff);
        let _ = app.on_shortcut(Shortcut::Preset(0));
        let _ = app.update(Message::Gen(1, GenEvent::Delta("They are here.".into())));
        let _ = app.update(Message::Gen(1, GenEvent::Done));
        assert!(!app.popup.as_ref().unwrap().show_diff, "a new draft keeps the choice");
        let _ = app.on_shortcut(Shortcut::Undo);
        assert!(!app.popup.as_ref().unwrap().show_diff, "so does undo");
    }

    #[test]
    fn opening_the_popup_notices_a_model_installed_meanwhile() {
        let path = std::env::temp_dir().join(format!("quillway-test-{}.gguf", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut config = Config::default();
        config.model.active = Some(format!("custom:{}", path.display()));
        let (mut app, _) = App::boot(config, None);
        assert_eq!(app.engine_state, EngineState::Missing);
        std::fs::write(&path, b"gguf").unwrap();
        let _ = app.update(Message::Ipc(Request::Show { input: Input::Text("hi".into()) }, reply()));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(app.engine_state, EngineState::Starting);
    }

    #[test]
    fn superseded_warm_up_result_is_ignored() {
        let (mut app, _) = App::boot(endpoint_config(), None);
        let first = app.warm_id;
        let _ = app.warm_up(); // a reload
        let _ = app.update(Message::Engine(first, Err("killed by the reload".into())));
        assert_eq!(app.engine_state, EngineState::Starting);
        let _ = app.update(Message::Engine(app.warm_id, Ok(())));
        assert_eq!(app.engine_state, EngineState::Ready);
    }

    #[test]
    fn error_keeps_partial_output_as_an_incomplete_draft() {
        let mut app = generating_app();
        let _ = app.update(Message::Gen(0, GenEvent::Delta("They're here, and".into())));
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
        let _ = app.update(Message::Gen(0, GenEvent::Delta("They're".into())));
        let _ = app.update(Message::Gen(0, GenEvent::Error("connection reset".into())));
        let _ = app.on_shortcut(Shortcut::Retry);
        let _ = app.update(Message::Gen(1, GenEvent::Delta("They're here.".into())));
        let _ = app.update(Message::Gen(1, GenEvent::Done));
        let p = app.popup.as_ref().unwrap();
        assert_eq!(p.drafts.len(), 1);
        assert_eq!((p.drafts[0].label.as_str(), p.drafts[0].incomplete), ("Proofread", false));
        assert_eq!(p.error, None);
    }

    #[test]
    fn stdin_text_replaces_a_pending_clipboard_read() {
        let (mut app, _) = App::boot(Config::default(), None);
        let _ = app.update(Message::Ipc(Request::Show { input: Input::Clipboard }, reply()));
        let _ = app.update(Message::Ipc(Request::Show { input: Input::Text("from editor".into()) }, reply()));
        assert_eq!(app.popup.as_ref().unwrap().source.text(), "from editor");
        let _ = app.update(Message::Captured { id: 0, text: Some("old clipboard".into()), error: None });
        assert_eq!(app.popup.as_ref().unwrap().source.text(), "from editor");
    }

    #[test]
    fn stdin_text_for_an_open_popup_is_refused_not_dropped() {
        let (mut app, _) = App::boot(Config::default(), None);
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
        let _ = app.update(Message::Gen(0, GenEvent::Delta("late response".into())));
        assert_eq!(app.popup.as_ref().unwrap().phase(), Phase::Composing);
    }

    #[test]
    fn hide_during_clipboard_read_keeps_popup_closed() {
        let (mut app, _) = App::boot(Config::default(), None);
        let _ = app.update(Message::Ipc(Request::Show { input: Input::Clipboard }, reply()));
        let _ = app.update(Message::Ipc(Request::Hide, reply()));
        let _ = app.update(Message::Captured { id: 0, text: Some("old clipboard".into()), error: None });
        assert!(app.popup.is_none());
    }

    #[test]
    fn cancelled_clipboard_read_cannot_supply_a_new_show() {
        let (mut app, _) = App::boot(Config::default(), None);
        let _ = app.update(Message::Ipc(Request::Show { input: Input::Clipboard }, reply()));
        let _ = app.update(Message::Ipc(Request::Toggle { input: Input::Clipboard }, reply()));
        let _ = app.update(Message::Ipc(Request::Show { input: Input::Clipboard }, reply()));
        let _ = app.update(Message::Captured { id: 0, text: Some("old clipboard".into()), error: None });
        assert!(app.popup.is_none());
        let _ = app.update(Message::Captured { id: 1, text: Some("new clipboard".into()), error: None });
        assert_eq!(app.popup.as_ref().unwrap().source.text(), "new clipboard");
    }
}
