use crate::{archive::Archive, Error, Result};
use regex::RegexBuilder;
use rusqlite::types::Value as SqlValue;
use serde::Serialize;

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 10_000;
const DEFAULT_MAX_PER_CHUNK: usize = 20;
const MAX_CONTEXT_LINES: usize = 10;

/// Options controlling exact substring/regex grep over the synced archive.
#[derive(Debug, Clone)]
pub struct GrepOptions {
    /// Treat `pattern` as a literal substring instead of a regular expression.
    pub literal: bool,
    /// Match case-insensitively.
    pub ignore_case: bool,
    /// Number of surrounding lines within a chunk to include before and after each match.
    pub context: usize,
    /// Maximum total hits to return.
    pub limit: usize,
    /// Maximum hits returned from any single chunk.
    pub max_per_chunk: usize,
    /// Restrict search to one `chat_id`.
    pub chat: Option<String>,
    /// Restrict search to chats whose name matches (case-insensitive substring).
    pub chat_name: Option<String>,
    /// Restrict search to chunks whose sender nickname matches (case-insensitive substring).
    pub sender: Option<String>,
    /// Restrict search to chunks with `ended_at >= since`.
    pub since: Option<String>,
    /// Restrict search to chunks with `started_at <= until`.
    pub until: Option<String>,
}

impl Default for GrepOptions {
    fn default() -> Self {
        Self {
            literal: false,
            ignore_case: false,
            context: 0,
            limit: DEFAULT_LIMIT,
            max_per_chunk: DEFAULT_MAX_PER_CHUNK,
            chat: None,
            chat_name: None,
            sender: None,
            since: None,
            until: None,
        }
    }
}

/// One exact line match inside a synced archive chunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GrepHit {
    pub ranker: &'static str,
    pub unit: &'static str,
    pub rank: usize,
    pub chunk_id: String,
    pub chat_id: String,
    pub chat_name: String,
    pub sender_nickname: String,
    pub started_at: String,
    pub ended_at: String,
    pub line_number: usize,
    pub line_end: usize,
    pub matched_line: String,
    pub snippet: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub context_before: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub context_after: Vec<String>,
    pub parent_chunk_ids: Vec<String>,
}

/// Run an exact substring or regex grep over the synced `chunks` table.
///
/// Reads only the local archive (never triggers sync or embeddings). For literal
/// patterns a SQL `LIKE ... ESCAPE '\'` prefilter narrows candidate rows before
/// line-level regex matching verifies exact case/pattern semantics.
pub fn grep_archive(
    archive: &Archive,
    pattern: &str,
    options: &GrepOptions,
) -> Result<Vec<GrepHit>> {
    if pattern.is_empty() {
        return Err(Error::EmptyQuery);
    }

    let limit = options.limit.clamp(1, MAX_LIMIT);
    let max_per_chunk = options.max_per_chunk.max(1);
    let context = options.context.min(MAX_CONTEXT_LINES);

    let regex_source = if options.literal {
        regex::escape(pattern)
    } else {
        pattern.to_string()
    };
    let matcher = RegexBuilder::new(&regex_source)
        .case_insensitive(options.ignore_case)
        .build()
        .map_err(|err| Error::InvalidRegex(err.to_string()))?;

    let mut clauses: Vec<String> = Vec::new();
    let mut bind_values: Vec<SqlValue> = Vec::new();

    if let Some(chat) = options.chat.as_deref().filter(|s| !s.is_empty()) {
        bind_values.push(SqlValue::Text(chat.to_string()));
        clauses.push(format!("chat_id = ?{}", bind_values.len()));
    }
    if let Some(chat_name) = options.chat_name.as_deref().filter(|s| !s.is_empty()) {
        bind_values.push(SqlValue::Text(format!(
            "%{}%",
            escape_sql_like(&chat_name.to_lowercase())
        )));
        clauses.push(format!(
            "LOWER(chat_name) LIKE ?{} ESCAPE '\\'",
            bind_values.len()
        ));
    }
    if let Some(sender) = options.sender.as_deref().filter(|s| !s.is_empty()) {
        bind_values.push(SqlValue::Text(format!(
            "%{}%",
            escape_sql_like(&sender.to_lowercase())
        )));
        clauses.push(format!(
            "LOWER(sender_nickname) LIKE ?{} ESCAPE '\\'",
            bind_values.len()
        ));
    }
    if let Some(since) = options.since.as_deref().filter(|s| !s.is_empty()) {
        bind_values.push(SqlValue::Text(since.to_string()));
        clauses.push(format!("ended_at >= ?{}", bind_values.len()));
    }
    if let Some(until) = options.until.as_deref().filter(|s| !s.is_empty()) {
        bind_values.push(SqlValue::Text(until.to_string()));
        clauses.push(format!("started_at <= ?{}", bind_values.len()));
    }

    // For literal patterns, narrow SQLite scan with a safe ESCAPE'd LIKE prefilter.
    // SQLite's LIKE is case-insensitive for ASCII only, so for case-insensitive
    // non-ASCII literals we skip the SQL prefilter and rely on Rust's Unicode regex.
    if options.literal && (!options.ignore_case || pattern.is_ascii()) {
        bind_values.push(SqlValue::Text(format!("%{}%", escape_sql_like(pattern))));
        clauses.push(format!("text LIKE ?{} ESCAPE '\\'", bind_values.len()));
    }

    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    let sql = format!(
        "SELECT chunk_id, chat_id, chat_name, sender_nickname, started_at, ended_at, text
         FROM chunks{where_sql}
         ORDER BY started_at DESC, chunk_id DESC"
    );

    let mut stmt = archive.connection().prepare(&sql).map_err(Error::Sql)?;
    let mut rows = stmt
        .query(rusqlite::params_from_iter(bind_values.iter()))
        .map_err(Error::Sql)?;

    let mut raw_hits: Vec<GrepHit> = Vec::new();
    while let Some(row) = rows.next().map_err(Error::Sql)? {
        if raw_hits.len() >= limit {
            break;
        }
        let chunk_id: String = row.get(0).map_err(Error::Sql)?;
        let chat_id: String = row.get(1).map_err(Error::Sql)?;
        let chat_name: String = row.get(2).map_err(Error::Sql)?;
        let sender_nickname: String = row.get(3).map_err(Error::Sql)?;
        let started_at: String = row.get(4).map_err(Error::Sql)?;
        let ended_at: String = row.get(5).map_err(Error::Sql)?;
        let text: String = row.get(6).map_err(Error::Sql)?;

        let lines: Vec<&str> = text.split('\n').collect();
        let mut matched_in_chunk = 0usize;

        for (idx, raw_line) in lines.iter().enumerate() {
            let line = raw_line.trim_end_matches('\r');
            if !matcher.is_match(line) {
                continue;
            }
            let start = idx.saturating_sub(context);
            let end = (idx + context).min(lines.len().saturating_sub(1));
            let context_before: Vec<String> = lines[start..idx]
                .iter()
                .map(|l| l.trim_end_matches('\r').to_string())
                .collect();
            let context_after: Vec<String> = if idx < end {
                lines[idx + 1..=end]
                    .iter()
                    .map(|l| l.trim_end_matches('\r').to_string())
                    .collect()
            } else {
                Vec::new()
            };
            let snippet = lines[start..=end]
                .iter()
                .map(|l| l.trim_end_matches('\r'))
                .collect::<Vec<_>>()
                .join("\n");

            raw_hits.push(GrepHit {
                ranker: "grep",
                unit: "micro_chunk",
                rank: raw_hits.len() + 1,
                chunk_id: chunk_id.clone(),
                chat_id: chat_id.clone(),
                chat_name: chat_name.clone(),
                sender_nickname: sender_nickname.clone(),
                started_at: started_at.clone(),
                ended_at: ended_at.clone(),
                line_number: idx + 1,
                line_end: end + 1,
                matched_line: line.to_string(),
                snippet,
                context_before,
                context_after,
                parent_chunk_ids: Vec::new(),
            });

            matched_in_chunk += 1;
            if matched_in_chunk >= max_per_chunk || raw_hits.len() >= limit {
                break;
            }
        }
    }

    drop(rows);
    drop(stmt);

    // Hydrate parent_chunk_ids for matched chunks once per unique chunk_id.
    let mut parent_cache = std::collections::HashMap::<String, Vec<String>>::new();
    for hit in &mut raw_hits {
        if let Some(parents) = parent_cache.get(&hit.chunk_id) {
            hit.parent_chunk_ids = parents.clone();
            continue;
        }
        let parents = archive.parent_chunks(&hit.chunk_id)?;
        hit.parent_chunk_ids = parents.clone();
        parent_cache.insert(hit.chunk_id.clone(), parents);
    }

    Ok(raw_hits)
}

fn escape_sql_like(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::escape_sql_like;

    #[test]
    fn sql_like_escapes_wildcards_and_backslashes() {
        assert_eq!(escape_sql_like(r"100%_a\b"), r"100\%\_a\\b");
    }
}
