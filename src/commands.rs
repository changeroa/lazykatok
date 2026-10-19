use crate::cli::{Commands, PermissionsCommand, SearchCommand, SourceCommand, WatchOutputFormat};
use crate::commands::source_adapter::adapter_for_source;
use crate::support::{dependency_status, print_payload};
use anyhow::{Context, Result};
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind,
        KeyModifiers,
    },
    execute, queue,
    style::{Attribute, Print, SetAttribute},
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use lazykatok::{
    adapters::ChatSummary,
    archive::Archive,
    chunking::{
        rebuild_chunks_for_chats, rebuild_chunks_with_settings, ChunkSettings, CHUNKER_VERSION,
    },
    config::KatokConfig,
    hybrid::hybrid_search_with_config,
    search::{bm25_search_with_snippet, keyword_search_with_snippet},
    semantic::semantic_search_live_with_config,
    transcript::export_transcript,
    types::{RawMessage, SyncReport, SyncTimings},
    watch::WATCH_EVENT_SCHEMA_VERSION,
    watch::{
        chat_count, format_human_message_line, parse_reply_command, sanitize_terminal_text,
        ChatEntry, ReplyCommand, ReplyRow, ReplyUiState, RowStyle, ScrollDelta, WatchEvent,
        WatchMessageChange,
    },
    watch::{WatchSnapshot, WatchState},
};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(all(target_os = "macos", feature = "private-send"))]
use std::ffi::OsString;
#[cfg(all(target_os = "macos", feature = "private-send"))]
use std::process::{Command as ProcessCommand, Stdio};

mod chunk_commands;
mod freshness;
mod index_commands;
mod media_commands;
mod permissions;
mod source_adapter;

#[derive(Debug, PartialEq, Eq)]
enum ReplyTerminalAction {
    Submit(String),
    Scroll(ScrollDelta),
    Quit,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum ReplyRedraw {
    #[default]
    None,
    Input,
    Full,
}

#[derive(Debug, Default)]
struct ReplyInputBatch {
    actions: Vec<ReplyTerminalAction>,
    redraw: ReplyRedraw,
}

const REPLY_INPUT_BURST_GRACE: Duration = Duration::from_millis(2);
const REPLY_INPUT_BATCH_LIMIT: Duration = Duration::from_millis(8);
const REPLY_INPUT_EVENT_LIMIT: usize = 256;
const REPLY_PUMP_SLICE: Duration = Duration::from_millis(16);
// Reply Enter is explicit intent, so do not inherit the measured 15-second focus-wait default.
const REPLY_FOCUS_WAIT_SECS: u64 = 2;

#[derive(Debug)]
enum ReplyPollOutcome {
    Completed {
        poll: u64,
        events: Vec<WatchEvent>,
        archive_changed: bool,
        observed_messages: usize,
        observed_chats: usize,
        emitted_messages: usize,
        archived_messages: usize,
        chunks: usize,
        selected_chat_name: Option<String>,
    },
    Error {
        poll: u64,
        error: anyhow::Error,
    },
}

struct ReplySendRequest {
    chat_id: String,
    body: String,
    no_open: bool,
    data_dir: PathBuf,
    outcome_tx: Sender<Result<usize>>,
}

struct ReplySendState {
    outcome_tx: Sender<Result<usize>>,
    outcome_rx: Receiver<Result<usize>>,
    in_flight: bool,
}

impl ReplySendState {
    fn new() -> Self {
        let (outcome_tx, outcome_rx) = mpsc::channel();
        Self {
            outcome_tx,
            outcome_rx,
            in_flight: false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct PollArchiveState {
    archive_changed: bool,
    archived_messages: usize,
    chunks: usize,
}

struct ReplyPollWorkerOptions {
    poll_interval: Duration,
    max_polls: Option<u64>,
    replay_existing: bool,
    tail: usize,
    chat_id: Option<String>,
    selected_chat_name: Option<String>,
}

trait ReplyPollRenderer {
    fn render_system(&mut self, line: String) -> Result<()>;
    fn render_chat(&mut self, entry: ChatEntry) -> Result<()>;
}

trait ReplyWatchUi: ReplyPollRenderer {
    fn poll_actions(&mut self, timeout: Duration) -> Result<Option<Vec<ReplyTerminalAction>>>;
    fn scroll(&mut self, delta: ScrollDelta) -> Result<()>;
    fn restore_draft(&mut self, draft: &str) -> Result<()>;
}

/// Apply already-decoded terminal events without performing terminal I/O.
///
/// Keeping this as one batch means callers can drain a rapid key/IME burst and redraw only once.
/// Paste is deliberately its own event path: CR/LF inside bracketed paste is text, while CR/LF
/// delivered as a key event is an explicit submission for terminals that do not use `Enter`.
fn apply_reply_input_events(
    state: &mut ReplyUiState,
    events: impl IntoIterator<Item = Event>,
) -> ReplyInputBatch {
    let mut batch = ReplyInputBatch::default();
    for event in events {
        match event {
            Event::Resize(_, _) => batch.redraw = ReplyRedraw::Full,
            Event::Paste(text) => {
                state.paste(&text);
                if batch.redraw == ReplyRedraw::None {
                    batch.redraw = ReplyRedraw::Input;
                }
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('c' | 'd'))
                {
                    batch.actions.push(ReplyTerminalAction::Quit);
                    break;
                }
                let mut edited = false;
                match key.code {
                    KeyCode::Enter | KeyCode::Char('\r' | '\n') => {
                        batch
                            .actions
                            .push(ReplyTerminalAction::Submit(state.take_draft()));
                        edited = true;
                    }
                    KeyCode::Backspace => {
                        state.backspace();
                        edited = true;
                    }
                    KeyCode::Delete => {
                        state.delete();
                        edited = true;
                    }
                    KeyCode::Left => {
                        state.left();
                        edited = true;
                    }
                    KeyCode::Right => {
                        state.right();
                        edited = true;
                    }
                    KeyCode::Home => {
                        state.home();
                        edited = true;
                    }
                    KeyCode::End => {
                        state.end();
                        edited = true;
                    }
                    KeyCode::Up => batch
                        .actions
                        .push(ReplyTerminalAction::Scroll(ScrollDelta::Rows(-1))),
                    KeyCode::Down => batch
                        .actions
                        .push(ReplyTerminalAction::Scroll(ScrollDelta::Rows(1))),
                    KeyCode::PageUp => batch
                        .actions
                        .push(ReplyTerminalAction::Scroll(ScrollDelta::Pages(-1))),
                    KeyCode::PageDown => batch
                        .actions
                        .push(ReplyTerminalAction::Scroll(ScrollDelta::Pages(1))),
                    KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        state.home();
                        edited = true;
                    }
                    KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        state.left();
                        edited = true;
                    }
                    KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        state.end();
                        edited = true;
                    }
                    KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        state.right();
                        edited = true;
                    }
                    KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        state.ctrl_w();
                        edited = true;
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        state.ctrl_u();
                        edited = true;
                    }
                    KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        state.ctrl_k();
                        edited = true;
                    }
                    KeyCode::Char(character)
                        if !character.is_control()
                            && !key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        state.insert(character);
                        edited = true;
                    }
                    _ => {}
                }
                if edited && batch.redraw == ReplyRedraw::None {
                    batch.redraw = ReplyRedraw::Input;
                }
            }
            _ => {}
        }
    }
    batch
}

struct ReplyTerminal {
    output: io::Stdout,
    state: ReplyUiState,
    previous_rows: Vec<(String, RowStyle)>,
    full_invalidate: bool,
    width: u16,
    height: u16,
    active: bool,
}

impl ReplyTerminal {
    fn new() -> Result<Self> {
        terminal::enable_raw_mode().context("enable reply terminal raw mode")?;
        let mut terminal = Self {
            output: io::stdout(),
            state: ReplyUiState::default(),
            previous_rows: Vec::new(),
            full_invalidate: true,
            width: 0,
            height: 0,
            active: true,
        };
        execute!(
            terminal.output,
            EnterAlternateScreen,
            EnableBracketedPaste,
            Hide
        )
        .context("initialize reply terminal screen")?;
        terminal.render()?;
        Ok(terminal)
    }

    fn push_line(&mut self, line: impl Into<String>) -> Result<()> {
        self.state.push_system(line);
        self.render()
    }

    fn push_chat(&mut self, entry: ChatEntry) -> Result<()> {
        self.state.push_chat(entry);
        self.render()
    }

    fn scroll(&mut self, delta: ScrollDelta) -> Result<()> {
        self.state.scroll(delta, self.height);
        self.render()
    }

    fn poll_actions(&mut self, timeout: Duration) -> Result<Option<Vec<ReplyTerminalAction>>> {
        if !event::poll(timeout).context("poll reply terminal input")? {
            return Ok(None);
        }
        let batch_started = Instant::now();
        let mut events = Vec::with_capacity(16);
        events.push(event::read().context("read reply terminal input")?);
        while events.len() < REPLY_INPUT_EVENT_LIMIT {
            let remaining = REPLY_INPUT_BATCH_LIMIT.saturating_sub(batch_started.elapsed());
            if remaining.is_zero()
                || !event::poll(REPLY_INPUT_BURST_GRACE.min(remaining))
                    .context("poll reply terminal input burst")?
            {
                break;
            }
            events.push(event::read().context("read reply terminal input burst")?);
        }

        let batch = apply_reply_input_events(&mut self.state, events);
        if batch.redraw == ReplyRedraw::Full {
            self.full_invalidate = true;
        }
        match batch.redraw {
            ReplyRedraw::None => {}
            ReplyRedraw::Input => self.render_input()?,
            ReplyRedraw::Full => self.render()?,
        }
        Ok(Some(batch.actions))
    }

    fn render_input(&mut self) -> Result<()> {
        let (width, height) = terminal::size().context("read reply terminal size")?;
        if (width, height) != (self.width, self.height) {
            self.full_invalidate = true;
            return self.render();
        }
        let frame = self.state.input_frame(width);
        let input_row = height.saturating_sub(2);
        let cache_index = input_row as usize;
        let changed = self
            .previous_rows
            .get(cache_index)
            .is_none_or(|previous| previous.0 != frame.input);
        queue!(self.output, Hide).context("hide reply cursor")?;
        if changed {
            queue!(
                self.output,
                MoveTo(0, input_row),
                Clear(ClearType::CurrentLine),
                Print(&frame.input)
            )
            .context("draw reply input")?;
            if let Some(previous) = self.previous_rows.get_mut(cache_index) {
                *previous = (frame.input.clone(), RowStyle::Plain);
            }
        }
        queue!(
            self.output,
            MoveTo(frame.cursor_column.min(width.saturating_sub(1)), input_row),
            Show
        )
        .context("position reply cursor")?;
        self.output.flush().context("flush reply terminal")?;
        Ok(())
    }

    fn render(&mut self) -> Result<()> {
        let (width, height) = terminal::size().context("read reply terminal size")?;
        if (width, height) != (self.width, self.height) {
            self.full_invalidate = true;
            self.width = width;
            self.height = height;
        }
        let frame = self.state.frame(width, height);
        let conversation_height = height.saturating_sub(3) as usize;
        let mut rows = Vec::with_capacity(height as usize);
        rows.extend(
            frame
                .conversation
                .iter()
                .cloned()
                .chain(std::iter::repeat(ReplyRow {
                    text: String::new(),
                    style: RowStyle::Plain,
                }))
                .take(conversation_height),
        );
        rows.push(ReplyRow {
            text: frame.separator,
            style: RowStyle::Dim,
        });
        rows.push(ReplyRow {
            text: frame.input,
            style: RowStyle::Plain,
        });
        rows.push(ReplyRow {
            text: frame.footer,
            style: RowStyle::Dim,
        });

        queue!(self.output, Hide).context("hide reply cursor")?;
        if self.full_invalidate {
            queue!(self.output, MoveTo(0, 0), Clear(ClearType::All))
                .context("clear reply terminal")?;
        }
        for (index, row) in rows.iter().enumerate() {
            if self.full_invalidate || reply_row_changed(self.previous_rows.get(index), row) {
                let terminal_row = u16::try_from(index).unwrap_or(u16::MAX);
                queue!(
                    self.output,
                    MoveTo(0, terminal_row),
                    Clear(ClearType::CurrentLine)
                )
                .context("clear changed reply row")?;
                queue_styled_row(&mut self.output, row).context("draw changed reply row")?;
            }
        }
        for index in rows.len()..self.previous_rows.len() {
            queue!(
                self.output,
                MoveTo(0, u16::try_from(index).unwrap_or(u16::MAX)),
                Clear(ClearType::CurrentLine)
            )
            .context("clear stale reply row")?;
        }
        let input_row = height.saturating_sub(2);
        queue!(
            self.output,
            MoveTo(frame.cursor_column.min(width.saturating_sub(1)), input_row),
            Show
        )
        .context("position reply cursor")?;
        self.output.flush().context("flush reply terminal")?;
        self.previous_rows = rows.into_iter().map(|row| (row.text, row.style)).collect();
        self.full_invalidate = false;
        Ok(())
    }
}

impl ReplyPollRenderer for ReplyTerminal {
    fn render_system(&mut self, line: String) -> Result<()> {
        self.push_line(line)
    }

    fn render_chat(&mut self, entry: ChatEntry) -> Result<()> {
        self.push_chat(entry)
    }
}

impl ReplyWatchUi for ReplyTerminal {
    fn poll_actions(&mut self, timeout: Duration) -> Result<Option<Vec<ReplyTerminalAction>>> {
        ReplyTerminal::poll_actions(self, timeout)
    }

    fn scroll(&mut self, delta: ScrollDelta) -> Result<()> {
        ReplyTerminal::scroll(self, delta)
    }

    fn restore_draft(&mut self, draft: &str) -> Result<()> {
        self.state.paste(draft);
        self.render_input()
    }
}

impl ReplyPollRenderer for ReplyUiState {
    fn render_system(&mut self, line: String) -> Result<()> {
        self.push_line(line);
        Ok(())
    }

    fn render_chat(&mut self, entry: ChatEntry) -> Result<()> {
        self.push_chat(entry);
        Ok(())
    }
}

#[cfg(test)]
impl ReplyWatchUi for ReplyUiState {
    fn poll_actions(&mut self, _timeout: Duration) -> Result<Option<Vec<ReplyTerminalAction>>> {
        Ok(None)
    }

    fn scroll(&mut self, delta: ScrollDelta) -> Result<()> {
        ReplyUiState::scroll(self, delta, 20);
        Ok(())
    }

    fn restore_draft(&mut self, draft: &str) -> Result<()> {
        self.paste(draft);
        Ok(())
    }
}

fn reply_row_changed(previous: Option<&(String, RowStyle)>, row: &ReplyRow) -> bool {
    previous.is_none_or(|previous| previous.0 != row.text || previous.1 != row.style)
}

fn queue_styled_row(output: &mut impl Write, row: &ReplyRow) -> io::Result<()> {
    match row.style {
        RowStyle::Plain => queue!(output, Print(&row.text))?,
        RowStyle::Dim => {
            queue!(
                output,
                SetAttribute(Attribute::Dim),
                Print(&row.text),
                SetAttribute(Attribute::Reset)
            )?;
        }
    }
    Ok(())
}

impl Drop for ReplyTerminal {
    fn drop(&mut self) {
        if self.active {
            let _ = execute!(
                self.output,
                Show,
                DisableBracketedPaste,
                LeaveAlternateScreen
            );
            let _ = terminal::disable_raw_mode();
            self.active = false;
        }
    }
}

pub(crate) fn command_requests_json(command: &Commands) -> bool {
    match command {
        Commands::Doctor { json, .. }
        | Commands::Sync { json, .. }
        | Commands::Index { json, .. }
        | Commands::WipeIndex { json, .. }
        | Commands::Chunks { json, .. }
        | Commands::Transcript { json, .. } => *json,
        Commands::Watch {
            select,
            format,
            reply,
            ..
        } => effective_watch_format(*select || *reply, *format) == WatchOutputFormat::Jsonl,
        Commands::Search { command } => match command {
            SearchCommand::Keyword { json, .. }
            | SearchCommand::Bm25 { json, .. }
            | SearchCommand::Semantic { json, .. }
            | SearchCommand::Hybrid { json, .. } => *json,
        },
        Commands::Chunk { command } => match command {
            crate::cli::ChunkCommand::Get { json, .. }
            | crate::cli::ChunkCommand::Context { json, .. }
            | crate::cli::ChunkCommand::Parent { json, .. } => *json,
        },
        Commands::Source { command } => match command {
            SourceCommand::Chats { json, .. } => *json,
        },
        Commands::Media { command } => match command {
            crate::cli::MediaCommand::Get { json, .. }
            | crate::cli::MediaCommand::Backfill { json, .. } => *json,
        },
        Commands::Permissions { command } => match command {
            PermissionsCommand::Macos { json, .. } => *json,
        },
        #[cfg(all(target_os = "macos", feature = "private-send"))]
        Commands::Send { json, .. } => *json,
    }
}

pub(crate) fn run(
    command: Commands,
    config: KatokConfig,
    data_dir: PathBuf,
    archive_path: PathBuf,
    semantic_dir: PathBuf,
) -> Result<()> {
    match command {
        Commands::Doctor { macos_probe, json } => run_doctor(
            macos_probe,
            json,
            config,
            data_dir,
            archive_path,
            semantic_dir,
        ),
        Commands::Sync {
            source,
            path,
            json,
            touched,
            prune_preview,
            prune_deleted,
        } => {
            let source = source.unwrap_or_else(|| config.source_adapter.clone());
            run_sync(
                &source,
                path,
                json,
                touched,
                prune_preview,
                prune_deleted,
                &config,
                &archive_path,
                &data_dir,
            )
        }
        Commands::Index {
            full,
            dry_run,
            json,
        } => index_commands::run(
            full,
            dry_run,
            json,
            &config,
            &archive_path,
            &semantic_dir,
            &data_dir,
        ),
        Commands::Search { command } => run_search(command, &config, &archive_path, &semantic_dir),
        Commands::Chunk { command } => chunk_commands::run(command, &archive_path),
        Commands::Source { command } => run_source(command, &config, &data_dir),
        Commands::Media { command } => media_commands::run(command, &data_dir),
        Commands::Permissions { command } => run_permissions(command),
        Commands::Chunks { chat, json } => run_chunks(&chat, json, &archive_path),
        Commands::Transcript {
            chat,
            since,
            out,
            json,
        } => run_transcript(&chat, since.as_deref(), out, json, &archive_path, &data_dir),
        Commands::Watch {
            source,
            path,
            chat,
            select,
            format,
            reply,
            reply_no_open,
            accept_use_policy,
            tail,
            poll_ms,
            once,
            max_polls,
            replay_existing,
        } => run_watch(
            source,
            path,
            chat,
            select,
            format,
            reply,
            reply_no_open,
            accept_use_policy,
            tail,
            poll_ms,
            once,
            max_polls,
            replay_existing,
            &config,
            &archive_path,
            &data_dir,
        ),
        Commands::WipeIndex { yes, json } => run_wipe_index(yes, json, &semantic_dir),
        #[cfg(all(target_os = "macos", feature = "private-send"))]
        Commands::Send {
            room,
            chat,
            text,
            image,
            list_windows,
            list_rooms,
            limit,
            dry_run,
            no_open,
            background_only,
            draft,
            take_focus_now,
            focus_wait,
            accept_use_policy,
            json,
        } => run_send(
            room,
            chat,
            text,
            image,
            list_windows,
            list_rooms,
            limit,
            dry_run,
            no_open,
            background_only,
            draft,
            take_focus_now,
            focus_wait,
            accept_use_policy,
            json,
            &archive_path,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_watch(
    source: Option<String>,
    path: Option<PathBuf>,
    chat: Option<String>,
    select: bool,
    format: Option<WatchOutputFormat>,
    reply: bool,
    reply_no_open: bool,
    accept_use_policy: bool,
    tail: u64,
    poll_ms: u64,
    once: bool,
    max_polls: Option<u64>,
    replay_existing: bool,
    config: &KatokConfig,
    archive_path: &Path,
    data_dir: &Path,
) -> Result<()> {
    let source = source.unwrap_or_else(|| config.source_adapter.clone());
    let format = effective_watch_format(select || reply, format);
    if reply {
        if format != WatchOutputFormat::Text {
            anyhow::bail!("reply mode is only available with --format text");
        }
        if chat.is_none() && !select {
            anyhow::bail!("reply mode requires --chat or --select");
        }
        if !accept_use_policy {
            anyhow::bail!(
                "refusing to continue without --accept-use-policy; read \
                 ACCEPTABLE_USE_POLICY.md and DISCLAIMER.md"
            );
        }
        if !io::stdin().is_terminal() {
            anyhow::bail!(
                "reply mode requires an interactive terminal; piped or redirected input is not sent"
            );
        }
        if !io::stdout().is_terminal() {
            anyhow::bail!("reply mode requires an interactive output terminal");
        }
    }
    let poll_interval = Duration::from_millis(poll_ms);
    let max_polls = if once { Some(1) } else { max_polls };
    let mut snapshot = WatchSnapshot::default();
    let mut stdout = std::io::stdout();
    let mut chat = chat;
    let mut selected_chat_name = None;
    let mut archive_counts = None;

    if select {
        let selected = select_watch_chat(&source, path.clone(), data_dir)?;
        selected_chat_name = Some(selected.chat_name);
        chat = Some(selected.chat_id);
    }

    if reply {
        return run_watch_with_reply(
            source,
            path,
            chat,
            selected_chat_name,
            tail,
            poll_interval,
            max_polls,
            replay_existing,
            reply_no_open,
            config,
            archive_path,
            data_dir,
        );
    }
    let mut reply_terminal: Option<ReplyTerminal> = None;
    let mut reply_send_state = ReplySendState::new();
    let mut reply_sender = spawn_reply_send;

    if format == WatchOutputFormat::Jsonl {
        print_jsonl_event(
            &mut stdout,
            &WatchEvent::State {
                schema_version: WATCH_EVENT_SCHEMA_VERSION,
                state: WatchState::Started,
                poll: 0,
                observed_messages: 0,
                observed_chats: 0,
                emitted_messages: 0,
                archived_messages: None,
                chunks: None,
            },
        )?;
    } else if !reply {
        eprintln!("lazykatok: starting terminal watch; press Ctrl-C to stop");
    }

    let mut poll = 0u64;
    loop {
        poll += 1;
        let poll_started = Instant::now();
        let next_poll_at = poll_started + poll_interval;
        let read_started = poll_started;
        if format == WatchOutputFormat::Jsonl {
            print_jsonl_event(
                &mut stdout,
                &WatchEvent::State {
                    schema_version: WATCH_EVENT_SCHEMA_VERSION,
                    state: WatchState::Reading,
                    poll,
                    observed_messages: snapshot.seen_count(),
                    observed_chats: 0,
                    emitted_messages: 0,
                    archived_messages: None,
                    chunks: None,
                },
            )?;
        } else if poll == 1 && !reply {
            eprintln!("lazykatok: reading source {source}...");
        }
        // Build a fresh adapter each pass. The macOS adapter memoizes one read
        // per instance, which is correct for `sync` but a watch loop must see
        // source changes made after the previous poll.
        let adapter = adapter_for_source(&source, path.clone(), data_dir)?;
        let messages = adapter.messages().context("read source messages")?;
        let read_source = read_started.elapsed().as_millis();
        if selected_chat_name.is_none() {
            selected_chat_name = chat
                .as_deref()
                .and_then(|chat_id| find_chat_name(&messages, chat_id));
        }
        let text_replay = format == WatchOutputFormat::Text && chat.is_some();
        let events = snapshot.diff(
            &messages,
            poll,
            replay_existing || text_replay,
            chat.as_deref(),
        );
        let events = if format == WatchOutputFormat::Text && poll == 1 {
            tail_events(events, tail as usize)
        } else {
            events
        };
        let emitted_messages = events.len();
        let (archive_changed, archived_messages, chunks) = if snapshot.source_changed() {
            let report = sync_watch_messages(
                &messages,
                read_source,
                &source,
                config,
                archive_path,
                data_dir,
            )
            .context("sync watched messages")?;
            let counts = (report.total_messages, report.chunks);
            archive_counts = Some(counts);
            (
                report.inserted_messages > 0 || report.updated_messages > 0,
                counts.0,
                counts.1,
            )
        } else {
            let (archived_messages, chunks) = archive_counts
                .context("watch archive counts missing after the initial source snapshot")?;
            // A successful unchanged poll is still a successful freshness check.
            // Preserve that timestamp while avoiding the full archive/chunk pass.
            freshness::record_sync(data_dir, &source, archived_messages, chunks)?;
            (false, archived_messages, chunks)
        };

        for event in events {
            match format {
                WatchOutputFormat::Jsonl => print_jsonl_event(&mut stdout, &event)?,
                WatchOutputFormat::Text => {
                    if let Some(terminal) = reply_terminal.as_mut() {
                        if let WatchEvent::Message {
                            change, message, ..
                        } = &event
                        {
                            let sender = if message.sender_nickname.trim().is_empty() {
                                message.sender_id.clone()
                            } else {
                                message.sender_nickname.clone()
                            };
                            let mut text = if message.text.trim().is_empty() {
                                format!("<{}>", message.message_type)
                            } else {
                                message.text.clone()
                            };
                            if *change == WatchMessageChange::Updated {
                                text.push_str(" (updated)");
                            }
                            terminal.push_chat(ChatEntry {
                                timestamp: message.timestamp,
                                sender,
                                text,
                            })?;
                        }
                    } else {
                        print_text_event(&mut stdout, &event)?;
                    }
                }
            }
        }

        if format == WatchOutputFormat::Jsonl {
            print_jsonl_event(
                &mut stdout,
                &WatchEvent::State {
                    schema_version: WATCH_EVENT_SCHEMA_VERSION,
                    state: if emitted_messages > 0 || archive_changed {
                        WatchState::Synced
                    } else {
                        WatchState::Idle
                    },
                    poll,
                    observed_messages: messages.len(),
                    observed_chats: chat_count(&messages),
                    emitted_messages,
                    archived_messages: Some(archived_messages),
                    chunks: Some(chunks),
                },
            )?;
        } else if poll == 1 {
            let label = selected_chat_name
                .as_deref()
                .map(sanitize_terminal_text)
                .unwrap_or_else(|| "selected chat".to_string());
            let status = format!(
                "lazykatok: watching {label}; displayed {emitted_messages} recent message(s); observed {} message(s) across {} chat(s)",
                messages.len(), chat_count(&messages)
            );
            if let Some(terminal) = reply_terminal.as_mut() {
                terminal.push_line(status)?;
                terminal.push_line(format!("lazykatok: replies target {label}"))?;
            } else {
                eprintln!("{status}");
            }
        }

        if let Some(terminal) = reply_terminal.as_mut() {
            if process_pending_reply_events(
                terminal,
                chat.as_deref(),
                reply_no_open,
                data_dir,
                &mut reply_send_state,
                &mut reply_sender,
            )? {
                break;
            }
        }

        if max_polls.is_some_and(|limit| poll >= limit) {
            break;
        }
        let remaining = remaining_poll_delay(next_poll_at, Instant::now());
        if let Some(terminal) = reply_terminal.as_mut() {
            if wait_for_next_poll_with_reply(
                terminal,
                chat.as_deref(),
                reply_no_open,
                data_dir,
                remaining,
                &mut reply_send_state,
                &mut reply_sender,
            )? {
                break;
            }
        } else {
            thread::sleep(remaining);
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_watch_with_reply(
    source: String,
    path: Option<PathBuf>,
    chat_id: Option<String>,
    selected_chat_name: Option<String>,
    tail: u64,
    poll_interval: Duration,
    max_polls: Option<u64>,
    replay_existing: bool,
    reply_no_open: bool,
    config: &KatokConfig,
    archive_path: &Path,
    data_dir: &Path,
) -> Result<()> {
    let mut terminal = ReplyTerminal::new()?;
    terminal.push_line("lazykatok: starting terminal watch; press Ctrl-C to stop")?;
    terminal.push_line(
        "lazykatok: reply mode enabled; type a message and press Enter to send, /help for commands, /quit to stop",
    )?;

    let (outcome_tx, outcome_rx) = mpsc::channel();
    let (stop_tx, stop_rx) = mpsc::channel();
    let options = ReplyPollWorkerOptions {
        poll_interval,
        max_polls,
        replay_existing,
        tail: tail as usize,
        chat_id: chat_id.clone(),
        selected_chat_name,
    };
    let read_source_name = source.clone();
    let read_data_dir = data_dir.to_path_buf();
    let sync_source_name = source.clone();
    let sync_config = config.clone();
    let sync_archive_path = archive_path.to_path_buf();
    let sync_data_dir = data_dir.to_path_buf();
    let freshness_source_name = source;
    let freshness_data_dir = data_dir.to_path_buf();
    let worker = thread::spawn(move || {
        reply_poll_worker(
            options,
            outcome_tx,
            stop_rx,
            move || {
                // A new adapter per pass is required because the macOS adapter memoizes its read.
                let adapter = adapter_for_source(&read_source_name, path.clone(), &read_data_dir)?;
                Ok(adapter.messages()?)
            },
            move |messages, read_source| {
                let report = sync_watch_messages(
                    messages,
                    read_source,
                    &sync_source_name,
                    &sync_config,
                    &sync_archive_path,
                    &sync_data_dir,
                )?;
                Ok(PollArchiveState {
                    archive_changed: report.inserted_messages > 0 || report.updated_messages > 0,
                    archived_messages: report.total_messages,
                    chunks: report.chunks,
                })
            },
            move |archived_messages, chunks| {
                freshness::record_sync(
                    &freshness_data_dir,
                    &freshness_source_name,
                    archived_messages,
                    chunks,
                )
            },
        );
    });

    let ui_result = pump_reply_watch(
        &mut terminal,
        &outcome_rx,
        chat_id.as_deref(),
        reply_no_open,
        data_dir,
        max_polls,
    );
    drop(stop_tx);
    // Shutdown can wait for an in-flight source read; the worker never touches terminal state.
    let join_result = worker
        .join()
        .map_err(|_| anyhow::anyhow!("watch poll worker panicked"));
    join_result?;
    ui_result
}

fn reply_poll_worker<ReadSource, SyncMessages, RecordFreshness>(
    options: ReplyPollWorkerOptions,
    outcome_tx: Sender<ReplyPollOutcome>,
    stop_rx: Receiver<()>,
    mut read_source: ReadSource,
    mut sync_messages: SyncMessages,
    mut record_freshness: RecordFreshness,
) where
    ReadSource: FnMut() -> Result<Vec<RawMessage>> + Send,
    SyncMessages: FnMut(&[RawMessage], u128) -> Result<PollArchiveState> + Send,
    RecordFreshness: FnMut(usize, usize) -> Result<()> + Send,
{
    let mut snapshot = WatchSnapshot::default();
    let mut archive_counts = None;
    let mut selected_chat_name = options.selected_chat_name;
    let mut poll = 0u64;

    loop {
        poll += 1;
        let poll_started = Instant::now();
        let next_poll_at = poll_started + options.poll_interval;
        let messages = match read_source().context("read source messages") {
            Ok(messages) => messages,
            Err(error) => {
                let _ = outcome_tx.send(ReplyPollOutcome::Error { poll, error });
                return;
            }
        };
        if matches!(
            stop_rx.try_recv(),
            Ok(()) | Err(mpsc::TryRecvError::Disconnected)
        ) {
            return;
        }
        let read_source = poll_started.elapsed().as_millis();
        if selected_chat_name.is_none() {
            selected_chat_name = options
                .chat_id
                .as_deref()
                .and_then(|chat_id| find_chat_name(&messages, chat_id));
        }
        let events = snapshot.diff(
            &messages,
            poll,
            options.replay_existing || options.chat_id.is_some(),
            options.chat_id.as_deref(),
        );
        let events = if poll == 1 {
            tail_events(events, options.tail)
        } else {
            events
        };
        let emitted_messages = events.len();
        let archive_state = if snapshot.source_changed() {
            match sync_messages(&messages, read_source).context("sync watched messages") {
                Ok(state) => {
                    archive_counts = Some((state.archived_messages, state.chunks));
                    state
                }
                Err(error) => {
                    let _ = outcome_tx.send(ReplyPollOutcome::Error { poll, error });
                    return;
                }
            }
        } else {
            let Some((archived_messages, chunks)) = archive_counts else {
                let error = anyhow::anyhow!(
                    "watch archive counts missing after the initial source snapshot"
                );
                let _ = outcome_tx.send(ReplyPollOutcome::Error { poll, error });
                return;
            };
            if let Err(error) = record_freshness(archived_messages, chunks) {
                let _ = outcome_tx.send(ReplyPollOutcome::Error { poll, error });
                return;
            }
            PollArchiveState {
                archive_changed: false,
                archived_messages,
                chunks,
            }
        };
        let outcome = ReplyPollOutcome::Completed {
            poll,
            events,
            archive_changed: archive_state.archive_changed,
            observed_messages: messages.len(),
            observed_chats: chat_count(&messages),
            emitted_messages,
            archived_messages: archive_state.archived_messages,
            chunks: archive_state.chunks,
            selected_chat_name: selected_chat_name.clone(),
        };
        if outcome_tx.send(outcome).is_err() {
            return;
        }
        if options.max_polls.is_some_and(|limit| poll >= limit) {
            return;
        }
        let remaining = remaining_poll_delay(next_poll_at, Instant::now());
        match stop_rx.recv_timeout(remaining) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

fn pump_reply_watch(
    terminal: &mut ReplyTerminal,
    outcome_rx: &Receiver<ReplyPollOutcome>,
    chat_id: Option<&str>,
    no_open: bool,
    data_dir: &Path,
    max_polls: Option<u64>,
) -> Result<()> {
    pump_reply_watch_with_sender(
        terminal,
        outcome_rx,
        chat_id,
        no_open,
        data_dir,
        max_polls,
        &mut spawn_reply_send,
    )
}

fn pump_reply_watch_with_sender<Ui, StartSend>(
    terminal: &mut Ui,
    outcome_rx: &Receiver<ReplyPollOutcome>,
    chat_id: Option<&str>,
    no_open: bool,
    data_dir: &Path,
    max_polls: Option<u64>,
    start_send: &mut StartSend,
) -> Result<()>
where
    Ui: ReplyWatchUi,
    StartSend: FnMut(ReplySendRequest),
{
    let mut send_state = ReplySendState::new();
    loop {
        let outcome = match outcome_rx.recv_timeout(REPLY_PUMP_SLICE) {
            Ok(outcome) => Some(outcome),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        };

        drain_reply_send_outcomes(terminal, &mut send_state)?;
        if process_pending_reply_events(
            terminal,
            chat_id,
            no_open,
            data_dir,
            &mut send_state,
            start_send,
        )? {
            return Ok(());
        }

        if let Some(outcome) = outcome {
            render_reply_poll_outcome(terminal, &outcome)?;
            match outcome {
                ReplyPollOutcome::Completed { poll, .. }
                    if max_polls.is_some_and(|limit| poll >= limit) =>
                {
                    return Ok(());
                }
                ReplyPollOutcome::Completed { .. } => {}
                ReplyPollOutcome::Error { error, .. } => return Err(error),
            }
        }
    }
}

fn render_reply_poll_outcome(
    renderer: &mut impl ReplyPollRenderer,
    outcome: &ReplyPollOutcome,
) -> Result<()> {
    match outcome {
        ReplyPollOutcome::Completed {
            poll,
            events,
            archive_changed,
            observed_messages,
            observed_chats,
            emitted_messages,
            archived_messages,
            chunks,
            selected_chat_name,
        } => {
            for event in events {
                if let WatchEvent::Message {
                    change, message, ..
                } = event
                {
                    let sender = if message.sender_nickname.trim().is_empty() {
                        message.sender_id.clone()
                    } else {
                        message.sender_nickname.clone()
                    };
                    let mut text = if message.text.trim().is_empty() {
                        format!("<{}>", message.message_type)
                    } else {
                        message.text.clone()
                    };
                    if *change == WatchMessageChange::Updated {
                        text.push_str(" (updated)");
                    }
                    renderer.render_chat(ChatEntry {
                        timestamp: message.timestamp,
                        sender,
                        text,
                    })?;
                }
            }
            if *poll == 1 {
                let label = selected_chat_name
                    .as_deref()
                    .map(sanitize_terminal_text)
                    .unwrap_or_else(|| "selected chat".to_string());
                renderer.render_system(format!(
                    "lazykatok: watching {label}; displayed {emitted_messages} recent message(s); observed {observed_messages} message(s) across {observed_chats} chat(s)"
                ))?;
                renderer.render_system(format!("lazykatok: replies target {label}"))?;
            }
            // The protocol carries the complete poll state even though the reply UI currently
            // prints only observed and emitted counts. Keep the other state available to the pump.
            let _poll_state = (archive_changed, archived_messages, chunks);
        }
        ReplyPollOutcome::Error { poll, error } => {
            renderer.render_system(format!("lazykatok: watch poll {poll} failed: {error:#}"))?;
        }
    }
    Ok(())
}

fn effective_watch_format(
    human_default: bool,
    format: Option<WatchOutputFormat>,
) -> WatchOutputFormat {
    format.unwrap_or(if human_default {
        WatchOutputFormat::Text
    } else {
        WatchOutputFormat::Jsonl
    })
}

/// Order the interactive room list oldest-first so the newest rooms sit at
/// the bottom, right above the input prompt. Chats the source cannot date go
/// to the top; ties break by name then id so the list is stable across runs.
fn order_chats_for_selection(chats: &mut [ChatSummary]) {
    chats.sort_by(|left, right| {
        left.last_message_at
            .cmp(&right.last_message_at)
            .then_with(|| left.chat_name.cmp(&right.chat_name))
            .then_with(|| left.chat_id.cmp(&right.chat_id))
    });
}

/// Numbering runs bottom-up: the newest room (last row, nearest the prompt)
/// is 1 and numbers grow toward the top of the list.
fn selection_number(index: usize, total: usize) -> usize {
    total - index
}

fn selection_index(number: usize, total: usize) -> Option<usize> {
    (1..=total).contains(&number).then(|| total - number)
}

fn write_chat_selection_list(stderr: &mut impl Write, chats: &[ChatSummary]) -> Result<()> {
    writeln!(stderr, "Choose a chat to watch:").context("write chat selection prompt")?;
    let total = chats.len();
    for (index, chat) in chats.iter().enumerate() {
        writeln!(
            stderr,
            "{:>3}. {} ({}, {})",
            selection_number(index, total),
            sanitize_terminal_text(&chat.chat_name),
            sanitize_terminal_text(&chat.chat_type),
            sanitize_terminal_text(&chat.chat_id)
        )
        .context("write chat selection option")?;
    }
    write!(stderr, "chat number or chat_id> ").context("write chat selection input prompt")?;
    Ok(())
}

fn select_watch_chat(source: &str, path: Option<PathBuf>, data_dir: &Path) -> Result<ChatSummary> {
    eprintln!("lazykatok: reading chat list from {source}...");
    let adapter = adapter_for_source(source, path, data_dir)?;
    let mut chats = adapter.chats().context("list source chats")?;
    if chats.is_empty() {
        anyhow::bail!("no chats found in source");
    }
    order_chats_for_selection(&mut chats);

    let mut stderr = io::stderr().lock();
    write_chat_selection_list(&mut stderr, &chats)?;
    stderr.flush().context("flush chat selection prompt")?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("read selected chat")?;
    let input = input.trim();
    if input.is_empty() {
        anyhow::bail!("no chat selected");
    }
    if let Ok(number) = input.parse::<usize>() {
        let index = selection_index(number, chats.len())
            .with_context(|| format!("chat number {number} is out of range"))?;
        return Ok(chats[index].clone());
    }
    chats
        .into_iter()
        .find(|chat| chat.chat_id == input)
        .with_context(|| format!("no chat_id {input} in source"))
}

fn find_chat_name(messages: &[RawMessage], chat_id: &str) -> Option<String> {
    messages
        .iter()
        .find(|message| message.chat_id == chat_id)
        .map(|message| message.chat_name.clone())
}

fn tail_events(mut events: Vec<WatchEvent>, tail: usize) -> Vec<WatchEvent> {
    if events.len() > tail {
        events.split_off(events.len() - tail)
    } else {
        events
    }
}

fn print_text_event(stdout: &mut impl Write, event: &WatchEvent) -> Result<()> {
    if let WatchEvent::Message {
        change, message, ..
    } = event
    {
        writeln!(stdout, "{}", format_human_message_line(*change, message))
            .context("write watch text line")?;
        stdout.flush().context("flush watch text line")?;
    }
    Ok(())
}

fn print_jsonl_event(stdout: &mut impl Write, event: &WatchEvent) -> Result<()> {
    serde_json::to_writer(&mut *stdout, event).context("serialize watch event")?;
    writeln!(stdout).context("write watch event")?;
    stdout.flush().context("flush watch event")?;
    Ok(())
}

fn process_pending_reply_events<Ui, StartSend>(
    terminal: &mut Ui,
    chat_id: Option<&str>,
    no_open: bool,
    data_dir: &Path,
    send_state: &mut ReplySendState,
    start_send: &mut StartSend,
) -> Result<bool>
where
    Ui: ReplyWatchUi,
    StartSend: FnMut(ReplySendRequest),
{
    while let Some(actions) = terminal.poll_actions(Duration::ZERO)? {
        for action in actions {
            if process_reply_action(
                terminal, action, chat_id, no_open, data_dir, send_state, start_send,
            )? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn process_reply_action<Ui, StartSend>(
    terminal: &mut Ui,
    action: ReplyTerminalAction,
    chat_id: Option<&str>,
    no_open: bool,
    data_dir: &Path,
    send_state: &mut ReplySendState,
    start_send: &mut StartSend,
) -> Result<bool>
where
    Ui: ReplyWatchUi,
    StartSend: FnMut(ReplySendRequest),
{
    let line = match action {
        ReplyTerminalAction::Quit => return finish_reply_quit(terminal, send_state),
        ReplyTerminalAction::Scroll(delta) => {
            terminal.scroll(delta)?;
            return Ok(false);
        }
        ReplyTerminalAction::Submit(line) => line,
    };
    match parse_reply_command(1, &line) {
        ReplyCommand::Send(body) => {
            if send_state.in_flight {
                terminal.restore_draft(&line)?;
                terminal.render_system(
                    "lazykatok: 이전 전송이 끝나기를 기다리는 중입니다".to_string(),
                )?;
                return Ok(false);
            }
            let chat_id = chat_id.context("watch --reply has no selected chat")?;
            terminal.render_system("lazykatok: 전송 중…".to_string())?;
            send_state.in_flight = true;
            start_send(ReplySendRequest {
                chat_id: chat_id.to_string(),
                body,
                no_open,
                data_dir: data_dir.to_path_buf(),
                outcome_tx: send_state.outcome_tx.clone(),
            });
        }
        ReplyCommand::Quit => return finish_reply_quit(terminal, send_state),
        ReplyCommand::Help => terminal.render_system(
            "lazykatok: Enter send; Left/Right/Home/End or Ctrl-A/B/E/F move; Backspace/Delete, Ctrl-W/U/K edit; Up/Down/PgUp/PgDn scroll; Ctrl-C/Ctrl-D or /quit quit".to_string(),
        )?,
        ReplyCommand::Empty => {}
        ReplyCommand::Ignored => terminal
            .render_system("lazykatok: not sent; unknown slash command".to_string())?,
    }
    Ok(false)
}

fn finish_reply_quit(
    terminal: &mut impl ReplyPollRenderer,
    send_state: &ReplySendState,
) -> Result<bool> {
    if send_state.in_flight {
        terminal.render_system("lazykatok: 전송은 백그라운드에서 계속됩니다".to_string())?;
    }
    Ok(true)
}

fn drain_reply_send_outcomes(
    terminal: &mut impl ReplyPollRenderer,
    send_state: &mut ReplySendState,
) -> Result<()> {
    while let Ok(outcome) = send_state.outcome_rx.try_recv() {
        send_state.in_flight = false;
        match outcome {
            Ok(chars) => {
                terminal.render_system(format!("lazykatok: sent reply ({chars} chars)"))?;
            }
            Err(err) => terminal.render_system(format!("lazykatok: send failed: {err:#}"))?,
        }
    }
    Ok(())
}

fn spawn_reply_send(request: ReplySendRequest) {
    thread::spawn(move || {
        let outcome = send_reply_to_chat(
            &request.chat_id,
            &request.body,
            request.no_open,
            &request.data_dir,
        );
        let _ = request.outcome_tx.send(outcome);
    });
}

fn remaining_poll_delay(deadline: Instant, now: Instant) -> Duration {
    deadline.saturating_duration_since(now)
}

fn wait_for_next_poll_with_reply<StartSend>(
    terminal: &mut ReplyTerminal,
    chat_id: Option<&str>,
    no_open: bool,
    data_dir: &Path,
    poll_interval: Duration,
    send_state: &mut ReplySendState,
    start_send: &mut StartSend,
) -> Result<bool>
where
    StartSend: FnMut(ReplySendRequest),
{
    let deadline = Instant::now() + poll_interval;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        drain_reply_send_outcomes(terminal, send_state)?;
        if let Some(actions) = terminal.poll_actions(remaining.min(Duration::from_millis(250)))? {
            for action in actions {
                if process_reply_action(
                    terminal, action, chat_id, no_open, data_dir, send_state, start_send,
                )? {
                    return Ok(true);
                }
            }
        }
    }
}

#[cfg(all(target_os = "macos", feature = "private-send"))]
fn send_reply_to_chat(chat_id: &str, body: &str, no_open: bool, data_dir: &Path) -> Result<usize> {
    let mut command =
        ProcessCommand::new(std::env::current_exe().context("resolve lazykatok binary")?);
    command.args(reply_send_args(chat_id, no_open, data_dir));
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start lazykatok send")?;
    {
        let stdin = child.stdin.as_mut().context("open lazykatok send stdin")?;
        stdin
            .write_all(body.as_bytes())
            .context("write reply body to lazykatok send")?;
        stdin
            .write_all(b"\n")
            .context("finish reply body for lazykatok send")?;
    }
    let output = child
        .wait_with_output()
        .context("wait for lazykatok send")?;
    if !output.status.success() {
        let detail = send_child_error_detail(output.status, &output.stdout, &output.stderr);
        anyhow::bail!("{detail}");
    }
    Ok(body.chars().count())
}

#[cfg(all(target_os = "macos", feature = "private-send"))]
fn reply_send_args(chat_id: &str, no_open: bool, data_dir: &Path) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("--data-dir"),
        data_dir.as_os_str().to_os_string(),
        OsString::from("send"),
        OsString::from("--chat"),
        OsString::from(chat_id),
        OsString::from("--accept-use-policy"),
        OsString::from("--json"),
    ];
    if no_open {
        args.push(OsString::from("--no-open"));
        args.push(OsString::from("--background-only"));
    } else {
        args.push(OsString::from("--focus-wait"));
        args.push(OsString::from(REPLY_FOCUS_WAIT_SECS.to_string()));
    }
    args
}

#[cfg(all(target_os = "macos", feature = "private-send"))]
fn send_child_error_detail(status: impl std::fmt::Display, stdout: &[u8], stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        return stderr.to_string();
    }

    let stdout = String::from_utf8_lossy(stdout);
    let stdout = stdout.trim();
    if !stdout.is_empty() {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout) {
            if let Some(cause) = value
                .pointer("/error/cause")
                .and_then(|value| value.as_str())
            {
                return cause.to_string();
            }
            if let Some(message) = value
                .pointer("/error/message")
                .and_then(|value| value.as_str())
            {
                return message.to_string();
            }
        }
        return stdout.to_string();
    }

    format!("lazykatok send failed with {status}")
}

#[cfg(not(all(target_os = "macos", feature = "private-send")))]
fn send_reply_to_chat(
    _chat_id: &str,
    _body: &str,
    _no_open: bool,
    _data_dir: &Path,
) -> Result<usize> {
    anyhow::bail!("watch --reply requires the macOS private-send feature")
}

fn sync_watch_messages(
    messages: &[RawMessage],
    read_source: u128,
    source: &str,
    config: &KatokConfig,
    archive_path: &Path,
    data_dir: &Path,
) -> Result<SyncReport> {
    let archive = Archive::open(archive_path).context("open archive")?;
    let mut report = archive.in_transaction(|| {
        let upsert_started = Instant::now();
        let mut report = archive.sync_messages(messages).context("sync messages")?;
        let upsert_messages = upsert_started.elapsed().as_millis();

        let rebuild_started = Instant::now();
        let settings = ChunkSettings {
            group_gap_seconds: config.chunk_gap_group_seconds,
            direct_gap_seconds: config.chunk_gap_direct_seconds,
        };
        let stored_settings = archive
            .stored_chunk_settings()
            .context("read chunk settings")?;
        let settings_changed = stored_settings
            != Some((
                settings.group_gap_seconds,
                settings.direct_gap_seconds,
                CHUNKER_VERSION,
            ));
        report.chunks = if archive.chunk_count().context("count chunks")? == 0 || settings_changed {
            rebuild_chunks_with_settings(&archive, settings).context("rebuild chunks")?
        } else {
            rebuild_chunks_for_chats(&archive, settings, &report.touched_chats)
                .context("rebuild chunks")?
        };
        archive
            .record_chunk_settings(
                settings.group_gap_seconds,
                settings.direct_gap_seconds,
                CHUNKER_VERSION,
            )
            .context("record chunk settings")?;

        report.timings_ms = SyncTimings {
            read_source,
            upsert_messages,
            rebuild_chunks: rebuild_started.elapsed().as_millis(),
        };
        Ok::<_, anyhow::Error>(report)
    })?;
    report.include_touched = false;
    freshness::record_sync(data_dir, source, report.total_messages, report.chunks)?;
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
#[cfg(all(target_os = "macos", feature = "private-send"))]
fn run_send(
    room: Option<String>,
    chat: Option<String>,
    text: Option<String>,
    image: Option<PathBuf>,
    list_windows: bool,
    list_rooms: bool,
    limit: usize,
    dry_run: bool,
    no_open: bool,
    background_only: bool,
    draft: bool,
    take_focus_now: bool,
    focus_wait: u64,
    accept_use_policy: bool,
    json: bool,
    archive_path: &Path,
) -> Result<()> {
    use lazykatok::kakao::ax_send;
    use std::io::Read;

    if list_windows {
        let titles = ax_send::open_window_titles()?;
        return print_payload(json, &serde_json::json!({ "open_windows": titles }));
    }
    if list_rooms {
        let rooms = ax_send::chat_list_rooms(limit)?;
        return print_payload(json, &serde_json::json!({ "rooms": rooms }));
    }
    if !dry_run && !accept_use_policy {
        anyhow::bail!(
            "refusing to continue without --accept-use-policy; read \
             ACCEPTABLE_USE_POLICY.md and DISCLAIMER.md"
        );
    }

    let allow_open = !no_open;
    // Anything that has to bring KakaoTalk forward waits for a gap in the user's
    // own typing first, so a send never lands mid-keystroke.
    let policy = if take_focus_now {
        ax_send::FocusPolicy::immediate()
    } else {
        ax_send::FocusPolicy {
            max_wait: std::time::Duration::from_secs(focus_wait),
            ..ax_send::FocusPolicy::default()
        }
    };

    // Read stdin before the curtain goes up: a blocked terminal behind a
    // full-screen overlay waiting for input the user cannot give would be a
    // deadlock they can only escape by killing the process.
    let body = if dry_run || image.is_some() {
        None
    } else {
        let body = match text {
            Some(t) => t,
            None => {
                let mut buf = String::new();
                std::io::stdin()
                    .read_to_string(&mut buf)
                    .context("failed to read message body from stdin")?;
                buf
            }
        };
        let body = body.trim_end_matches('\n').to_string();
        if body.is_empty() {
            anyhow::bail!("refusing to send an empty message");
        }
        Some(body)
    };

    // The curtain owns the main thread — AppKit insists on it — so the send runs
    // on a worker. It only actually covers the screen for the steps that must
    // bring KakaoTalk forward; an ordinary background text send never raises it.
    // Resolve the target once. A `--chat` id also brings the last-message time,
    // which is the only thing on the chat list that separates two rooms sharing
    // a name — without it such a send is refused rather than sent to whichever
    // one happens to sit higher.
    let target = match (&room, &chat) {
        (_, Some(chat_id)) => {
            let archive = Archive::open(archive_path).context("open archive")?;
            let (name, rank) = archive
                .chat_identity(chat_id)
                .context("look up chat")?
                .with_context(|| format!("no chat {chat_id} in the archive; run sync first"))?;
            ax_send::RoomTarget {
                name,
                rank: Some(rank),
            }
        }
        (Some(name), None) => ax_send::RoomTarget::named(name),
        (None, None) => anyhow::bail!("pass --room or --chat"),
    };
    let room_display = target.name.clone();
    let target_owned = target.clone();
    let image_owned = image.clone();
    let body_owned = body.clone();
    // The curtain says what is actually happening. It used to read "전송중" for
    // every operation, including ones that send nothing at all — the same kind
    // of claim that made a draft mode report `sent: false` while delivering.
    let curtain_title = if dry_run {
        "카톡 채팅방 여는 중"
    } else if draft {
        "카톡 초안 넣는 중"
    } else if image.is_some() {
        "카톡 이미지 보내는 중"
    } else {
        "카톡 보내는 중"
    };
    if background_only {
        let body = body_owned.expect("text body prepared for background-only send");
        let ctx = ax_send::SendContext::background_only(policy);
        ax_send::send_to_open_window(&target_owned, &body, false, &ctx)?;
        return print_payload(
            json,
            &serde_json::json!({ "sent": true, "room": room_display }),
        );
    }

    let outcome = lazykatok::kakao::send_curtain::run_with_curtain(curtain_title, move |curtain| {
        let ctx = ax_send::SendContext::with_curtain(policy, curtain);
        if dry_run {
            return ax_send::resolve_room_window(&target_owned, allow_open, &ctx);
        }
        if let Some(path) = &image_owned {
            return ax_send::send_image_to_open_window(&target_owned, path, allow_open, &ctx);
        }
        let body = body_owned.expect("text body prepared before the curtain");
        if draft {
            return ax_send::draft_to_open_window(&target_owned, &body, allow_open, &ctx);
        }
        ax_send::send_to_open_window(&target_owned, &body, allow_open, &ctx)
    });

    match outcome {
        Ok(result) => result?,
        // The worker panicked. Input and focus were already handed back by the
        // curtain's own scope guards, so report rather than resume it.
        Err(_) => anyhow::bail!("the send thread panicked; nothing was confirmed sent"),
    }

    if dry_run {
        return print_payload(
            json,
            &serde_json::json!({ "resolved": true, "room": room_display, "sent": false }),
        );
    }
    if let Some(path) = image {
        // Report the file name only; the path can leak directory structure into logs.
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("(unnamed)");
        return print_payload(
            json,
            &serde_json::json!({ "sent": true, "room": room_display, "image": name }),
        );
    }
    // Never echo the body: sent content is as sensitive as anything else this crate handles.
    let chars = body.map(|b| b.chars().count()).unwrap_or(0);
    print_payload(
        json,
        &serde_json::json!({
            "sent": !draft,
            "drafted": draft,
            "room": room_display,
            "chars": chars
        }),
    )
}

fn run_permissions(command: PermissionsCommand) -> Result<()> {
    match command {
        PermissionsCommand::Macos {
            accessibility,
            dry_run,
            json,
        } => permissions::open_macos(accessibility, dry_run, json),
    }
}

fn run_doctor(
    macos_probe_enabled: bool,
    json: bool,
    config: KatokConfig,
    data_dir: PathBuf,
    archive_path: PathBuf,
    semantic_dir: PathBuf,
) -> Result<()> {
    let macos_probe = macos_probe_payload(macos_probe_enabled, &data_dir);
    // `provider` names the backend the indexer will pick; the deterministic test
    // embedders still resolve locally, so only the HTTP backend reports differently.
    let embedder_provider = match config.embedder_family() {
        "openai-compatible" => "openai-compatible",
        _ => "local",
    };
    let embedder_mode = config.embedder_provider.clone();
    let embedder_endpoint = config.embedder_url.clone();
    let embedder_model = config.embedder_model.clone();
    let embedder_dimension = config.vector_dimension;
    let payload = serde_json::json!({
        "name": "lazykatok",
        "command": "lazykatok",
        "data_dir": data_dir,
        "archive": archive_path,
        "semantic_index": semantic_dir,
        "freshness": freshness::load(&data_dir, &archive_path, &semantic_dir)?,
        "local_first": true,
        "macos": cfg!(target_os = "macos"),
        "source_adapter": {
            "configured": config.source_adapter,
            "fixture": "ok",
            "kakaocli": dependency_status("kakaocli"),
            "macos": macos_probe
        },
        "archive": {
            "status": if archive_path.exists() { "present" } else { "missing" }
        },
        "embedder": {
            "model": embedder_model,
            "dimension": embedder_dimension,
            "provider": embedder_provider,
            "mode": embedder_mode,
            "endpoint": embedder_endpoint
        }
    });
    print_payload(json, &payload)
}

#[cfg(test)]
mod chat_selection_tests {
    use super::*;

    fn summary(id: &str, name: &str, at: Option<&str>) -> ChatSummary {
        ChatSummary {
            chat_id: id.to_string(),
            chat_name: name.to_string(),
            chat_type: "group".to_string(),
            last_message_at: at.map(|iso| {
                chrono::DateTime::parse_from_rfc3339(iso)
                    .expect("parse timestamp")
                    .with_timezone(&chrono::Utc)
            }),
        }
    }

    #[test]
    fn selection_orders_oldest_first_so_newest_sits_at_the_prompt() {
        let mut chats = vec![
            summary("chat-none", "Undated", None),
            summary("chat-old", "Older", Some("2026-01-01T09:00:00Z")),
            summary("chat-new", "Newest", Some("2026-01-03T09:00:00Z")),
            summary("chat-mid", "Middle", Some("2026-01-02T09:00:00Z")),
        ];
        order_chats_for_selection(&mut chats);
        let ids: Vec<_> = chats.iter().map(|chat| chat.chat_id.as_str()).collect();
        assert_eq!(ids, ["chat-none", "chat-old", "chat-mid", "chat-new"]);
    }

    #[test]
    fn selection_numbers_run_bottom_up_so_the_newest_room_is_one() {
        let mut chats = vec![
            summary("chat-old", "Older", Some("2026-01-01T09:00:00Z")),
            summary("chat-new", "Newest", Some("2026-01-03T09:00:00Z")),
        ];
        order_chats_for_selection(&mut chats);

        let mut list = Vec::new();
        write_chat_selection_list(&mut list, &chats).expect("write list");
        let text = String::from_utf8(list).expect("utf8");
        assert!(text.contains("2. Older"), "oldest row numbered 2: {text}");
        assert!(text.contains("1. Newest"), "newest row numbered 1: {text}");
        let older_line = text
            .lines()
            .find(|line| line.contains("Older"))
            .expect("older line");
        let newer_line = text
            .lines()
            .find(|line| line.contains("Newest"))
            .expect("newest line");
        let older_at = text.rfind(older_line.trim()).expect("older position");
        let newer_at = text.rfind(newer_line.trim()).expect("newer position");
        assert!(older_at < newer_at, "oldest renders above newest: {text}");

        assert_eq!(selection_index(1, 2), Some(1));
        assert_eq!(selection_index(2, 2), Some(0));
        assert_eq!(selection_index(3, 2), None);
    }

    #[test]
    fn selection_ties_break_by_name_then_id() {
        let mut chats = vec![
            summary("chat-b", "Same Room", Some("2026-01-01T09:00:00Z")),
            summary("chat-a", "Same Room", Some("2026-01-01T09:00:00Z")),
        ];
        order_chats_for_selection(&mut chats);
        assert_eq!(chats[0].chat_id, "chat-a");
    }
}

#[cfg(test)]
mod reply_terminal_tests {
    use super::{
        apply_reply_input_events, drain_reply_send_outcomes, process_reply_action,
        pump_reply_watch_with_sender, render_reply_poll_outcome, reply_poll_worker,
        reply_row_changed, PollArchiveState, ReplyPollOutcome, ReplyPollRenderer,
        ReplyPollWorkerOptions, ReplyRedraw, ReplySendRequest, ReplySendState, ReplyTerminalAction,
        ReplyWatchUi,
    };
    use anyhow::anyhow;
    use chrono::{TimeZone, Utc};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use lazykatok::{
        types::RawMessage,
        watch::{ReplyRow, ReplyUiState, RowStyle, ScrollDelta},
    };
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::path::Path;
    use std::rc::Rc;
    use std::sync::mpsc;
    use std::time::Duration;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    fn synthetic_message() -> RawMessage {
        RawMessage {
            account_hash: "account-synthetic".to_string(),
            chat_id: "chat-synthetic-1".to_string(),
            chat_name: "Synthetic Team".to_string(),
            chat_type: "group".to_string(),
            message_id: "message-synthetic-1".to_string(),
            sender_id: "sender-synthetic-1".to_string(),
            sender_nickname: "Synthetic Sender".to_string(),
            timestamp: Utc
                .with_ymd_and_hms(2026, 1, 1, 9, 0, 0)
                .single()
                .expect("valid timestamp"),
            text: "Synthetic incoming message".to_string(),
            message_type: "text".to_string(),
            reply_to_message_id: None,
        }
    }

    fn worker_options() -> ReplyPollWorkerOptions {
        ReplyPollWorkerOptions {
            poll_interval: Duration::from_secs(60),
            max_polls: Some(1),
            replay_existing: true,
            tail: 50,
            chat_id: Some("chat-synthetic-1".to_string()),
            selected_chat_name: Some("Synthetic Team".to_string()),
        }
    }

    fn system_lines(state: &mut ReplyUiState) -> Vec<String> {
        state
            .frame(120, 20)
            .conversation
            .into_iter()
            .map(|row| row.text)
            .collect()
    }

    #[test]
    fn reply_send_status_precedes_channel_completion() {
        let mut state = ReplyUiState::default();
        let mut sends = ReplySendState::new();
        let call_count = Rc::new(Cell::new(0));
        let counted = Rc::clone(&call_count);
        let mut sender = move |request: ReplySendRequest| {
            counted.set(counted.get() + 1);
            request.outcome_tx.send(Ok(2)).expect("return fake send");
        };

        let quit = process_reply_action(
            &mut state,
            ReplyTerminalAction::Submit("확인".to_string()),
            Some("chat-synthetic-1"),
            false,
            Path::new("/tmp/lazykatok-synthetic"),
            &mut sends,
            &mut sender,
        )
        .expect("start reply send");

        assert!(!quit);
        assert_eq!(call_count.get(), 1);
        assert_eq!(system_lines(&mut state), ["lazykatok: 전송 중…"]);

        drain_reply_send_outcomes(&mut state, &mut sends).expect("render fake completion");
        assert_eq!(
            system_lines(&mut state),
            ["lazykatok: 전송 중…", "lazykatok: sent reply (2 chars)"]
        );
    }

    #[test]
    fn reply_send_busy_keeps_draft_and_does_not_start_a_second_sender() {
        let mut state = ReplyUiState::default();
        let mut sends = ReplySendState::new();
        let call_count = Rc::new(Cell::new(0));
        let counted = Rc::clone(&call_count);
        let mut sender = move |_request: ReplySendRequest| counted.set(counted.get() + 1);

        process_reply_action(
            &mut state,
            ReplyTerminalAction::Submit("first".to_string()),
            Some("chat-synthetic-1"),
            false,
            Path::new("/tmp/lazykatok-synthetic"),
            &mut sends,
            &mut sender,
        )
        .expect("start first reply");
        process_reply_action(
            &mut state,
            ReplyTerminalAction::Submit("둘째 초안".to_string()),
            Some("chat-synthetic-1"),
            false,
            Path::new("/tmp/lazykatok-synthetic"),
            &mut sends,
            &mut sender,
        )
        .expect("reject concurrent reply");

        assert_eq!(call_count.get(), 1);
        assert_eq!(state.draft(), "둘째 초안");
        assert_eq!(
            system_lines(&mut state),
            [
                "lazykatok: 전송 중…",
                "lazykatok: 이전 전송이 끝나기를 기다리는 중입니다"
            ]
        );
    }

    #[test]
    fn reply_send_error_allows_a_subsequent_send() {
        let mut state = ReplyUiState::default();
        let mut sends = ReplySendState::new();
        let call_count = Rc::new(Cell::new(0));
        let counted = Rc::clone(&call_count);
        let mut sender = move |request: ReplySendRequest| {
            counted.set(counted.get() + 1);
            if counted.get() == 1 {
                request
                    .outcome_tx
                    .send(Err(anyhow!("synthetic send failure")))
                    .expect("return fake failure");
            }
        };

        process_reply_action(
            &mut state,
            ReplyTerminalAction::Submit("first".to_string()),
            Some("chat-synthetic-1"),
            false,
            Path::new("/tmp/lazykatok-synthetic"),
            &mut sends,
            &mut sender,
        )
        .expect("start failing reply");
        drain_reply_send_outcomes(&mut state, &mut sends).expect("render fake failure");
        process_reply_action(
            &mut state,
            ReplyTerminalAction::Submit("second".to_string()),
            Some("chat-synthetic-1"),
            false,
            Path::new("/tmp/lazykatok-synthetic"),
            &mut sends,
            &mut sender,
        )
        .expect("start subsequent reply");

        assert_eq!(call_count.get(), 2);
        assert!(system_lines(&mut state)
            .iter()
            .any(|line| line == "lazykatok: send failed: synthetic send failure"));
    }

    struct ScriptedReplyUi {
        state: ReplyUiState,
        actions: VecDeque<Vec<ReplyTerminalAction>>,
    }

    impl ReplyPollRenderer for ScriptedReplyUi {
        fn render_system(&mut self, line: String) -> anyhow::Result<()> {
            self.state.push_system(line);
            Ok(())
        }

        fn render_chat(&mut self, entry: lazykatok::watch::ChatEntry) -> anyhow::Result<()> {
            self.state.push_chat(entry);
            Ok(())
        }
    }

    impl ReplyWatchUi for ScriptedReplyUi {
        fn poll_actions(
            &mut self,
            _timeout: Duration,
        ) -> anyhow::Result<Option<Vec<ReplyTerminalAction>>> {
            Ok(self.actions.pop_front())
        }

        fn scroll(&mut self, delta: ScrollDelta) -> anyhow::Result<()> {
            self.state.scroll(delta, 20);
            Ok(())
        }

        fn restore_draft(&mut self, draft: &str) -> anyhow::Result<()> {
            self.state.paste(draft);
            Ok(())
        }
    }

    #[test]
    fn reply_pump_quits_without_joining_an_in_flight_sender() {
        let mut ui = ScriptedReplyUi {
            state: ReplyUiState::default(),
            actions: VecDeque::from([vec![
                ReplyTerminalAction::Submit("still sending".to_string()),
                ReplyTerminalAction::Quit,
            ]]),
        };
        let (poll_tx, poll_rx) = mpsc::channel();
        poll_tx
            .send(ReplyPollOutcome::Completed {
                poll: 1,
                events: vec![],
                archive_changed: false,
                observed_messages: 0,
                observed_chats: 0,
                emitted_messages: 0,
                archived_messages: 0,
                chunks: 0,
                selected_chat_name: Some("Synthetic Team".to_string()),
            })
            .expect("seed pump outcome");
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let mut release_rx = Some(release_rx);
        let mut sender = move |request: ReplySendRequest| {
            let entered_tx = entered_tx.clone();
            let done_tx = done_tx.clone();
            let release_rx = release_rx.take().expect("only one fake send");
            std::thread::spawn(move || {
                entered_tx.send(()).expect("signal fake sender");
                release_rx.recv().expect("release fake sender");
                let _ = request.outcome_tx.send(Ok(13));
                done_tx.send(()).expect("signal fake sender done");
            });
        };

        pump_reply_watch_with_sender(
            &mut ui,
            &poll_rx,
            Some("chat-synthetic-1"),
            false,
            Path::new("/tmp/lazykatok-synthetic"),
            None,
            &mut sender,
        )
        .expect("quit reply pump");

        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("fake sender started");
        assert!(matches!(done_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        assert!(system_lines(&mut ui.state)
            .iter()
            .any(|line| line == "lazykatok: 전송은 백그라운드에서 계속됩니다"));
        release_tx.send(()).expect("release fake sender");
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("fake sender finished");
    }

    #[test]
    fn reply_pump_processes_input_while_worker_read_is_pending_then_renders_outcome() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (outcome_tx, outcome_rx) = mpsc::channel();
        let (_stop_tx, stop_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            reply_poll_worker(
                worker_options(),
                outcome_tx,
                stop_rx,
                move || {
                    entered_tx.send(()).expect("signal entered read");
                    release_rx.recv().expect("release fake read");
                    Ok(vec![synthetic_message()])
                },
                |_, _| {
                    Ok(PollArchiveState {
                        archive_changed: true,
                        archived_messages: 1,
                        chunks: 1,
                    })
                },
                |_, _| Ok(()),
            );
        });

        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker entered fake read");
        assert!(matches!(
            outcome_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        let mut state = ReplyUiState::default();
        let input =
            apply_reply_input_events(&mut state, [key(KeyCode::Char('x'), KeyModifiers::NONE)]);
        assert_eq!(input.redraw, ReplyRedraw::Input);
        assert_eq!(state.frame(80, 8).input, "reply> x");

        release_tx.send(()).expect("release worker read");
        let outcome = outcome_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("receive completed poll");
        render_reply_poll_outcome(&mut state, &outcome).expect("render poll outcome");

        let frame = state.frame(80, 8);
        assert_eq!(frame.input, "reply> x");
        assert!(frame
            .conversation
            .iter()
            .any(|row| row.text.contains("Synthetic incoming message")));
        worker.join().expect("join fake worker");
    }

    #[test]
    fn reply_worker_error_is_rendered_as_a_system_line() {
        let (outcome_tx, outcome_rx) = mpsc::channel();
        let (_stop_tx, stop_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            reply_poll_worker(
                worker_options(),
                outcome_tx,
                stop_rx,
                || Err(anyhow!("synthetic read failure")),
                |_, _| unreachable!("sync must not run after a read error"),
                |_, _| unreachable!("freshness must not run after a read error"),
            );
        });

        let outcome = outcome_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("receive worker error");
        assert!(matches!(outcome, ReplyPollOutcome::Error { poll: 1, .. }));

        let mut state = ReplyUiState::default();
        render_reply_poll_outcome(&mut state, &outcome).expect("render worker error");
        assert!(state
            .frame(80, 8)
            .conversation
            .iter()
            .any(|row| row.text.contains("synthetic read failure")));
        worker.join().expect("join fake worker");
    }

    #[test]
    fn row_diff_marks_only_changed_rows_after_the_initial_batch() {
        let previous = [
            ("history".to_string(), RowStyle::Dim),
            ("reply> old".to_string(), RowStyle::Plain),
        ];
        let unchanged = ReplyRow {
            text: "history".to_string(),
            style: RowStyle::Dim,
        };
        let changed = ReplyRow {
            text: "reply> new".to_string(),
            style: RowStyle::Plain,
        };

        assert!(!reply_row_changed(Some(&previous[0]), &unchanged));
        assert!(reply_row_changed(Some(&previous[1]), &changed));
        assert!(reply_row_changed(None, &unchanged));
    }

    #[test]
    fn rapid_ascii_and_korean_input_is_applied_as_one_redraw_batch() {
        let mut state = ReplyUiState::default();
        let events = "rapid 확인 중"
            .chars()
            .map(|character| key(KeyCode::Char(character), KeyModifiers::NONE));

        let batch = apply_reply_input_events(&mut state, events);

        assert_eq!(batch.redraw, ReplyRedraw::Input);
        assert!(batch.actions.is_empty());
        assert_eq!(state.frame(80, 5).input, "reply> rapid 확인 중");
    }

    #[test]
    fn enter_code_and_cr_lf_character_variants_submit() {
        for enter in [
            key(KeyCode::Enter, KeyModifiers::NONE),
            key(KeyCode::Char('\r'), KeyModifiers::CONTROL),
            key(KeyCode::Char('\n'), KeyModifiers::NONE),
        ] {
            let mut state = ReplyUiState::default();
            state.paste("확인");

            let batch = apply_reply_input_events(&mut state, [enter]);

            assert_eq!(
                batch.actions,
                [ReplyTerminalAction::Submit("확인".to_string())]
            );
            assert_eq!(state.frame(80, 5).input, "reply> ");
        }
    }

    #[test]
    fn paste_newlines_do_not_submit_and_a_later_enter_does() {
        let mut state = ReplyUiState::default();
        let paste =
            apply_reply_input_events(&mut state, [Event::Paste("first\r\n둘째".to_string())]);

        assert!(paste.actions.is_empty());
        assert_eq!(state.frame(80, 5).input, "reply> first  둘째");

        let enter = apply_reply_input_events(&mut state, [key(KeyCode::Enter, KeyModifiers::NONE)]);
        assert_eq!(
            enter.actions,
            [ReplyTerminalAction::Submit("first  둘째".to_string())]
        );
    }

    #[test]
    fn backspace_resize_and_incoming_history_preserve_the_draft() {
        let mut state = ReplyUiState::default();
        let first = apply_reply_input_events(
            &mut state,
            "abc한"
                .chars()
                .map(|character| key(KeyCode::Char(character), KeyModifiers::NONE)),
        );
        assert_eq!(first.redraw, ReplyRedraw::Input);

        state.push_line("[now] Synthetic Room / Tester: incoming");
        let second = apply_reply_input_events(
            &mut state,
            [
                Event::Resize(40, 8),
                key(KeyCode::Backspace, KeyModifiers::NONE),
                key(KeyCode::Char('글'), KeyModifiers::NONE),
            ],
        );

        assert_eq!(second.redraw, ReplyRedraw::Full);
        let frame = state.frame(40, 8);
        assert_eq!(frame.input, "reply> abc글");
        assert_eq!(frame.conversation.len(), 1);
        assert_eq!(
            frame.conversation[0].text,
            "[now] Synthetic Room / Tester: incoming"
        );
    }

    #[test]
    fn printable_alt_characters_are_kept_but_control_commands_remain_explicit() {
        let mut state = ReplyUiState::default();
        let batch = apply_reply_input_events(
            &mut state,
            [
                key(KeyCode::Char('é'), KeyModifiers::ALT),
                key(KeyCode::Char('A'), KeyModifiers::SHIFT),
                key(KeyCode::Char('x'), KeyModifiers::CONTROL),
                key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            ],
        );

        assert_eq!(state.frame(80, 5).input, "reply> éA");
        assert_eq!(batch.actions, [ReplyTerminalAction::Quit]);
    }

    #[test]
    fn movement_delete_kill_and_scroll_keys_apply_expected_actions() {
        let mut state = ReplyUiState::default();
        state.paste("one two");
        let batch = apply_reply_input_events(
            &mut state,
            [
                key(KeyCode::Left, KeyModifiers::NONE),
                key(KeyCode::Char('b'), KeyModifiers::CONTROL),
                key(KeyCode::Delete, KeyModifiers::NONE),
                key(KeyCode::Char('w'), KeyModifiers::CONTROL),
                key(KeyCode::Home, KeyModifiers::NONE),
                key(KeyCode::Char('k'), KeyModifiers::CONTROL),
                key(KeyCode::Up, KeyModifiers::NONE),
                key(KeyCode::PageDown, KeyModifiers::NONE),
            ],
        );

        assert_eq!(state.draft(), "");
        assert_eq!(batch.redraw, ReplyRedraw::Input);
        assert_eq!(
            batch.actions,
            [
                ReplyTerminalAction::Scroll(ScrollDelta::Rows(-1)),
                ReplyTerminalAction::Scroll(ScrollDelta::Pages(1)),
            ]
        );
    }

    #[test]
    fn control_a_b_e_f_match_home_left_end_right() {
        let mut state = ReplyUiState::default();
        state.paste("abc");
        apply_reply_input_events(
            &mut state,
            [
                key(KeyCode::Char('a'), KeyModifiers::CONTROL),
                key(KeyCode::Char('f'), KeyModifiers::CONTROL),
                key(KeyCode::Char('e'), KeyModifiers::CONTROL),
                key(KeyCode::Char('b'), KeyModifiers::CONTROL),
                key(KeyCode::Char('u'), KeyModifiers::CONTROL),
            ],
        );
        assert_eq!(state.draft(), "c");
        assert_eq!(state.cursor(), 0);
    }

    #[test]
    fn release_events_are_ignored() {
        let mut state = ReplyUiState::default();
        let event = Event::Key(KeyEvent {
            code: KeyCode::Char('x'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        });

        let batch = apply_reply_input_events(&mut state, [event]);

        assert_eq!(batch.redraw, ReplyRedraw::None);
        assert!(batch.actions.is_empty());
        assert_eq!(state.frame(80, 5).input, "reply> ");
    }
}

#[cfg(all(test, target_os = "macos", feature = "private-send"))]
mod tests {
    use super::{remaining_poll_delay, reply_send_args, send_child_error_detail};
    use std::ffi::OsStr;
    use std::path::Path;
    use std::time::{Duration, Instant};

    #[test]
    fn poll_delay_preserves_start_to_start_interval_and_saturates_on_overrun() {
        let poll_started = Instant::now();
        let deadline = poll_started + Duration::from_secs(2);
        assert_eq!(
            remaining_poll_delay(deadline, poll_started + Duration::from_millis(750)),
            Duration::from_millis(1_250)
        );
        assert_eq!(
            remaining_poll_delay(deadline, poll_started + Duration::from_millis(2_500)),
            Duration::ZERO
        );
    }

    #[test]
    fn send_child_error_detail_prefers_stderr() {
        let detail = send_child_error_detail(
            "exit status: 1",
            br#"{"ok":false,"error":{"message":"stdout message","cause":"stdout cause"}}"#,
            b"stderr cause\n",
        );

        assert_eq!(detail, "stderr cause");
    }

    #[test]
    fn send_child_error_detail_reads_json_stdout_cause() {
        let detail = send_child_error_detail(
            "exit status: 1",
            br#"{
              "ok": false,
              "error": {
                "message": "command failed",
                "cause": "no chat chat-group-1 in the archive; run sync first"
              }
            }"#,
            b"",
        );

        assert_eq!(
            detail,
            "no chat chat-group-1 in the archive; run sync first"
        );
    }

    #[test]
    fn send_child_error_detail_falls_back_to_status() {
        let detail = send_child_error_detail("exit status: 1", b"", b"");

        assert_eq!(detail, "lazykatok send failed with exit status: 1");
    }

    #[test]
    fn reply_send_args_makes_no_open_replies_strictly_background_only() {
        let default_args = reply_send_args("chat-group-1", false, Path::new("/tmp/katok-data"));
        let no_open_args = reply_send_args("chat-group-1", true, Path::new("/tmp/katok-data"));

        assert!(!default_args
            .iter()
            .any(|arg| arg.as_os_str() == OsStr::new("--no-open")));
        assert!(!default_args
            .iter()
            .any(|arg| arg.as_os_str() == OsStr::new("--background-only")));
        assert!(default_args.windows(2).any(|args| {
            args[0].as_os_str() == OsStr::new("--focus-wait")
                && args[1].as_os_str() == OsStr::new("2")
        }));
        assert!(no_open_args
            .iter()
            .any(|arg| arg.as_os_str() == OsStr::new("--no-open")));
        assert!(no_open_args
            .iter()
            .any(|arg| arg.as_os_str() == OsStr::new("--background-only")));
        assert!(!no_open_args
            .iter()
            .any(|arg| arg.as_os_str() == OsStr::new("--focus-wait")));
    }
}

fn macos_probe_payload(enabled: bool, data_dir: &Path) -> serde_json::Value {
    if !enabled {
        return serde_json::json!({
            "status": "not_checked",
            "reason": "run katok doctor --macos-probe --json to check KakaoTalk app data access"
        });
    }
    match dirs::home_dir() {
        Some(home) => {
            let status = lazykatok::kakao::probe_status(&home, data_dir);
            serde_json::json!({
                "status": "checked",
                "app_installed": status.app_installed,
                "container_present": status.container_present,
                "db_file_count": status.db_file_count,
                "auth_cached": status.auth_cached
            })
        }
        None => serde_json::json!({ "status": "home_unavailable" }),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_sync(
    source: &str,
    path: Option<PathBuf>,
    json: bool,
    include_touched: bool,
    prune_preview: bool,
    prune_deleted: bool,
    config: &KatokConfig,
    archive_path: &Path,
    data_dir: &Path,
) -> Result<()> {
    let adapter = adapter_for_source(source, path, data_dir)?;
    let read_started = Instant::now();
    let messages = adapter.messages().context("read source messages")?;
    let read_source = read_started.elapsed().as_millis();
    let archive = Archive::open(archive_path).context("open archive")?;
    // Message upserts and the chunk rebuild are one unit: chunks derived from half-written
    // messages are not a usable archive state, so either both land or neither does.
    let report = archive.in_transaction(|| {
        let upsert_started = Instant::now();
        let mut report = archive.sync_messages(&messages).context("sync messages")?;
        let upsert_messages = upsert_started.elapsed().as_millis();

        // Before chunking, so a removed message never reaches a chunk. Inside
        // the same transaction, so a failure anywhere leaves nothing deleted.
        let pruned = if prune_preview || prune_deleted {
            let doomed = archive
                .reconcile_deletions(&messages, prune_deleted)
                .context("reconcile deletions")?;
            if prune_deleted {
                // Their chats have to be rechunked for the removal to reach the
                // chunk text and the index, not just the messages table.
                for chat_id in doomed
                    .iter()
                    .map(|d| d.chat_id.as_str())
                    .collect::<std::collections::BTreeSet<_>>()
                {
                    if let Some(touched) = report
                        .touched_chats
                        .iter_mut()
                        .find(|candidate| candidate.chat_id == chat_id)
                    {
                        // A later edit may already have put this chat in the
                        // report with a tail floor after the deleted row. A
                        // deletion has no surviving row that can safely anchor
                        // a cut, so widen that existing touch to the whole chat.
                        touched.earliest_changed_timestamp.clear();
                        touched.earliest_changed_message_id.clear();
                    } else {
                        report.touched_chats.push(lazykatok::types::TouchedChat {
                            chat_id: chat_id.to_string(),
                            earliest_changed_timestamp: String::new(),
                            earliest_changed_message_id: String::new(),
                        });
                    }
                }
                report.rebuilt_chats = report.touched_chats.len();
                report.total_messages = archive.message_count().context("count messages")?;
            }
            doomed
        } else {
            Vec::new()
        };

        let rebuild_started = Instant::now();
        let settings = ChunkSettings {
            group_gap_seconds: config.chunk_gap_group_seconds,
            direct_gap_seconds: config.chunk_gap_direct_seconds,
        };
        // Recompute only the chats that changed. Three cases still need the full pass: a first
        // sync has no chunks to scope to, a gap-settings change invalidates every existing
        // chunk, and a chunker-version bump does the same — which includes the first run
        // against an archive written before the version was recorded. Without these checks a
        // settings or algorithm change would only ever reach rooms that happened to receive a
        // message, leaving the rest on the old boundaries forever.
        let stored_settings = archive
            .stored_chunk_settings()
            .context("read chunk settings")?;
        let settings_changed = stored_settings
            != Some((
                settings.group_gap_seconds,
                settings.direct_gap_seconds,
                CHUNKER_VERSION,
            ));
        report.chunks = if archive.chunk_count().context("count chunks")? == 0 || settings_changed {
            rebuild_chunks_with_settings(&archive, settings).context("rebuild chunks")?
        } else {
            rebuild_chunks_for_chats(&archive, settings, &report.touched_chats)
                .context("rebuild chunks")?
        };
        archive
            .record_chunk_settings(
                settings.group_gap_seconds,
                settings.direct_gap_seconds,
                CHUNKER_VERSION,
            )
            .context("record chunk settings")?;

        report.timings_ms = SyncTimings {
            read_source,
            upsert_messages,
            rebuild_chunks: rebuild_started.elapsed().as_millis(),
        };
        Ok::<_, anyhow::Error>((report, pruned))
    })?;
    let (mut report, pruned) = report;
    // Gate is output-only: the archive always computed touched_chats for the rebuild above.
    report.include_touched = include_touched;
    freshness::record_sync(data_dir, source, report.total_messages, report.chunks)?;

    if prune_preview || prune_deleted {
        let mut payload = serde_json::to_value(&report).context("serialize sync report")?;
        if let Some(map) = payload.as_object_mut() {
            map.insert(
                if prune_deleted {
                    "pruned_messages".to_string()
                } else {
                    "prunable_messages".to_string()
                },
                serde_json::to_value(&pruned).context("serialize pruned")?,
            );
        }
        return print_payload(json, &payload);
    }
    print_payload(json, &report)
}

fn run_transcript(
    chat: &str,
    since: Option<&str>,
    out: Option<PathBuf>,
    json: bool,
    archive_path: &Path,
    data_dir: &Path,
) -> Result<()> {
    let archive = Archive::open(archive_path).context("open archive")?;
    // Transcripts hold raw message bodies, so they default under the katok data dir rather than
    // the working directory, where they could be committed by accident.
    let out_dir = out.unwrap_or_else(|| data_dir.join("transcripts"));
    let report = export_transcript(&archive, chat, since, &out_dir).context("export transcript")?;
    print_payload(json, &report)
}

fn run_search(
    command: SearchCommand,
    config: &KatokConfig,
    archive_path: &Path,
    semantic_dir: &Path,
) -> Result<()> {
    let archive = Archive::open(archive_path).context("open archive")?;
    match command {
        SearchCommand::Keyword { query, limit, json } => {
            let hits = keyword_search_with_snippet(&archive, &query, limit, config.snippet_length)
                .context("keyword search")?;
            print_payload(json, &hits)
        }
        SearchCommand::Bm25 { query, limit, json } => {
            let hits = bm25_search_with_snippet(&archive, &query, limit, config.snippet_length)
                .context("bm25 search")?;
            print_payload(json, &hits)
        }
        SearchCommand::Semantic { query, limit, json } => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("create semantic runtime")?;
            let hits = runtime
                .block_on(semantic_search_live_with_config(
                    &archive,
                    semantic_dir,
                    &query,
                    limit,
                    config,
                ))
                .context("semantic search")?;
            print_payload(json, &hits)
        }
        SearchCommand::Hybrid { query, limit, json } => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("create semantic runtime")?;
            let hits = runtime
                .block_on(hybrid_search_with_config(
                    &archive,
                    semantic_dir,
                    &query,
                    limit,
                    config,
                ))
                .context("hybrid search")?;
            print_payload(json, &hits)
        }
    }
}

fn run_source(command: SourceCommand, config: &KatokConfig, data_dir: &Path) -> Result<()> {
    match command {
        SourceCommand::Chats { source, path, json } => {
            let source = source.unwrap_or_else(|| config.source_adapter.clone());
            let adapter = adapter_for_source(&source, path, data_dir)?;
            let chats = adapter.chats().context("list source chats")?;
            print_payload(json, &chats)
        }
    }
}

fn run_chunks(chat: &str, json: bool, archive_path: &Path) -> Result<()> {
    let archive = Archive::open(archive_path).context("open archive")?;
    let chunks = archive.chunks_for_chat(chat).context("list chunks")?;
    print_payload(json, &chunks)
}

fn run_wipe_index(yes: bool, json: bool, semantic_dir: &Path) -> Result<()> {
    if !yes {
        anyhow::bail!("refusing to wipe semantic index without --yes");
    }
    if semantic_dir.exists() {
        std::fs::remove_dir_all(semantic_dir).context("remove semantic index")?;
    }
    print_payload(json, &serde_json::json!({"semantic_removed": true}))
}
