use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Stdout};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender, select, tick, unbounded};
use crossterm::{
    event::{self, Event as TerminalEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use momentum_core::{
    DeviceInfo, Headset, Notification, NotificationFrame, ShowState, Transport,
    gaia::EqConfig,
    state::{AncSubMode, Toggle, WindState},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};

use crate::{args::Cli, commands};

type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

const INPUT_TICK: Duration = Duration::from_millis(50);
const WORKER_TICK: Duration = Duration::from_millis(75);
const WRITE_DEBOUNCE: Duration = Duration::from_millis(150);
const MAX_EVENTS: usize = 200;
const ANC_CONTROL_COUNT: usize = 3;

pub fn run(cli: Cli) -> Result<()> {
    let mut terminal = TerminalSession::enter()?;
    let (command_tx, event_rx) = spawn_worker(cli)?;
    let mut app = App::new();

    let result = run_event_loop(&mut terminal.terminal, &mut app, &command_tx, &event_rx);
    let _ = command_tx.send(WorkerCommand::Quit);
    result
}

struct TerminalSession {
    terminal: TuiTerminal,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        terminal.clear()?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusPanel {
    Anc,
    Transparency,
    Eq,
    Events,
}

impl FocusPanel {
    fn next(self) -> Self {
        match self {
            Self::Anc => Self::Transparency,
            Self::Transparency => Self::Eq,
            Self::Eq => Self::Events,
            Self::Events => Self::Anc,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::Anc => Self::Events,
            Self::Transparency => Self::Anc,
            Self::Eq => Self::Transparency,
            Self::Events => Self::Eq,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Anc => "ANC",
            Self::Transparency => "Transparency",
            Self::Eq => "EQ",
            Self::Events => "Events",
        }
    }
}

#[derive(Debug, Clone)]
enum WorkerCommand {
    Refresh,
    SetAnc(Toggle),
    SetWind(WindState),
    SetAdaptive(bool),
    AdjustTransparency(i16),
    AdjustEqGain { band: u8, delta: i16 },
    Quit,
}

impl WorkerCommand {
    fn label(&self) -> String {
        match self {
            Self::Refresh => "refresh".into(),
            Self::SetAnc(value) => format!("set ANC {value}"),
            Self::SetWind(value) => format!("set wind {value}"),
            Self::SetAdaptive(value) => {
                format!("set adaptive {}", if *value { "on" } else { "off" })
            }
            Self::AdjustTransparency(delta) => format!("transparency {delta:+}"),
            Self::AdjustEqGain { band, delta } => format!("EQ band {} {delta:+}", band + 1),
            Self::Quit => "quit".into(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum PendingScope {
    None,
    State,
    Transparency,
    Eq,
    All,
}

fn pending_scope(command: &WorkerCommand) -> PendingScope {
    match command {
        WorkerCommand::Refresh => PendingScope::All,
        WorkerCommand::SetAnc(_) | WorkerCommand::SetWind(_) | WorkerCommand::SetAdaptive(_) => {
            PendingScope::State
        }
        WorkerCommand::AdjustTransparency(_) => PendingScope::Transparency,
        WorkerCommand::AdjustEqGain { .. } => PendingScope::Eq,
        WorkerCommand::Quit => PendingScope::None,
    }
}

#[derive(Debug, Default)]
struct PendingWrites {
    transparency_delta: i16,
    eq_deltas: BTreeMap<u8, i16>,
    flush_at: Option<Instant>,
}

impl PendingWrites {
    fn adjust_transparency(&mut self, delta: i16) {
        self.transparency_delta = self.transparency_delta.saturating_add(delta);
        self.reschedule();
    }

    fn adjust_eq_gain(&mut self, band: u8, delta: i16) {
        let next = self
            .eq_deltas
            .get(&band)
            .copied()
            .unwrap_or_default()
            .saturating_add(delta);
        if next == 0 {
            self.eq_deltas.remove(&band);
        } else {
            self.eq_deltas.insert(band, next);
        }
        self.reschedule();
    }

    fn flush_if_ready(
        &mut self,
        headset: &mut Headset<Box<dyn Transport>>,
        event_tx: &Sender<WorkerEvent>,
    ) {
        if self.flush_at.is_some_and(|at| Instant::now() >= at) {
            self.flush(headset, event_tx);
        }
    }

    fn flush(&mut self, headset: &mut Headset<Box<dyn Transport>>, event_tx: &Sender<WorkerEvent>) {
        self.flush_at = None;
        let transparency_delta = std::mem::take(&mut self.transparency_delta);
        let eq_deltas = std::mem::take(&mut self.eq_deltas);

        if transparency_delta != 0
            && apply_transparency_delta(headset, transparency_delta, event_tx)
        {
            refresh_state(headset, event_tx);
        }
        if !eq_deltas.is_empty() && apply_eq_deltas(headset, eq_deltas, event_tx) {
            refresh_eq(headset, event_tx);
        }
    }

    fn reschedule(&mut self) {
        self.flush_at = if self.transparency_delta == 0 && self.eq_deltas.is_empty() {
            None
        } else {
            Some(Instant::now() + WRITE_DEBOUNCE)
        };
    }
}

#[derive(Debug)]
enum WorkerEvent {
    Connection(String),
    Message(String),
    Error(String),
    Snapshot(ShowState),
    Info(DeviceInfo),
    Eq { config: EqConfig, gains: Vec<i8> },
    Notification(Notification),
    Closed,
}

#[derive(Debug, Clone, Copy)]
enum EventLevel {
    Info,
    Error,
}

#[derive(Debug)]
struct EventLine {
    level: EventLevel,
    text: String,
}

#[derive(Debug, Default)]
struct UiPending {
    anc: bool,
    transparency: bool,
    eq: bool,
}

impl UiPending {
    fn any(&self) -> bool {
        self.anc || self.transparency || self.eq
    }

    fn clear_all(&mut self) {
        self.anc = false;
        self.transparency = false;
        self.eq = false;
    }
}

#[derive(Debug)]
struct App {
    connection: String,
    state: Option<ShowState>,
    info: Option<DeviceInfo>,
    eq_config: Option<EqConfig>,
    eq_gains: Vec<i8>,
    focus: FocusPanel,
    anc_row: usize,
    selected_eq_band: usize,
    pending: UiPending,
    pending_label: Option<String>,
    last_error: Option<String>,
    last_sync: Option<Instant>,
    show_help: bool,
    events: VecDeque<EventLine>,
}

impl App {
    fn new() -> Self {
        let mut app = Self {
            connection: "starting".into(),
            state: None,
            info: None,
            eq_config: None,
            eq_gains: Vec::new(),
            focus: FocusPanel::Anc,
            anc_row: 0,
            selected_eq_band: 0,
            pending: UiPending::default(),
            pending_label: None,
            last_error: None,
            last_sync: None,
            show_help: false,
            events: VecDeque::new(),
        };
        app.push_info("starting terminal UI");
        app
    }

    fn mark_pending(&mut self, scope: PendingScope, label: String) {
        match scope {
            PendingScope::None => return,
            PendingScope::State => self.pending.anc = true,
            PendingScope::Transparency => {
                self.pending.anc = true;
                self.pending.transparency = true;
            }
            PendingScope::Eq => self.pending.eq = true,
            PendingScope::All => {
                self.pending.anc = true;
                self.pending.transparency = true;
                self.pending.eq = true;
            }
        }
        self.pending_label = Some(label);
    }

    fn clear_pending_label_if_done(&mut self) {
        if !self.pending.any() {
            self.pending_label = None;
        }
    }

    fn apply_worker_event(&mut self, event: WorkerEvent) {
        match event {
            WorkerEvent::Connection(status) => {
                self.connection = status.clone();
                self.push_info(status);
            }
            WorkerEvent::Message(message) => self.push_info(message),
            WorkerEvent::Error(error) => {
                self.last_error = Some(error.clone());
                self.pending.clear_all();
                self.pending_label = None;
                self.push_error(error);
            }
            WorkerEvent::Snapshot(state) => {
                self.state = Some(state);
                self.last_error = None;
                self.last_sync = Some(Instant::now());
                self.pending.anc = false;
                self.pending.transparency = false;
                self.clear_pending_label_if_done();
            }
            WorkerEvent::Info(info) => {
                self.info = Some(info);
            }
            WorkerEvent::Eq { config, gains } => {
                self.eq_config = Some(config);
                self.eq_gains = gains;
                if !self.eq_gains.is_empty() {
                    self.selected_eq_band = self.selected_eq_band.min(self.eq_gains.len() - 1);
                }
                self.pending.eq = false;
                self.clear_pending_label_if_done();
            }
            WorkerEvent::Notification(notification) => {
                self.apply_notification(&notification);
                self.push_info(notification_summary(&notification));
            }
            WorkerEvent::Closed => {
                self.connection = "worker stopped".into();
                self.push_error("headset worker stopped");
            }
        }
    }

    fn apply_notification(&mut self, notification: &Notification) {
        match notification {
            Notification::Battery { percent } => {
                if let Some(state) = &mut self.state {
                    state.battery = *percent;
                }
            }
            Notification::Charging { status } => {
                if let Some(state) = &mut self.state {
                    state.charging = *status;
                }
            }
            Notification::AncStatus { state: anc } if !self.pending.anc => {
                if let Some(state) = &mut self.state {
                    state.noise = *anc;
                }
            }
            Notification::AncModes { modes } if !self.pending.anc => {
                if let Some(state) = &mut self.state {
                    state.anc_modes = *modes;
                }
            }
            Notification::AncTransparencyLevel { level } if !self.pending.transparency => {
                if let Some(state) = &mut self.state {
                    state.transparency = *level;
                }
            }
            Notification::EqGain { gains_centibel } if !self.pending.eq => {
                self.eq_gains = gains_centibel.clone();
                if !self.eq_gains.is_empty() {
                    self.selected_eq_band = self.selected_eq_band.min(self.eq_gains.len() - 1);
                }
            }
            _ => {}
        }
    }

    fn push_info(&mut self, text: impl Into<String>) {
        self.push_event(EventLevel::Info, text);
    }

    fn push_error(&mut self, text: impl Into<String>) {
        self.push_event(EventLevel::Error, text);
    }

    fn push_event(&mut self, level: EventLevel, text: impl Into<String>) {
        if self.events.len() == MAX_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(EventLine {
            level,
            text: text.into(),
        });
    }
}

fn run_event_loop(
    terminal: &mut TuiTerminal,
    app: &mut App,
    command_tx: &Sender<WorkerCommand>,
    event_rx: &Receiver<WorkerEvent>,
) -> Result<()> {
    loop {
        drain_worker_events(app, event_rx);
        terminal.draw(|frame| draw(frame, app))?;

        if event::poll(INPUT_TICK)?
            && let TerminalEvent::Key(key) = event::read()?
            && handle_key(key, app, command_tx)
        {
            return Ok(());
        }
    }
}

fn drain_worker_events(app: &mut App, event_rx: &Receiver<WorkerEvent>) {
    while let Ok(event) = event_rx.try_recv() {
        app.apply_worker_event(event);
    }
}

fn handle_key(key: KeyEvent, app: &mut App, command_tx: &Sender<WorkerCommand>) -> bool {
    if key.kind != KeyEventKind::Press {
        return false;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return true;
    }
    if app.show_help {
        return handle_help_key(key, app);
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => true,
        KeyCode::Char('?') => {
            app.show_help = true;
            false
        }
        KeyCode::Tab => {
            app.focus = app.focus.next();
            false
        }
        KeyCode::BackTab => {
            app.focus = app.focus.prev();
            false
        }
        KeyCode::Char('r') => {
            send_action(app, command_tx, WorkerCommand::Refresh);
            false
        }
        KeyCode::Char('a') => {
            toggle_anc_master(app, command_tx);
            false
        }
        KeyCode::Char('w') => {
            cycle_wind(app, command_tx);
            false
        }
        KeyCode::Char('d') => {
            toggle_adaptive(app, command_tx);
            false
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            activate_focused_control(app, command_tx);
            false
        }
        KeyCode::Up | KeyCode::Char('k') => {
            move_or_adjust(app, command_tx, DirectionKey::Up);
            false
        }
        KeyCode::Down | KeyCode::Char('j') => {
            move_or_adjust(app, command_tx, DirectionKey::Down);
            false
        }
        KeyCode::Left | KeyCode::Char('h') => {
            move_or_adjust(app, command_tx, DirectionKey::Left);
            false
        }
        KeyCode::Right | KeyCode::Char('l') => {
            move_or_adjust(app, command_tx, DirectionKey::Right);
            false
        }
        KeyCode::Char('+') | KeyCode::Char('=') => {
            if app.focus == FocusPanel::Eq {
                adjust_eq_gain(app, command_tx, 1);
            } else {
                adjust_transparency(app, command_tx, 5);
            }
            false
        }
        KeyCode::Char('-') => {
            if app.focus == FocusPanel::Eq {
                adjust_eq_gain(app, command_tx, -1);
            } else {
                adjust_transparency(app, command_tx, -5);
            }
            false
        }
        _ => false,
    }
}

fn handle_help_key(key: KeyEvent, app: &mut App) -> bool {
    match key.code {
        KeyCode::Esc | KeyCode::Char('?') | KeyCode::Enter | KeyCode::Char(' ') => {
            app.show_help = false;
            false
        }
        KeyCode::Char('q') => true,
        _ => false,
    }
}

enum DirectionKey {
    Up,
    Down,
    Left,
    Right,
}

fn move_or_adjust(app: &mut App, command_tx: &Sender<WorkerCommand>, direction: DirectionKey) {
    match (app.focus, direction) {
        (FocusPanel::Anc, DirectionKey::Up) | (FocusPanel::Anc, DirectionKey::Left) => {
            app.anc_row = app.anc_row.saturating_sub(1);
        }
        (FocusPanel::Anc, DirectionKey::Down) | (FocusPanel::Anc, DirectionKey::Right) => {
            app.anc_row = (app.anc_row + 1).min(ANC_CONTROL_COUNT - 1);
        }
        (FocusPanel::Transparency, DirectionKey::Up)
        | (FocusPanel::Transparency, DirectionKey::Right) => {
            adjust_transparency(app, command_tx, 5)
        }
        (FocusPanel::Transparency, DirectionKey::Down)
        | (FocusPanel::Transparency, DirectionKey::Left) => {
            adjust_transparency(app, command_tx, -5)
        }
        (FocusPanel::Eq, DirectionKey::Up) => adjust_eq_gain(app, command_tx, 1),
        (FocusPanel::Eq, DirectionKey::Down) => adjust_eq_gain(app, command_tx, -1),
        (FocusPanel::Eq, DirectionKey::Left) => select_eq_band(app, -1),
        (FocusPanel::Eq, DirectionKey::Right) => select_eq_band(app, 1),
        (FocusPanel::Events, _) => {}
    }
}

fn activate_focused_control(app: &mut App, command_tx: &Sender<WorkerCommand>) {
    match app.focus {
        FocusPanel::Anc => match app.anc_row {
            0 => toggle_anc_master(app, command_tx),
            1 => cycle_wind(app, command_tx),
            2 => toggle_adaptive(app, command_tx),
            _ => {}
        },
        FocusPanel::Transparency => adjust_transparency(app, command_tx, 5),
        FocusPanel::Eq => adjust_eq_gain(app, command_tx, 1),
        FocusPanel::Events => {}
    }
}

fn send_action(app: &mut App, command_tx: &Sender<WorkerCommand>, command: WorkerCommand) {
    let label = command.label();
    let scope = pending_scope(&command);
    if command_tx.send(command).is_err() {
        app.push_error("headset worker is not running");
    } else {
        app.mark_pending(scope, label.clone());
        app.push_info(format!("queued {label}"));
    }
}

fn toggle_anc_master(app: &mut App, command_tx: &Sender<WorkerCommand>) {
    let Some(state) = app.state.as_mut() else {
        app.push_error("no ANC state yet");
        return;
    };
    let next = match state.noise {
        Toggle::On => Toggle::Off,
        Toggle::Off => Toggle::On,
    };
    state.noise = next;
    send_action(app, command_tx, WorkerCommand::SetAnc(next));
}

fn cycle_wind(app: &mut App, command_tx: &Sender<WorkerCommand>) {
    let Some(state) = app.state.as_mut() else {
        app.push_error("no wind state yet");
        return;
    };
    let next = match state.anc_modes.wind {
        WindState::Off => WindState::On,
        WindState::On => WindState::Auto,
        WindState::Auto | WindState::Unknown(_) => WindState::Off,
    };
    state.anc_modes.wind = next;
    send_action(app, command_tx, WorkerCommand::SetWind(next));
}

fn toggle_adaptive(app: &mut App, command_tx: &Sender<WorkerCommand>) {
    let Some(state) = app.state.as_mut() else {
        app.push_error("no adaptive ANC state yet");
        return;
    };
    let next = !state.anc_modes.adaptive;
    state.anc_modes.adaptive = next;
    if next {
        state.transparency = 0;
    }
    send_action(app, command_tx, WorkerCommand::SetAdaptive(next));
}

fn adjust_transparency(app: &mut App, command_tx: &Sender<WorkerCommand>, delta: i16) {
    let command_delta = {
        let Some(state) = app.state.as_mut() else {
            app.push_error("no transparency state yet");
            return;
        };
        let current = state.transparency;
        let next = (i16::from(current) + delta).clamp(0, 100) as u8;
        if next == current {
            return;
        }
        state.transparency = next;
        if next > 0 {
            state.anc_modes.adaptive = false;
        }
        i16::from(next) - i16::from(current)
    };
    send_action(
        app,
        command_tx,
        WorkerCommand::AdjustTransparency(command_delta),
    );
}

fn select_eq_band(app: &mut App, delta: i16) {
    if app.eq_gains.is_empty() {
        return;
    }
    let max = app.eq_gains.len() as i16 - 1;
    app.selected_eq_band = (app.selected_eq_band as i16 + delta).clamp(0, max) as usize;
}

fn adjust_eq_gain(app: &mut App, command_tx: &Sender<WorkerCommand>, delta: i16) {
    let Some(config) = app.eq_config else {
        app.push_error("no EQ config yet");
        return;
    };
    if app.eq_gains.is_empty() {
        app.push_error("no EQ bands yet");
        return;
    }
    let band = app.selected_eq_band.min(app.eq_gains.len() - 1);
    let current = app.eq_gains[band];
    let next =
        (i16::from(current) + delta).clamp(config.min_gain.into(), config.max_gain.into()) as i8;
    if next == current {
        return;
    }
    app.eq_gains[band] = next;
    send_action(
        app,
        command_tx,
        WorkerCommand::AdjustEqGain {
            band: band as u8,
            delta: i16::from(next) - i16::from(current),
        },
    );
}

fn draw(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    let event_height = if area.height >= 30 { 8 } else { 6 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(event_height),
            Constraint::Length(3),
        ])
        .split(area);

    draw_header(frame, chunks[0], app);
    draw_main(frame, chunks[1], app);
    draw_events(frame, chunks[2], app);
    draw_footer(frame, chunks[3], app);

    if app.show_help {
        draw_help_overlay(frame, area);
    }
}

fn draw_main(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if area.width >= 104 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(43), Constraint::Percentage(57)])
            .split(area);
        let left = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(10), Constraint::Min(7)])
            .split(columns[0]);
        draw_anc(frame, left[0], app);
        draw_transparency(frame, left[1], app);
        draw_eq(frame, columns[1], app);
    } else {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(9),
                Constraint::Length(7),
                Constraint::Min(8),
            ])
            .split(area);
        draw_anc(frame, rows[0], app);
        draw_transparency(frame, rows[1], app);
        draw_eq(frame, rows[2], app);
    }
}

fn draw_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let model = app
        .info
        .as_ref()
        .map(|info| info.model.as_str())
        .unwrap_or("MOMENTUM 4");
    let release = app
        .info
        .as_ref()
        .map(|info| info.release.as_str())
        .unwrap_or("firmware ?");
    let battery = app
        .state
        .as_ref()
        .map(|state| format!("{}%", state.battery))
        .unwrap_or_else(|| "--%".into());
    let sync = app
        .last_sync
        .map(format_elapsed)
        .unwrap_or_else(|| "never".into());

    let pending = app
        .pending_label
        .as_ref()
        .map(|label| format!(" | pending {label}"))
        .unwrap_or_default();

    let line = Line::from(vec![
        Span::styled(
            format!(" {model} "),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(" {release} | battery {battery} | sync {sync} | ")),
        Span::styled(
            app.connection.as_str(),
            Style::default().fg(connection_color(&app.connection)),
        ),
        Span::styled(pending, Style::default().fg(Color::Yellow)),
    ]);
    frame.render_widget(
        Paragraph::new(line)
            .alignment(Alignment::Center)
            .block(panel_block(" Control ", false)),
        area,
    );
}

fn draw_anc(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let focused = app.focus == FocusPanel::Anc;
    let mut lines = Vec::new();
    if let Some(state) = &app.state {
        lines.push(control_line(
            focused && app.anc_row == 0,
            "master",
            state.noise.to_string(),
            "space/a toggle",
            app.pending.anc,
        ));
        lines.push(control_line(
            focused && app.anc_row == 1,
            "wind",
            state.anc_modes.wind.to_string(),
            "space/w cycle",
            app.pending.anc,
        ));
        lines.push(control_line(
            focused && app.anc_row == 2,
            "adaptive",
            if state.anc_modes.adaptive {
                "on"
            } else {
                "off"
            }
            .to_string(),
            "space/d toggle",
            app.pending.anc,
        ));
        if state.anc_modes.adaptive {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "adaptive clamps transparency to 0",
                Style::default().fg(Color::Yellow),
            )));
        }
    } else {
        lines.push(Line::from("waiting for headset state..."));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(panel_block(" ANC ", focused))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_transparency(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let focused = app.focus == FocusPanel::Transparency;
    let mut lines = Vec::new();
    if let Some(state) = &app.state {
        lines.push(Line::from(Span::styled(
            transparency_bar(state.transparency, area.width.saturating_sub(18) as usize),
            Style::default().fg(if app.pending.transparency {
                Color::Yellow
            } else {
                Color::Green
            }),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::raw("level "),
            Span::styled(
                format!("{}", state.transparency),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw("  +/- or h/l adjusts by 5"),
        ]));
        if state.anc_modes.adaptive {
            lines.push(Line::from(Span::styled(
                "raising transparency will disable adaptive ANC",
                Style::default().fg(Color::Yellow),
            )));
        }
    } else {
        lines.push(Line::from("waiting for transparency..."));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel_block(" Transparency ", focused))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_eq(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let focused = app.focus == FocusPanel::Eq;
    let mut lines = Vec::new();
    if let Some(config) = app.eq_config {
        lines.push(Line::from(vec![
            Span::raw("range "),
            Span::styled(
                format!("{}..{}", config.min_gain, config.max_gain),
                Style::default().fg(Color::Gray),
            ),
            Span::raw("  h/l band  j/k gain"),
        ]));
        lines.push(Line::from(""));
        if app.eq_gains.is_empty() {
            lines.push(Line::from("waiting for EQ bands..."));
        } else {
            let bar_width = area.width.saturating_sub(25).clamp(12, 36) as usize;
            for (idx, gain) in app.eq_gains.iter().enumerate() {
                let selected = focused && idx == app.selected_eq_band;
                let marker = if selected { ">" } else { " " };
                let style = if selected {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else if app.pending.eq {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default()
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("{marker} band {:>2} ", idx + 1), style),
                    Span::styled(
                        eq_bar(*gain, config.min_gain, config.max_gain, bar_width),
                        style,
                    ),
                    Span::styled(format!(" {gain:+}"), style),
                ]));
            }
        }
    } else {
        lines.push(Line::from("waiting for EQ config..."));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel_block(" Equalizer ", focused))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_events(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let focused = app.focus == FocusPanel::Events;
    let visible = area.height.saturating_sub(2) as usize;
    let start = app.events.len().saturating_sub(visible);
    let items = app.events.iter().skip(start).map(|event| {
        let (tag, style) = match event.level {
            EventLevel::Info => ("info", Style::default().fg(Color::Gray)),
            EventLevel::Error => ("err ", Style::default().fg(Color::Red)),
        };
        ListItem::new(Line::from(vec![
            Span::styled(format!("{tag} "), style.add_modifier(Modifier::BOLD)),
            Span::styled(event.text.clone(), style),
        ]))
    });
    frame.render_widget(
        List::new(items).block(panel_block(" Events ", focused)),
        area,
    );
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let focus_help = match app.focus {
        FocusPanel::Anc => "space toggle/cycle | up/down choose ANC row | a/w/d direct",
        FocusPanel::Transparency => "+/- or h/l adjust transparency by 5",
        FocusPanel::Eq => "h/l choose band | j/k or +/- adjust gain",
        FocusPanel::Events => "event feed follows latest messages",
    };
    let line = Line::from(vec![
        Span::styled("tab", Style::default().fg(Color::Yellow)),
        Span::raw(format!(" focus ({})  ", app.focus.label())),
        Span::styled("r", Style::default().fg(Color::Yellow)),
        Span::raw(" refresh  "),
        Span::styled("?", Style::default().fg(Color::Yellow)),
        Span::raw(" help  "),
        Span::styled("q", Style::default().fg(Color::Yellow)),
        Span::raw(" quit  |  "),
        Span::raw(focus_help),
    ]);
    frame.render_widget(
        Paragraph::new(line)
            .alignment(Alignment::Center)
            .block(panel_block(" Keys ", false)),
        area,
    );
}

fn draw_help_overlay(frame: &mut Frame<'_>, area: Rect) {
    let popup = centered_rect(72, 70, area);
    let lines = vec![
        Line::from(Span::styled(
            "MOMENTUM TUI",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("tab / shift-tab       move focus between panels"),
        Line::from("enter / space         activate the focused control"),
        Line::from("a / w / d             toggle ANC, cycle wind, toggle adaptive"),
        Line::from("+ / -                 adjust transparency, or EQ if EQ is focused"),
        Line::from("h/l or left/right     move/adjust in focused panel"),
        Line::from("j/k or up/down        move/adjust in focused panel"),
        Line::from("r                     refresh all state from headset"),
        Line::from("q / esc               quit"),
        Line::from(""),
        Line::from("Transparency above 0 disables adaptive ANC because the headset firmware"),
        Line::from("otherwise clamps transparency back to 0."),
        Line::from(""),
        Line::from(Span::styled(
            "Press ? or Esc to close help.",
            Style::default().fg(Color::Yellow),
        )),
    ];
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Left)
            .block(panel_block(" Help ", true))
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn panel_block(title: impl Into<String>, focused: bool) -> Block<'static> {
    let border = if focused {
        Color::Yellow
    } else {
        Color::DarkGray
    };
    Block::default()
        .title(title.into())
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
}

fn control_line(
    selected: bool,
    label: &str,
    value: String,
    hint: &str,
    pending: bool,
) -> Line<'static> {
    let marker = if selected { ">" } else { " " };
    let value_style = if pending {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Green)
    };
    let marker_style = if selected {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Line::from(vec![
        Span::styled(format!("{marker} "), marker_style),
        Span::raw(format!("{label:<11} ")),
        Span::styled(format!("{value:<8}"), value_style),
        Span::styled(hint.to_string(), Style::default().fg(Color::DarkGray)),
    ])
}

fn transparency_bar(level: u8, width: usize) -> String {
    let width = width.clamp(10, 40);
    let filled = (usize::from(level) * width + 50) / 100;
    format!(
        "[{}{}] {:>3}%",
        "#".repeat(filled),
        ".".repeat(width - filled),
        level
    )
}

fn eq_bar(gain: i8, min: i8, max: i8, width: usize) -> String {
    let width = width.max(7);
    let min = i16::from(min);
    let max = i16::from(max);
    let gain = i16::from(gain).clamp(min, max);
    let range = (max - min).max(1);
    let to_pos = |value: i16| -> usize {
        (((value - min) * (width as i16 - 1)) / range).clamp(0, width as i16 - 1) as usize
    };
    let gain_pos = to_pos(gain);
    let zero_pos = to_pos(0.clamp(min, max));
    let mut chars = vec!['.'; width];
    chars[zero_pos] = '|';
    let (start, end) = if gain_pos <= zero_pos {
        (gain_pos, zero_pos)
    } else {
        (zero_pos, gain_pos)
    };
    for ch in chars.iter_mut().take(end + 1).skip(start) {
        *ch = '#';
    }
    chars[zero_pos] = '|';
    chars[gain_pos] = '*';
    format!("[{}]", chars.into_iter().collect::<String>())
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1]);
    horizontal[1]
}

fn connection_color(connection: &str) -> Color {
    if connection.contains("connected") {
        Color::Green
    } else if connection.contains("stopped") || connection.contains("error") {
        Color::Red
    } else {
        Color::Yellow
    }
}

fn format_elapsed(at: Instant) -> String {
    let secs = at.elapsed().as_secs();
    if secs < 60 {
        format!("{secs}s ago")
    } else {
        format!("{}m ago", secs / 60)
    }
}

fn notification_summary(notification: &Notification) -> String {
    match notification {
        Notification::Battery { percent } => format!("battery {percent}%"),
        Notification::Charging { status } => format!("charging {status}"),
        Notification::AncStatus { state } => format!("ANC {state}"),
        Notification::AncModes { modes } => format!("ANC modes {modes}"),
        Notification::AncTransparencyLevel { level } => format!("transparency {level}"),
        Notification::EqGain { gains_centibel } => format!("EQ gains {gains_centibel:?}"),
        Notification::WearState { state } => format!("wear state {state}"),
        Notification::Preset { preset } => format!("preset {preset}"),
        Notification::LowLatency { state } => format!("low latency {state}"),
        Notification::SampleRate { hz } => format!("sample rate {hz} Hz"),
        Notification::Codec { codec } => format!("codec {codec}"),
        Notification::ConnectionChanged {
            device_index,
            status,
        } => format!("connection changed index={device_index} status={status}"),
        Notification::Unknown { vendor, opcode, .. } => {
            format!("unknown vendor={vendor:#06x} opcode={opcode:#06x}")
        }
        other => format!("{other:?}"),
    }
}

fn spawn_worker(cli: Cli) -> Result<(Sender<WorkerCommand>, Receiver<WorkerEvent>)> {
    let (command_tx, command_rx) = unbounded();
    let (event_tx, event_rx) = unbounded();
    thread::Builder::new()
        .name("momentum-tui-worker".into())
        .spawn({
            let event_tx = event_tx.clone();
            move || {
                if let Err(error) = worker_loop(cli, command_rx, event_tx.clone()) {
                    let _ = event_tx.send(WorkerEvent::Error(format!("{error:#}")));
                }
                let _ = event_tx.send(WorkerEvent::Closed);
            }
        })?;
    Ok((command_tx, event_rx))
}

fn worker_loop(
    cli: Cli,
    command_rx: Receiver<WorkerCommand>,
    event_tx: Sender<WorkerEvent>,
) -> Result<()> {
    send_event(&event_tx, WorkerEvent::Connection("connecting".into()));
    let mut headset = commands::open_headset(&cli)?;
    send_event(&event_tx, WorkerEvent::Connection("connected".into()));

    refresh_state(&mut headset, &event_tx);
    refresh_eq(&mut headset, &event_tx);
    refresh_info(&mut headset, &event_tx);

    let notifications = headset.notifications();
    for (vendor, feature, error) in headset.subscribe_all() {
        send_event(
            &event_tx,
            WorkerEvent::Error(format!(
                "notification registration failed vendor={vendor:#06x} feature={feature}: {error}"
            )),
        );
    }

    let ticker = tick(WORKER_TICK);
    let mut pending = PendingWrites::default();
    loop {
        select! {
            recv(command_rx) -> command => match command {
                Ok(WorkerCommand::Quit) => {
                    pending.flush(&mut headset, &event_tx);
                    break;
                }
                Err(_) => {
                    pending.flush(&mut headset, &event_tx);
                    break;
                }
                Ok(WorkerCommand::AdjustTransparency(delta)) => {
                    pending.adjust_transparency(delta);
                }
                Ok(WorkerCommand::AdjustEqGain { band, delta }) => {
                    pending.adjust_eq_gain(band, delta);
                }
                Ok(command) => {
                    pending.flush(&mut headset, &event_tx);
                    handle_worker_command(&mut headset, command, &event_tx);
                }
            },
            recv(ticker) -> _ => {
                if let Some(notifications) = &notifications {
                    drain_notifications(notifications, &event_tx);
                }
                pending.flush_if_ready(&mut headset, &event_tx);
            }
        }
    }
    Ok(())
}

fn handle_worker_command(
    headset: &mut Headset<Box<dyn Transport>>,
    command: WorkerCommand,
    event_tx: &Sender<WorkerEvent>,
) {
    match command {
        WorkerCommand::Refresh => {
            send_event(event_tx, WorkerEvent::Connection("refreshing".into()));
            refresh_state(headset, event_tx);
            refresh_eq(headset, event_tx);
            refresh_info(headset, event_tx);
            send_event(event_tx, WorkerEvent::Connection("connected".into()));
        }
        WorkerCommand::SetAnc(value) => match headset.set_anc(value) {
            Ok(()) => {
                send_event(event_tx, WorkerEvent::Message(format!("ANC {value}")));
                refresh_state(headset, event_tx);
            }
            Err(error) => send_event(event_tx, WorkerEvent::Error(format!("set ANC: {error}"))),
        },
        WorkerCommand::SetWind(value) => {
            match headset.set_anc_mode(AncSubMode::Wind, value.as_u8()) {
                Ok(()) => {
                    send_event(event_tx, WorkerEvent::Message(format!("wind {value}")));
                    refresh_state(headset, event_tx);
                }
                Err(error) => {
                    send_event(event_tx, WorkerEvent::Error(format!("set wind: {error}")));
                }
            }
        }
        WorkerCommand::SetAdaptive(value) => {
            match headset.set_anc_mode(AncSubMode::Adaptive, u8::from(value)) {
                Ok(()) => {
                    send_event(
                        event_tx,
                        WorkerEvent::Message(format!(
                            "adaptive {}",
                            if value { "on" } else { "off" }
                        )),
                    );
                    refresh_state(headset, event_tx);
                }
                Err(error) => {
                    send_event(
                        event_tx,
                        WorkerEvent::Error(format!("set adaptive: {error}")),
                    );
                }
            }
        }
        WorkerCommand::AdjustTransparency(_) | WorkerCommand::AdjustEqGain { .. } => {}
        WorkerCommand::Quit => {}
    }
}

fn apply_transparency_delta(
    headset: &mut Headset<Box<dyn Transport>>,
    delta: i16,
    event_tx: &Sender<WorkerEvent>,
) -> bool {
    let result = (|| -> momentum_core::Result<Option<u8>> {
        let current = headset.transparency()?;
        let next = (i16::from(current) + delta).clamp(0, 100) as u8;
        if next == current {
            return Ok(None);
        }
        headset.set_transparency(next)?;
        Ok(Some(next))
    })();

    match result {
        Ok(Some(level)) => {
            send_event(
                event_tx,
                WorkerEvent::Message(format!("transparency {level}")),
            );
            true
        }
        Ok(None) => false,
        Err(error) => {
            send_event(
                event_tx,
                WorkerEvent::Error(format!("set transparency: {error}")),
            );
            false
        }
    }
}

fn apply_eq_deltas(
    headset: &mut Headset<Box<dyn Transport>>,
    deltas: BTreeMap<u8, i16>,
    event_tx: &Sender<WorkerEvent>,
) -> bool {
    let config = match headset.eq_config() {
        Ok(config) => config,
        Err(error) => {
            send_event(event_tx, WorkerEvent::Error(format!("set EQ: {error}")));
            return false;
        }
    };

    let mut changed = false;
    for (band, delta) in deltas {
        let result = (|| -> momentum_core::Result<Option<i8>> {
            let current = headset.eq_gain(band)?;
            let next = (i16::from(current) + delta)
                .clamp(config.min_gain.into(), config.max_gain.into()) as i8;
            if next == current {
                return Ok(None);
            }
            headset.set_eq_gain(band, next)?;
            Ok(Some(next))
        })();

        match result {
            Ok(Some(gain)) => {
                changed = true;
                send_event(
                    event_tx,
                    WorkerEvent::Message(format!("EQ band {} {gain}", band + 1)),
                );
            }
            Ok(None) => {}
            Err(error) => {
                send_event(event_tx, WorkerEvent::Error(format!("set EQ: {error}")));
            }
        }
    }

    changed
}

fn refresh_state(headset: &mut Headset<Box<dyn Transport>>, event_tx: &Sender<WorkerEvent>) {
    match headset.show_state() {
        Ok(state) => send_event(event_tx, WorkerEvent::Snapshot(state)),
        Err(error) => send_event(
            event_tx,
            WorkerEvent::Error(format!("refresh state: {error}")),
        ),
    }
}

fn refresh_info(headset: &mut Headset<Box<dyn Transport>>, event_tx: &Sender<WorkerEvent>) {
    match headset.device_info() {
        Ok(info) => send_event(event_tx, WorkerEvent::Info(info)),
        Err(error) => send_event(
            event_tx,
            WorkerEvent::Error(format!("refresh info: {error}")),
        ),
    }
}

fn refresh_eq(headset: &mut Headset<Box<dyn Transport>>, event_tx: &Sender<WorkerEvent>) {
    let result = (|| -> momentum_core::Result<(EqConfig, Vec<i8>)> {
        let config = headset.eq_config()?;
        let gains = (0..config.bands)
            .map(|band| headset.eq_gain(band))
            .collect::<momentum_core::Result<Vec<_>>>()?;
        Ok((config, gains))
    })();

    match result {
        Ok((config, gains)) => send_event(event_tx, WorkerEvent::Eq { config, gains }),
        Err(error) => send_event(event_tx, WorkerEvent::Error(format!("refresh EQ: {error}"))),
    }
}

fn drain_notifications(
    notifications: &Receiver<NotificationFrame>,
    event_tx: &Sender<WorkerEvent>,
) {
    while let Ok(frame) = notifications.try_recv() {
        send_event(
            event_tx,
            WorkerEvent::Notification(Notification::decode(&frame)),
        );
    }
}

fn send_event(event_tx: &Sender<WorkerEvent>, event: WorkerEvent) {
    let _ = event_tx.send(event);
}
