use crate::types::RawMessage;
use chrono::{DateTime, Local, SecondsFormat, Utc};
use serde::Serialize;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub const WATCH_EVENT_SCHEMA_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchMessageChange {
    Existing,
    Inserted,
    Updated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WatchEvent {
    State {
        schema_version: u8,
        state: WatchState,
        poll: u64,
        observed_messages: usize,
        observed_chats: usize,
        emitted_messages: usize,
        archived_messages: Option<usize>,
        chunks: Option<usize>,
    },
    Message {
        schema_version: u8,
        change: WatchMessageChange,
        poll: u64,
        message: RawMessage,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchState {
    Started,
    Reading,
    Synced,
    Idle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyCommand {
    Send(String),
    Quit,
    Help,
    Empty,
    Ignored,
}

const REPLY_HISTORY_LIMIT: usize = 2_000;
pub const REPLY_PROMPT: &str = "reply> ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatEntry {
    pub timestamp: DateTime<Utc>,
    pub sender: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryEntry {
    Chat(ChatEntry),
    System(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowStyle {
    Plain,
    Dim,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyRow {
    pub text: String,
    pub style: RowStyle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollDelta {
    Rows(isize),
    Pages(isize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyInputFrame {
    pub input: String,
    pub cursor_column: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyFrame {
    pub conversation: Vec<ReplyRow>,
    pub separator: String,
    pub input: String,
    pub cursor_column: u16,
    pub footer: String,
}

#[derive(Debug, Clone)]
struct WrappedEntry {
    width: usize,
    rows: Vec<ReplyRow>,
}

#[derive(Debug, Clone)]
struct StoredEntry {
    id: u64,
    entry: HistoryEntry,
    grouped: bool,
    day_separator: Option<String>,
    wrapped: Option<WrappedEntry>,
}

/// Terminal-independent state for the interactive reply surface.
#[derive(Debug, Clone, Default)]
pub struct ReplyUiState {
    history: VecDeque<StoredEntry>,
    draft: Vec<char>,
    cursor: usize,
    next_id: u64,
    layout_width: Option<usize>,
    cumulative_rows: Vec<usize>,
    total_rows: usize,
    scroll: Option<usize>,
    new_since_scroll: usize,
    wrap_computations: usize,
}

impl ReplyUiState {
    pub fn push_chat(&mut self, mut chat: ChatEntry) {
        chat.sender = sanitize_terminal_text(&chat.sender);
        chat.text = sanitize_terminal_text(&chat.text);
        let (grouped, day_separator) = self.chat_context(&chat);
        self.push_entry(HistoryEntry::Chat(chat), grouped, day_separator, true);
    }

    pub fn push_system(&mut self, line: impl Into<String>) {
        self.push_entry(
            HistoryEntry::System(sanitize_terminal_text(&line.into())),
            false,
            None,
            false,
        );
    }

    /// Compatibility alias for status producers and the standalone benchmark.
    pub fn push_line(&mut self, line: impl Into<String>) {
        self.push_system(line);
    }

    fn push_entry(
        &mut self,
        entry: HistoryEntry,
        grouped: bool,
        day_separator: Option<String>,
        is_chat: bool,
    ) {
        if self.scroll.is_some() && is_chat {
            self.new_since_scroll = self.new_since_scroll.saturating_add(1);
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let mut stored = StoredEntry {
            id,
            entry,
            grouped,
            day_separator,
            wrapped: None,
        };
        if self.history.len() < REPLY_HISTORY_LIMIT {
            if let Some(width) = self.layout_width {
                let wrapped = wrap_history_entry(&stored, width);
                self.wrap_computations += 1;
                self.total_rows += wrapped.rows.len();
                self.cumulative_rows.push(self.total_rows);
                stored.wrapped = Some(wrapped);
            }
            self.history.push_back(stored);
            return;
        }

        let removed_rows = self
            .history
            .pop_front()
            .and_then(|entry| entry.wrapped.map(|wrapped| wrapped.rows.len()))
            .unwrap_or(0);
        if let Some(top) = self.scroll.as_mut() {
            *top = top.saturating_sub(removed_rows);
        }
        if let Some(first) = self.history.front_mut() {
            if first.grouped || first.day_separator.is_some() {
                first.grouped = false;
                first.day_separator = None;
                first.wrapped = None;
            }
        }
        self.history.push_back(stored);
        self.layout_width = None;
    }

    fn chat_context(&self, next: &ChatEntry) -> (bool, Option<String>) {
        let Some(previous) = self.history.back() else {
            return (false, None);
        };
        let HistoryEntry::Chat(previous) = &previous.entry else {
            return (false, None);
        };
        let previous_local = previous.timestamp.with_timezone(&Local);
        let next_local = next.timestamp.with_timezone(&Local);
        let day_changed = previous_local.date_naive() != next_local.date_naive();
        let gap = next.timestamp.signed_duration_since(previous.timestamp);
        let grouped = !day_changed
            && previous.sender == next.sender
            && gap.num_seconds() >= 0
            && gap.num_seconds() <= 5 * 60;
        let separator = day_changed.then(|| next_local.format("%Y-%m-%d").to_string());
        (grouped, separator)
    }

    pub fn insert(&mut self, character: char) {
        if !character.is_control() {
            self.draft.insert(self.cursor, character);
            self.cursor += 1;
        }
    }

    /// Paste is insertion only. Newlines become spaces and cannot submit the draft.
    pub fn paste(&mut self, text: &str) {
        for character in text.chars() {
            if matches!(character, '\r' | '\n') {
                self.insert(' ');
            } else if !character.is_control() {
                self.insert(character);
            }
        }
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.draft.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.draft.len() {
            self.draft.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.draft.len());
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.draft.len();
    }

    pub fn ctrl_w(&mut self) {
        let mut start = self.cursor;
        while start > 0 && self.draft[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !self.draft[start - 1].is_whitespace() {
            start -= 1;
        }
        self.draft.drain(start..self.cursor);
        self.cursor = start;
    }

    pub fn ctrl_u(&mut self) {
        self.draft.drain(..self.cursor);
        self.cursor = 0;
    }

    pub fn ctrl_k(&mut self) {
        self.draft.truncate(self.cursor);
    }

    pub fn take_draft(&mut self) -> String {
        self.cursor = 0;
        self.draft.drain(..).collect()
    }

    pub fn draft(&self) -> String {
        self.draft.iter().collect()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_pinned_to_bottom(&self) -> bool {
        self.scroll.is_none()
    }

    pub fn new_since_scroll(&self) -> usize {
        self.new_since_scroll
    }

    #[cfg(test)]
    pub(crate) fn wrap_computations(&self) -> usize {
        self.wrap_computations
    }

    pub fn scroll(&mut self, delta: ScrollDelta, terminal_height: u16) {
        let page = terminal_height.saturating_sub(3).saturating_sub(1).max(1) as isize;
        let amount = match delta {
            ScrollDelta::Rows(rows) => rows,
            ScrollDelta::Pages(pages) => pages.saturating_mul(page),
        };
        let viewport = terminal_height.saturating_sub(3) as usize;
        let bottom = self.total_rows.saturating_sub(viewport);
        let current = self.scroll.unwrap_or(bottom);
        let next = if amount < 0 {
            current.saturating_sub(amount.unsigned_abs())
        } else {
            current.saturating_add(amount as usize).min(bottom)
        };
        if next >= bottom {
            self.scroll = None;
            self.new_since_scroll = 0;
        } else {
            self.scroll = Some(next);
        }
    }

    pub fn input_frame(&self, terminal_width: u16) -> ReplyInputFrame {
        let width = terminal_width.saturating_sub(1).max(1) as usize;
        let prompt_width = UnicodeWidthStr::width(REPLY_PROMPT);
        if prompt_width >= width {
            return ReplyInputFrame {
                input: display_prefix(REPLY_PROMPT, width),
                cursor_column: width.try_into().unwrap_or(u16::MAX),
            };
        }
        let available = width - prompt_width;
        let mut start = 0;
        let before_width = chars_width(&self.draft[..self.cursor]);
        if before_width > available {
            start = self.cursor;
            let mut used = 0;
            while start > 0 {
                let candidate = char_width(self.draft[start - 1]);
                if used + candidate > available {
                    break;
                }
                start -= 1;
                used += candidate;
            }
        }
        let mut end = start;
        let mut used = 0;
        while end < self.draft.len() {
            let candidate = char_width(self.draft[end]);
            if used + candidate > available {
                break;
            }
            used += candidate;
            end += 1;
        }
        let visible: String = self.draft[start..end].iter().collect();
        let cursor_column = (prompt_width + chars_width(&self.draft[start..self.cursor]))
            .min(width)
            .try_into()
            .unwrap_or(u16::MAX);
        ReplyInputFrame {
            input: format!("{REPLY_PROMPT}{visible}"),
            cursor_column,
        }
    }

    pub fn frame(&mut self, terminal_width: u16, terminal_height: u16) -> ReplyFrame {
        let width = terminal_width.saturating_sub(1).max(1) as usize;
        self.ensure_layout(width);
        let conversation_height = terminal_height.saturating_sub(3) as usize;
        let marker = self.scroll.is_some() && self.new_since_scroll > 0 && conversation_height > 0;
        let row_capacity = conversation_height.saturating_sub(usize::from(marker));
        let bottom = self.total_rows.saturating_sub(row_capacity);
        let start = self.scroll.unwrap_or(bottom).min(bottom);
        if let Some(top) = self.scroll.as_mut() {
            *top = start;
        }
        let end = (start + row_capacity).min(self.total_rows);
        let mut conversation = self.visible_rows(start, end);
        if marker {
            conversation.push(ReplyRow {
                text: centered_rule(&format!("{} new messages", self.new_since_scroll), width),
                style: RowStyle::Dim,
            });
        }
        let input = self.input_frame(terminal_width);
        ReplyFrame {
            conversation,
            separator: "─".repeat(width),
            input: input.input,
            cursor_column: input.cursor_column,
            footer: display_prefix(
                "↑↓ scroll  PgUp/PgDn page  Enter send  Ctrl-C quit  /help",
                width,
            ),
        }
    }

    fn ensure_layout(&mut self, width: usize) {
        if self.layout_width == Some(width) && self.cumulative_rows.len() == self.history.len() {
            return;
        }
        let mut cumulative = Vec::with_capacity(self.history.len());
        let mut total = 0usize;
        for stored in &mut self.history {
            let needs_wrap = stored
                .wrapped
                .as_ref()
                .is_none_or(|wrapped| wrapped.width != width);
            if needs_wrap {
                stored.wrapped = Some(wrap_history_entry(stored, width));
                self.wrap_computations += 1;
            }
            total += stored.wrapped.as_ref().expect("wrapped entry").rows.len();
            cumulative.push(total);
        }
        self.cumulative_rows = cumulative;
        self.total_rows = total;
        self.layout_width = Some(width);
    }

    fn visible_rows(&self, start: usize, end: usize) -> Vec<ReplyRow> {
        if start >= end || self.history.is_empty() {
            return Vec::new();
        }
        let first = self
            .cumulative_rows
            .partition_point(|&row_end| row_end <= start);
        let mut output = Vec::with_capacity(end - start);
        for index in first..self.history.len() {
            let entry_start = if index == 0 {
                0
            } else {
                self.cumulative_rows[index - 1]
            };
            if entry_start >= end {
                break;
            }
            let rows = &self.history[index]
                .wrapped
                .as_ref()
                .expect("layout ensured")
                .rows;
            let local_start = start.saturating_sub(entry_start);
            let local_end = (end - entry_start).min(rows.len());
            output.extend(rows[local_start..local_end].iter().cloned());
        }
        output
    }
}

fn wrap_history_entry(stored: &StoredEntry, width: usize) -> WrappedEntry {
    let _identity = stored.id;
    let mut rows = Vec::new();
    if let Some(date) = &stored.day_separator {
        rows.push(ReplyRow {
            text: centered_rule(date, width),
            style: RowStyle::Dim,
        });
    }
    match &stored.entry {
        HistoryEntry::System(text) => rows.extend(wrap_display_line(text, width).into_iter().map(
            |text| ReplyRow {
                text,
                style: RowStyle::Dim,
            },
        )),
        HistoryEntry::Chat(chat) => rows.extend(wrap_chat(chat, stored.grouped, width)),
    }
    WrappedEntry { width, rows }
}

fn wrap_chat(chat: &ChatEntry, grouped: bool, width: usize) -> Vec<ReplyRow> {
    let time = chat.timestamp.with_timezone(&Local).format("%H:%M");
    let prefix = if width < 24 {
        let sender = if grouped { "·" } else { &chat.sender };
        let delimiter = if grouped { " " } else { ": " };
        format!("{time} {sender}{delimiter}")
    } else {
        let sender_width = if width < 40 { width / 3 } else { 12 };
        let sender = if grouped {
            pad_display("·", sender_width)
        } else {
            pad_display(&chat.sender, sender_width)
        };
        format!("{time} {sender} ")
    };
    wrap_prefixed(&prefix, &chat.text, width)
}

fn wrap_prefixed(prefix: &str, body: &str, width: usize) -> Vec<ReplyRow> {
    let prefix_width = UnicodeWidthStr::width(prefix);
    if prefix_width >= width {
        return wrap_display_line(&format!("{prefix}{body}"), width)
            .into_iter()
            .map(|text| ReplyRow {
                text,
                style: RowStyle::Plain,
            })
            .collect();
    }
    let body_width = width - prefix_width;
    let wrapped = wrap_display_line(body, body_width);
    wrapped
        .into_iter()
        .enumerate()
        .map(|(index, text)| ReplyRow {
            text: if index == 0 {
                format!("{prefix}{text}")
            } else {
                format!("{}{text}", " ".repeat(prefix_width))
            },
            style: RowStyle::Plain,
        })
        .collect()
}

fn wrap_display_line(line: &str, width: usize) -> Vec<String> {
    if line.is_empty() || width == 0 {
        return vec![String::new()];
    }
    let mut rows = vec![String::new()];
    let mut row_width = 0usize;
    for character in line.chars() {
        let character_width = char_width(character);
        if character_width > width {
            continue;
        }
        if row_width > 0 && row_width + character_width > width {
            rows.push(String::new());
            row_width = 0;
        }
        rows.last_mut().expect("one reply row").push(character);
        row_width += character_width;
    }
    rows
}

fn centered_rule(label: &str, width: usize) -> String {
    let label = format!("  {label}  ");
    let label_width = UnicodeWidthStr::width(label.as_str());
    if label_width >= width {
        return display_prefix(&label, width);
    }
    let remaining = width - label_width;
    format!(
        "{}{}{}",
        "─".repeat(remaining / 2),
        label,
        "─".repeat(remaining - remaining / 2)
    )
}

fn pad_display(value: &str, width: usize) -> String {
    let current = UnicodeWidthStr::width(value);
    if current <= width {
        return format!("{value}{}", " ".repeat(width - current));
    }
    if width == 0 {
        return String::new();
    }
    let mut shortened = display_prefix(value, width.saturating_sub(1));
    shortened.push('…');
    let shortened_width = UnicodeWidthStr::width(shortened.as_str());
    shortened.push_str(&" ".repeat(width.saturating_sub(shortened_width)));
    shortened
}

fn char_width(character: char) -> usize {
    UnicodeWidthChar::width(character).unwrap_or(0)
}

fn chars_width(characters: &[char]) -> usize {
    characters.iter().copied().map(char_width).sum()
}

fn display_prefix(value: &str, width: usize) -> String {
    let mut output = String::new();
    let mut used = 0usize;
    for character in value.chars() {
        let character_width = char_width(character);
        if used + character_width > width {
            break;
        }
        output.push(character);
        used += character_width;
    }
    output
}

pub fn parse_reply_command(bytes_read: usize, line: &str) -> ReplyCommand {
    if bytes_read == 0 {
        return ReplyCommand::Quit;
    }

    let trimmed = line.trim();
    if trimmed.is_empty() {
        ReplyCommand::Empty
    } else if matches!(trimmed, "/quit" | "/exit" | "/q") {
        ReplyCommand::Quit
    } else if matches!(trimmed, "/help" | "/?") {
        ReplyCommand::Help
    } else if let Some(rest) = trimmed.strip_prefix("/send") {
        if rest.is_empty() {
            return ReplyCommand::Empty;
        }
        if !rest.chars().next().is_some_and(char::is_whitespace) {
            return ReplyCommand::Ignored;
        }
        let body = rest.trim();
        if body.is_empty() {
            ReplyCommand::Empty
        } else {
            ReplyCommand::Send(body.to_string())
        }
    } else if trimmed.starts_with('/') {
        ReplyCommand::Ignored
    } else {
        ReplyCommand::Send(trimmed.to_string())
    }
}

#[derive(Debug, Clone, Default)]
pub struct WatchSnapshot {
    initialized: bool,
    source_changed: bool,
    fingerprints: BTreeMap<MessageKey, MessageFingerprint>,
    /// Indexes, into the slice last passed to `diff`, of the messages that
    /// were new or changed in any chat. `None` after the first diff, when the
    /// whole slice is new to the archive.
    archive_changes: Option<Vec<usize>>,
}

impl WatchSnapshot {
    pub fn diff(
        &mut self,
        messages: &[RawMessage],
        poll: u64,
        replay_existing: bool,
        chat_filter: Option<&str>,
    ) -> Vec<WatchEvent> {
        let initial = !self.initialized;
        let mut next = BTreeMap::new();
        let mut changed = Vec::new();
        let mut archive_changes = Vec::new();

        for (index, message) in messages.iter().enumerate() {
            let key = MessageKey::from(message);
            let fingerprint = MessageFingerprint::from(message);
            let previous = self.fingerprints.get(&key);
            if previous != Some(&fingerprint) {
                archive_changes.push(index);
            }
            next.insert(key, fingerprint.clone());

            if chat_filter.is_some_and(|chat_id| chat_id != message.chat_id.as_str()) {
                continue;
            }

            let change = match previous {
                None if initial && replay_existing => Some(WatchMessageChange::Existing),
                None if !initial => Some(WatchMessageChange::Inserted),
                Some(old) if old != &fingerprint => Some(WatchMessageChange::Updated),
                _ => None,
            };

            if let Some(change) = change {
                changed.push((message_order_key(message), change, message.clone()));
            }
        }

        self.source_changed = initial || self.fingerprints != next;
        self.fingerprints = next;
        self.initialized = true;
        self.archive_changes = (!initial).then_some(archive_changes);

        changed.sort_by(|left, right| left.0.cmp(&right.0));
        changed
            .into_iter()
            .map(|(_, change, message)| WatchEvent::Message {
                schema_version: WATCH_EVENT_SCHEMA_VERSION,
                change,
                poll,
                message,
            })
            .collect()
    }

    /// Record a poll that skipped reading because the source was untouched.
    pub fn unchanged(&mut self) {
        self.source_changed = false;
        self.archive_changes = Some(Vec::new());
    }

    /// The messages from the last `diff` that the archive has not stored yet:
    /// all of them on the first diff, then only those new or changed since.
    /// Upserting every message on each change cost seconds on large accounts.
    pub fn archive_batch<'a>(&self, messages: &'a [RawMessage]) -> Cow<'a, [RawMessage]> {
        match &self.archive_changes {
            None => Cow::Borrowed(messages),
            Some(indexes) => Cow::Owned(
                indexes
                    .iter()
                    .map(|&index| messages[index].clone())
                    .collect(),
            ),
        }
    }

    pub fn seen_count(&self) -> usize {
        self.fingerprints.len()
    }

    pub fn source_changed(&self) -> bool {
        self.source_changed
    }
}

fn message_order_key(message: &RawMessage) -> (String, String, String) {
    (
        message.timestamp.to_rfc3339(),
        message.chat_id.clone(),
        message.message_id.clone(),
    )
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct MessageKey {
    account_hash: String,
    chat_id: String,
    message_id: String,
}

impl From<&RawMessage> for MessageKey {
    fn from(message: &RawMessage) -> Self {
        Self {
            account_hash: message.account_hash.clone(),
            chat_id: message.chat_id.clone(),
            message_id: message.message_id.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MessageFingerprint {
    chat_name: String,
    chat_type: String,
    sender_id: String,
    sender_nickname: String,
    timestamp: String,
    text: String,
    message_type: String,
    reply_to_message_id: Option<String>,
}

impl From<&RawMessage> for MessageFingerprint {
    fn from(message: &RawMessage) -> Self {
        Self {
            chat_name: message.chat_name.clone(),
            chat_type: message.chat_type.clone(),
            sender_id: message.sender_id.clone(),
            sender_nickname: message.sender_nickname.clone(),
            timestamp: message.timestamp.to_rfc3339(),
            text: message.text.clone(),
            message_type: message.message_type.clone(),
            reply_to_message_id: message.reply_to_message_id.clone(),
        }
    }
}

pub fn chat_count(messages: &[RawMessage]) -> usize {
    messages
        .iter()
        .map(|message| message.chat_id.as_str())
        .collect::<BTreeSet<_>>()
        .len()
}

pub fn format_human_message_line(change: WatchMessageChange, message: &RawMessage) -> String {
    let timestamp = message
        .timestamp
        .to_rfc3339_opts(SecondsFormat::Secs, true)
        .replace('T', " ")
        .replace('Z', " UTC");
    let room = sanitize_terminal_text(&message.chat_name);
    let sender = if message.sender_nickname.trim().is_empty() {
        sanitize_terminal_text(&message.sender_id)
    } else {
        sanitize_terminal_text(&message.sender_nickname)
    };
    let body = if message.text.trim().is_empty() {
        format!("<{}>", sanitize_terminal_text(&message.message_type))
    } else {
        sanitize_terminal_text(&message.text)
    };
    let change_label = match change {
        WatchMessageChange::Existing | WatchMessageChange::Inserted => "",
        WatchMessageChange::Updated => " (updated)",
    };

    format!("[{timestamp}] {room} / {sender}{change_label}: {body}")
}

/// Convert untrusted chat/status text into one inert terminal line.
///
/// Terminal escape protocols require control characters such as ESC, BEL, CR, or C1 controls.
/// Replacing every Unicode control before folding whitespace prevents chat content from moving
/// the cursor, changing terminal state, or opening an OSC sequence.
pub fn sanitize_terminal_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn message(chat_id: &str, message_id: &str, seconds: i64, text: &str) -> RawMessage {
        RawMessage {
            account_hash: "acct-synthetic".to_string(),
            chat_id: chat_id.to_string(),
            chat_name: format!("Synthetic {chat_id}"),
            chat_type: "direct".to_string(),
            message_id: message_id.to_string(),
            sender_id: "sender-1".to_string(),
            sender_nickname: "Tester".to_string(),
            timestamp: Utc.timestamp_opt(seconds, 0).single().expect("timestamp"),
            text: text.to_string(),
            message_type: "text".to_string(),
            reply_to_message_id: None,
        }
    }

    fn message_ids(events: &[WatchEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                WatchEvent::Message { message, .. } => Some(message.message_id.clone()),
                WatchEvent::State { .. } => None,
            })
            .collect()
    }

    fn batch_ids(snapshot: &WatchSnapshot, messages: &[RawMessage]) -> Vec<String> {
        snapshot
            .archive_batch(messages)
            .iter()
            .map(|message| message.message_id.clone())
            .collect()
    }

    #[test]
    fn archive_batch_holds_every_message_first_then_only_changes_in_any_chat() {
        let mut snapshot = WatchSnapshot::default();
        let first = vec![message("a", "1", 1, "one"), message("b", "2", 2, "two")];
        snapshot.diff(&first, 1, false, Some("a"));
        assert_eq!(batch_ids(&snapshot, &first), vec!["1", "2"]);

        snapshot.diff(&first, 2, false, Some("a"));
        assert!(batch_ids(&snapshot, &first).is_empty());

        // An edit and an insert outside the filtered chat still reach the archive.
        let next = vec![
            message("a", "1", 1, "one"),
            message("b", "2", 2, "two edited"),
            message("b", "3", 3, "three"),
        ];
        snapshot.diff(&next, 3, false, Some("a"));
        assert!(snapshot.source_changed());
        assert_eq!(batch_ids(&snapshot, &next), vec!["2", "3"]);
    }

    #[test]
    fn an_unchanged_poll_leaves_nothing_to_archive() {
        let mut snapshot = WatchSnapshot::default();
        let messages = vec![message("a", "1", 1, "one")];
        snapshot.diff(&messages, 1, false, None);
        snapshot.unchanged();
        assert!(!snapshot.source_changed());
        assert!(batch_ids(&snapshot, &messages).is_empty());
        assert_eq!(snapshot.seen_count(), 1);
    }

    #[test]
    fn first_snapshot_is_quiet_unless_replay_is_requested() {
        let mut snapshot = WatchSnapshot::default();
        let messages = vec![message("chat-a", "a-1", 1, "one")];

        assert!(snapshot.diff(&messages, 1, false, None).is_empty());

        let mut replay = WatchSnapshot::default();
        let events = replay.diff(&messages, 1, true, None);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            WatchEvent::Message {
                change: WatchMessageChange::Existing,
                ..
            }
        ));
    }

    #[test]
    fn later_snapshots_emit_inserted_and_updated_messages_in_send_order() {
        let mut snapshot = WatchSnapshot::default();
        let first = vec![
            message("chat-a", "a-1", 20, "seed"),
            message("chat-b", "b-1", 10, "seed"),
        ];
        snapshot.diff(&first, 1, false, None);

        let mut edited = first.clone();
        edited[0].text = "edited".to_string();
        edited.push(message("chat-a", "a-2", 30, "new"));

        let events = snapshot.diff(&edited, 2, false, None);
        assert_eq!(message_ids(&events), ["a-1", "a-2"]);
        assert!(matches!(
            events[0],
            WatchEvent::Message {
                change: WatchMessageChange::Updated,
                ..
            }
        ));
        assert!(matches!(
            events[1],
            WatchEvent::Message {
                change: WatchMessageChange::Inserted,
                ..
            }
        ));
    }

    #[test]
    fn chat_filter_limits_emitted_messages_without_forgetting_snapshot_state() {
        let mut snapshot = WatchSnapshot::default();
        let first = vec![
            message("chat-a", "a-1", 1, "seed"),
            message("chat-b", "b-1", 2, "seed"),
        ];
        snapshot.diff(&first, 1, false, Some("chat-a"));

        let mut second = first.clone();
        second.push(message("chat-a", "a-2", 3, "visible"));
        second.push(message("chat-b", "b-2", 4, "hidden"));

        let events = snapshot.diff(&second, 2, false, Some("chat-a"));
        assert_eq!(message_ids(&events), ["a-2"]);
        assert_eq!(snapshot.seen_count(), 4);
        assert_eq!(chat_count(&second), 2);
    }

    #[test]
    fn human_message_lines_are_readable_and_single_line() {
        let mut item = message("chat-a", "a-1", 1, "hello\nthere");
        item.chat_name = "Synthetic\nRoom".to_string();
        item.sender_nickname = "Tester One".to_string();

        assert_eq!(
            format_human_message_line(WatchMessageChange::Existing, &item),
            "[1970-01-01 00:00:01 UTC] Synthetic Room / Tester One: hello there"
        );

        item.text.clear();
        item.message_type = "photo".to_string();
        assert_eq!(
            format_human_message_line(WatchMessageChange::Updated, &item),
            "[1970-01-01 00:00:01 UTC] Synthetic Room / Tester One (updated): <photo>"
        );
    }

    #[test]
    fn human_message_lines_remove_terminal_escape_and_control_characters() {
        let mut item = message(
            "chat-a",
            "a-1",
            1,
            "safe\u{1b}]0;owned\u{7} body\r\nnext\u{009b}2J",
        );
        item.chat_name = "Room\u{1b}[2J".to_string();
        item.sender_nickname = "Tester\tName\u{7}".to_string();

        let rendered = format_human_message_line(WatchMessageChange::Inserted, &item);

        assert_eq!(
            rendered,
            "[1970-01-01 00:00:01 UTC] Room [2J / Tester Name: safe ]0;owned body next 2J"
        );
        assert!(!rendered.chars().any(char::is_control));
    }

    fn chat_at(day: u32, minute: u32, sender: &str, text: &str) -> ChatEntry {
        ChatEntry {
            timestamp: Utc
                .with_ymd_and_hms(2026, 9, day, 12, minute, 0)
                .single()
                .expect("synthetic timestamp"),
            sender: sender.to_string(),
            text: text.to_string(),
        }
    }

    fn texts(frame: &ReplyFrame) -> Vec<&str> {
        frame
            .conversation
            .iter()
            .map(|row| row.text.as_str())
            .collect()
    }

    #[test]
    fn reply_ui_groups_only_adjacent_same_sender_messages_within_five_minutes() {
        let mut ui = ReplyUiState::default();
        ui.push_chat(chat_at(2, 0, "Alice", "one"));
        ui.push_chat(chat_at(2, 4, "Alice", "two"));
        ui.push_chat(chat_at(2, 5, "민준", "three"));
        ui.push_chat(chat_at(2, 11, "민준", "four"));
        ui.push_system("status");
        ui.push_chat(chat_at(2, 12, "민준", "five"));

        let frame = ui.frame(80, 20);
        let rendered = texts(&frame);
        assert!(rendered[1][6..].starts_with("·"));
        assert!(rendered[2][6..].starts_with("민준"));
        assert!(rendered[3][6..].starts_with("민준"));
        assert_eq!(rendered[4], "status");
        assert!(rendered[5][6..].starts_with("민준"));
    }

    #[test]
    fn reply_ui_inserts_local_day_separator_and_breaks_grouping() {
        let mut ui = ReplyUiState::default();
        ui.push_chat(chat_at(2, 59, "Alice", "before"));
        ui.push_chat(chat_at(3, 0, "Alice", "after"));

        let frame = ui.frame(50, 10);
        assert!(frame.conversation[1].text.contains("2026-09-03"));
        assert_eq!(frame.conversation[1].style, RowStyle::Dim);
        assert!(frame.conversation[2].text[6..].starts_with("Alice"));
    }

    #[test]
    fn reply_ui_sender_columns_measure_cjk_and_degrade_at_narrow_widths() {
        let mut wide = ReplyUiState::default();
        wide.push_chat(chat_at(2, 0, "민준", "body"));
        wide.push_chat(chat_at(2, 1, "ABCDEFGHIJKLM", "long"));
        let frame = wide.frame(80, 10);
        let rows = texts(&frame);
        assert_eq!(
            UnicodeWidthStr::width(rows[0].split("body").next().unwrap()),
            19
        );
        assert!(rows[1].contains("ABCDEFGHIJK…"));

        let mut medium = ReplyUiState::default();
        medium.push_chat(chat_at(2, 0, "Alice", "body"));
        let medium_frame = medium.frame(30, 8);
        let medium_row = texts(&medium_frame)[0];
        assert_eq!(
            UnicodeWidthStr::width(medium_row.split("body").next().unwrap()),
            16
        );

        let mut tiny = ReplyUiState::default();
        tiny.push_chat(chat_at(2, 0, "민준", "body"));
        assert!(texts(&tiny.frame(23, 8))[0][6..].starts_with("민준: body"));
    }

    #[test]
    fn wrap_display_line_drops_wide_characters_that_cannot_fit() {
        let rows = wrap_display_line("한a글", 1);

        assert_eq!(rows, ["a"]);
        assert!(rows
            .iter()
            .all(|row| UnicodeWidthStr::width(row.as_str()) <= 1));
        assert!(rows
            .iter()
            .all(|row| row.chars().all(|character| char_width(character) <= 1)));
    }

    #[test]
    fn wrap_display_line_keeps_two_cell_characters_at_width_two() {
        let rows = wrap_display_line("한a글", 2);

        assert_eq!(rows, ["한", "a", "글"]);
        assert!(rows
            .iter()
            .all(|row| UnicodeWidthStr::width(row.as_str()) <= 2));
    }

    #[test]
    fn wrap_display_line_never_exceeds_requested_width() {
        let line = "한a\u{0301}글bc";

        for width in 1..=4 {
            let rows = wrap_display_line(line, width);
            assert!(
                rows.iter()
                    .all(|row| UnicodeWidthStr::width(row.as_str()) <= width),
                "width {width}: {rows:?}"
            );
        }
    }

    #[test]
    fn reply_ui_wrap_cache_reuses_history_and_wraps_only_appends() {
        let mut ui = ReplyUiState::default();
        for index in 0..100 {
            ui.push_system(format!("synthetic line {index}"));
        }
        ui.frame(80, 8);
        let primed = ui.wrap_computations();
        ui.frame(80, 8);
        assert_eq!(ui.wrap_computations(), primed);

        ui.push_system("one appended line");
        ui.frame(80, 8);
        assert_eq!(ui.wrap_computations(), primed + 1);
    }

    #[test]
    fn reply_ui_scroll_tracks_unread_and_clamps_back_to_bottom() {
        let mut ui = ReplyUiState::default();
        for index in 0..10 {
            ui.push_system(format!("line {index}"));
        }
        ui.frame(40, 8);
        ui.scroll(ScrollDelta::Rows(-2), 8);
        ui.push_chat(chat_at(2, 0, "Alice", "new"));
        ui.push_chat(chat_at(2, 1, "민준", "newer"));
        let frame = ui.frame(40, 8);
        assert!(frame
            .conversation
            .last()
            .unwrap()
            .text
            .contains("2 new messages"));
        assert_eq!(ui.new_since_scroll(), 2);

        ui.scroll(ScrollDelta::Rows(isize::MAX), 8);
        assert_eq!(ui.new_since_scroll(), 0);
        assert!(ui.is_pinned_to_bottom());
        assert!(!texts(&ui.frame(40, 8))
            .iter()
            .any(|row| row.contains("new messages")));
    }

    #[test]
    fn reply_ui_edits_unicode_draft_at_the_cursor() {
        let mut ui = ReplyUiState::default();
        ui.paste("한글");
        ui.left();
        ui.insert('어');
        assert_eq!(ui.draft(), "한어글");
        assert_eq!(ui.cursor(), 2);
        ui.home();
        ui.insert('\u{1112}');
        ui.insert('\u{1161}');
        assert!(ui.draft().starts_with("\u{1112}\u{1161}"));

        ui.end();
        ui.paste("a\r\nb");
        ui.left();
        ui.backspace();
        ui.delete();
        ui.ctrl_w();
        ui.ctrl_u();
        ui.ctrl_k();
        assert_eq!(ui.cursor(), 0);
    }

    #[test]
    fn reply_ui_paste_in_middle_never_submits() {
        let mut ui = ReplyUiState::default();
        ui.paste("ac");
        ui.left();
        ui.paste("한\n글");
        assert_eq!(ui.draft(), "a한 글c");
        assert_eq!(ui.cursor(), 4);
    }

    #[test]
    fn reply_ui_horizontal_input_scroll_keeps_wide_chars_whole_and_cursor_exact() {
        let mut ui = ReplyUiState::default();
        ui.paste("abc한글def");
        let end = ui.input_frame(12);
        assert!(end.input.is_char_boundary(end.input.len()));
        assert!(!end.input.starts_with('글'));
        assert!(end.cursor_column <= 10);

        ui.home();
        ui.right();
        ui.right();
        ui.right();
        ui.right();
        let middle = ui.input_frame(20);
        assert_eq!(middle.cursor_column, 12);
        assert_eq!(middle.input, "reply> abc한글def");
    }

    #[test]
    fn reply_ui_sanitizes_every_history_line_before_rendering() {
        let mut ui = ReplyUiState::default();
        ui.push_line("status\u{1b}]8;;https://example.invalid\u{7}link\u{1b}]8;;\u{7}\nnext");

        let frame = ui.frame(120, 5);
        assert!(frame
            .conversation
            .iter()
            .all(|line| !line.text.chars().any(char::is_control)));
    }

    #[test]
    fn reply_line_parser_sends_direct_lines_and_keeps_slash_commands_explicit() {
        assert_eq!(parse_reply_command(0, ""), ReplyCommand::Quit);
        assert_eq!(parse_reply_command(1, "\n"), ReplyCommand::Empty);
        assert_eq!(parse_reply_command(4, "   \n"), ReplyCommand::Empty);
        assert_eq!(parse_reply_command(6, "/quit\n"), ReplyCommand::Quit);
        assert_eq!(parse_reply_command(6, "/help\n"), ReplyCommand::Help);
        assert_eq!(
            parse_reply_command(25, "/send   확인했습니다  \n"),
            ReplyCommand::Send("확인했습니다".to_string())
        );
        assert_eq!(
            parse_reply_command(25, "/send\t확인했습니다\n"),
            ReplyCommand::Send("확인했습니다".to_string())
        );
        assert_eq!(
            parse_reply_command(25, "/send\u{00a0}확인했습니다\n"),
            ReplyCommand::Send("확인했습니다".to_string())
        );
        assert_eq!(
            parse_reply_command(25, "/send확인했습니다\n"),
            ReplyCommand::Ignored
        );
        assert_eq!(
            parse_reply_command(20, "확인했습니다\n"),
            ReplyCommand::Send("확인했습니다".to_string())
        );
        assert_eq!(
            parse_reply_command(20, "/unknown 확인했습니다\n"),
            ReplyCommand::Ignored
        );
    }

    #[test]
    fn reply_ui_keeps_korean_draft_intact_while_history_arrives() {
        let mut ui = ReplyUiState::default();
        for character in "확인 중".chars() {
            ui.insert(character);
        }
        ui.push_line("[now] Synthetic Room / Tester: 새 메시지");

        let frame = ui.frame(80, 8);
        assert_eq!(texts(&frame), ["[now] Synthetic Room / Tester: 새 메시지"]);
        assert_eq!(frame.input, "reply> 확인 중");

        ui.backspace();
        assert_eq!(ui.take_draft(), "확인 ");
    }

    #[test]
    fn reply_ui_resize_wraps_history_without_changing_draft() {
        let mut ui = ReplyUiState::default();
        ui.push_line("abcdefghij");
        ui.paste("한글\n붙여넣기");

        let narrow = ui.frame(7, 5);
        assert_eq!(texts(&narrow), ["abcdef", "ghij"]);
        assert_eq!(narrow.separator, "──────");
        assert_eq!(narrow.input, "reply>");

        let wide = ui.frame(40, 5);
        assert_eq!(texts(&wide), ["abcdefghij"]);
        assert_eq!(wide.input, "reply> 한글 붙여넣기");
        assert_eq!(ui.take_draft(), "한글 붙여넣기");
    }

    #[test]
    fn reply_ui_history_eviction_retains_cached_rows() {
        let mut ui = ReplyUiState::default();
        for index in 0..REPLY_HISTORY_LIMIT {
            ui.push_system(format!("cached {index}"));
        }
        ui.frame(80, 8);
        let primed = ui.wrap_computations();
        ui.push_system("cached appended");
        let frame = ui.frame(80, 4);
        assert_eq!(ui.wrap_computations(), primed + 1);
        assert_eq!(texts(&frame), ["cached appended"]);
    }

    #[test]
    fn reply_ui_paste_never_submits_and_history_is_bounded() {
        let mut ui = ReplyUiState::default();
        ui.paste("first\r\nsecond");
        for index in 0..=REPLY_HISTORY_LIMIT {
            ui.push_line(format!("line {index}"));
        }

        assert_eq!(ui.take_draft(), "first  second");
        let frame = ui.frame(80, 4);
        assert_eq!(texts(&frame), ["line 2000"]);
    }

    #[test]
    fn snapshot_tracks_full_source_changes_even_when_they_are_filtered_from_display() {
        let mut snapshot = WatchSnapshot::default();
        let first = vec![
            message("chat-a", "a-1", 1, "seed"),
            message("chat-b", "b-1", 2, "seed"),
        ];

        snapshot.diff(&first, 1, false, Some("chat-a"));
        assert!(snapshot.source_changed(), "the baseline must be archived");

        snapshot.diff(&first, 2, false, Some("chat-a"));
        assert!(!snapshot.source_changed(), "an identical poll is redundant");

        let mut hidden_change = first.clone();
        hidden_change[1].text = "changed outside display filter".to_string();
        let events = snapshot.diff(&hidden_change, 3, false, Some("chat-a"));
        assert!(events.is_empty(), "the chat filter still controls display");
        assert!(
            snapshot.source_changed(),
            "archive sync must see changes outside the display filter"
        );
    }
}
