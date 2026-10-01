//! The daemon: an `iced_layershell` program with no surface until `show`.
//!
//! One popup at a time. Its life: take the input text (recent clipboard, text
//! from the CLI, or typed into the popup) → compose → generate (streamed) →
//! review / refine → copy → hide.

mod style;
mod view;

use std::os::unix::net::UnixListener;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, Stream, StreamExt, stream};
use iced::keyboard::{self, Key, key::Named};
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
const COPIED_LINGER: Duration = Duration::from_millis(450);
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

/// Which box has the keyboard while composing.
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
    Copied,
}

pub struct Draft {
    pub text: String,
    pub label: String,
    pub stats: String,
    pub show_diff: bool,
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
    pub generation: Option<Generation>,
    pub show_diff: bool,
    pub copied: bool,
    pub error: Option<String>,
    focused: bool,
    size: (u32, u32),
    opened: Instant,
}

impl Popup {
    pub const fn phase(&self) -> Phase {
        if self.copied {
            Phase::Copied
        } else if self.generation.is_some() {
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
    /// `None` if the compositor can't report clipboard changes.
    watch: Option<quillway_wl::ClipboardWatch>,
    /// A clipboard read is in flight; a second toggle cancels it.
    capturing: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum Shortcut {
    Escape,
    Tab,
    Retry,
    Undo,
    Copy,
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
        text: Option<String>,
        error: Option<String>,
    },
    Input(String),
    Edit(text_editor::Action),
    Submit,
    Preset(usize),
    Gen(u64, GenEvent),
    Shortcut(Shortcut),
    Resized(Size),
    Tick(Instant),
    /// Close the popup if it is still this window (a delayed hide must not close a newer one).
    Hide(window::Id),
    Engine(Result<(), String>),
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
            watch,
            capturing: false,
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
        let engine = self.engine.clone();
        Task::perform(async move { engine.warm_up().await.map_err(|e| format!("{e:#}")) }, Message::Engine)
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subs = vec![
            Subscription::run(ipc_stream).map(|(req, reply)| Message::Ipc(req, reply)),
            event::listen_with(shortcut),
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
            Message::Captured { text, error } => self.captured(text, error),
            Message::Input(s) => self.on_input(s),
            Message::Edit(action) => {
                if let Some(p) = self.popup.as_mut().filter(|p| p.phase() == Phase::Composing) {
                    p.source.perform(action);
                }
                Task::none()
            }
            Message::Submit => self.submit(),
            Message::Preset(i) => self.run_preset(i),
            Message::Gen(id, ev) => self.on_gen(id, ev),
            Message::Shortcut(s) => self.on_shortcut(s),
            Message::Resized(size) => self.on_resize(size),
            Message::Tick(now) => {
                self.now = now;
                Task::none()
            }
            Message::Hide(id) => {
                if self.popup.as_ref().is_some_and(|p| p.id == id) {
                    self.hide()
                } else {
                    Task::none()
                }
            }
            Message::Engine(r) => {
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
            Request::Toggle { .. } if self.popup.is_some() => {
                reply.send(Response::Ok);
                self.hide()
            }
            Request::Toggle { .. } if self.capturing => {
                // Pressed again while the clipboard is still being read: cancel.
                reply.send(Response::Ok);
                self.capturing = false;
                Task::none()
            }
            Request::Toggle { input } | Request::Show { input } => {
                reply.send(Response::Ok);
                if self.popup.is_some() || self.capturing {
                    return Task::none();
                }
                match input {
                    Input::Text(t) => self.open(t, Origin::Editor, None),
                    Input::Clipboard if self.clipboard_recent() => {
                        self.capturing = true;
                        Task::perform(read_clipboard(), |(text, error)| Message::Captured { text, error })
                    }
                    Input::Clipboard => self.open(String::new(), Origin::Typed, None),
                }
            }
            Request::Hide => {
                reply.send(Response::Ok);
                self.hide()
            }
            Request::Reload => match Config::load(&paths::config_file()) {
                Ok(config) => {
                    reply.send(Response::Ok);
                    self.presets = config.presets();
                    self.active = models::active(&config);
                    self.palette = style::Palette::new(&config.ui);
                    self.config = config.clone();
                    let engine = self.engine.clone();
                    let reload =
                        Task::perform(async move { engine.reload(config).await }, |()| Message::Tick(Instant::now()));
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

    fn captured(&mut self, text: Option<String>, error: Option<String>) -> Task<Message> {
        if !std::mem::take(&mut self.capturing) || self.popup.is_some() {
            return Task::none(); // cancelled by a second toggle
        }
        match text {
            Some(t) => self.open(t, Origin::Clipboard, error),
            _ => self.open(String::new(), Origin::Typed, error),
        }
    }

    fn open(&mut self, text: String, origin: Origin, error: Option<String>) -> Task<Message> {
        let id = window::Id::unique();
        let size = self.surface_size(196);
        let top = i32::try_from(self.config.ui.top_margin.saturating_sub(self.margin())).unwrap_or(i32::MAX);
        self.now = Instant::now();
        self.popup = Some(Popup {
            id,
            field: if text.trim().is_empty() { Field::Source } else { Field::Instruction },
            source: text_editor::Content::with_text(&text),
            original: text,
            origin,
            input: String::new(),
            drafts: Vec::new(),
            generation: None,
            show_diff: false,
            copied: false,
            error,
            focused: false,
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
        if p.generation.is_some() || p.copied {
            return Task::none();
        }
        // A lone digit typed into an empty box picks a preset.
        if p.input.is_empty()
            && let Some(d) = single_digit(&s)
            && d >= 1
            && (d as usize) <= self.presets.len()
        {
            return self.run_preset(d as usize - 1);
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
            Phase::Generating | Phase::Copied => Task::none(),
        }
    }

    fn run_preset(&mut self, i: usize) -> Task<Message> {
        let Some(preset) = self.presets.get(i).cloned() else { return Task::none() };
        if self.popup.as_ref().is_none_or(|p| p.generation.is_some() || p.copied) {
            return Task::none();
        }
        self.generate(preset.name, preset.instruction, preset.temperature.unwrap_or(0.7), preset.show_diff)
    }

    fn generate(&mut self, label: String, instruction: String, temperature: f32, show_diff: bool) -> Task<Message> {
        let context = self.config.model.context;
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
        if !prompt::fits(&text, context) {
            p.error = Some(format!(
                "Too long for the model's {context}-token context ({} chars). Select less, or raise `model.context`.",
                text.chars().count()
            ));
            return Task::none();
        }
        let request =
            Rewrite { instruction, max_tokens: prompt::max_tokens(&text, context), text, temperature, sampling };
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
        task
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
                p.generation = None;
                p.error = Some(e);
            }
            GenEvent::Done => {
                let g = p.generation.take().expect("matched above");
                let text = clean::clean(&g.raw, &g.request.text, true);
                if text.trim().is_empty() {
                    p.error = Some("The model returned nothing. Try again (Ctrl+R) or another preset.".into());
                    return Task::none();
                }
                let secs = g.started.elapsed().as_secs_f64();
                let gen_secs = g.first.map_or(secs, |f| f.elapsed().as_secs_f64()).max(1e-3);
                let rate = f64::from(u32::try_from(g.deltas.saturating_sub(1)).unwrap_or(u32::MAX)) / gen_secs;
                p.show_diff = g.show_diff;
                if g.replace {
                    p.drafts.pop();
                }
                p.drafts.push(Draft {
                    text,
                    label: g.label,
                    stats: format!("{secs:.1}s · {rate:.0} tok/s"),
                    show_diff: g.show_diff,
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
            (Shortcut::Tab, Phase::Composing) => {
                p.field = match p.field {
                    Field::Instruction => Field::Source,
                    Field::Source => Field::Instruction,
                };
                focus(p.field)
            }
            (Shortcut::Tab, Phase::Reviewing) => {
                p.show_diff = !p.show_diff;
                Task::none()
            }
            (Shortcut::Retry, Phase::Reviewing) => {
                let d = p.drafts.last().expect("reviewing has a draft");
                let mut request = d.request.clone();
                request.temperature = (request.temperature + 0.3).min(1.2);
                let (label, show_diff) = (d.label.clone(), d.show_diff);
                self.start(label, request, show_diff, true)
            }
            (Shortcut::Undo, Phase::Reviewing) => {
                p.drafts.pop();
                p.show_diff = p.drafts.last().is_some_and(|d| d.show_diff);
                Task::none()
            }
            (Shortcut::Copy, Phase::Reviewing) => self.copy(),
            _ => Task::none(),
        }
    }

    fn copy(&mut self) -> Task<Message> {
        let Some(p) = self.popup.as_mut() else { return Task::none() };
        let Some(d) = p.drafts.last() else { return Task::none() };
        match quillway_wl::copy(&d.text) {
            Ok(()) => {
                p.copied = true;
                let id = p.id;
                Task::perform(tokio::time::sleep(COPIED_LINGER), move |()| Message::Hide(id))
            }
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

fn single_digit(s: &str) -> Option<u32> {
    let mut chars = s.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    c.to_digit(10)
}

fn shortcut(event: Event, _status: event::Status, _window: window::Id) -> Option<Message> {
    let Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) = event else { return None };
    let s = match key.as_ref() {
        Key::Named(Named::Escape) => Shortcut::Escape,
        Key::Named(Named::Tab) => Shortcut::Tab,
        Key::Character("r") if modifiers.control() => Shortcut::Retry,
        Key::Character("z") if modifiers.control() => Shortcut::Undo,
        Key::Character("c") if modifiers.control() => Shortcut::Copy,
        _ => return None,
    };
    Some(Message::Shortcut(s))
}

fn ipc_stream() -> impl Stream<Item = (Request, Reply)> {
    let listener = LISTENER.lock().expect("listener lock").take().expect("IPC subscription started once");
    ipc::serve(listener)
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
