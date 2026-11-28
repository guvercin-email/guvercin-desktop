//! Calendar: a full personal calendar with multiple colored calendars, recurring
//! events, reminders, attendees and iCalendar (.ics) import/export.
//!
//! The canonical representation of an event is [`EventCard`], serialized into the
//! `events.event_json` column. A handful of flat columns (title, start/end, all-day
//! and recurrence flags) are kept in sync on every write so range queries and
//! sorting stay cheap and never have to parse JSON per row.
//!
//! Times are stored as **naive wall-clock epoch millis**: the ISO-local string
//! (`2026-07-23T09:00`) is parsed as if it were UTC. The frontend computes its
//! query window the same way, so month/week/day views line up without any
//! timezone drift regardless of the machine's locale.
//!
//! Recurring events keep a single master row (`recurs = 1`) plus an `RRULE`-style
//! [`Recurrence`]; the concrete occurrences are expanded on read, only within the
//! requested window, and returned as individual [`EventInstance`]s.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{header::CONTENT_DISPOSITION, header::CONTENT_TYPE},
    response::Response,
    Json,
};
use chrono::{DateTime, Datelike, Duration, Months, NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{Row, SqlitePool};

use crate::{db, db::AppState, error::AppError};

// ─────────────────────────── Data model ───────────────────────────

fn default_interval() -> i64 {
    1
}
fn default_true() -> bool {
    true
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Attendee {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub status: String, // "", "accepted", "declined", "tentative"
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Recurrence {
    /// "none" | "daily" | "weekly" | "monthly" | "yearly"
    #[serde(default)]
    pub freq: String,
    #[serde(default = "default_interval")]
    pub interval: i64,
    /// Inclusive end date "YYYY-MM-DD", or "" for open-ended.
    #[serde(default)]
    pub until: String,
    /// Max number of occurrences, 0 = unlimited.
    #[serde(default)]
    pub count: i64,
    /// Weekly BYDAY, 0 = Sunday .. 6 = Saturday.
    #[serde(default, rename = "byWeekday")]
    pub by_weekday: Vec<i64>,
}

impl Default for Recurrence {
    fn default() -> Self {
        Recurrence {
            freq: "none".to_string(),
            interval: 1,
            until: String::new(),
            count: 0,
            by_weekday: Vec::new(),
        }
    }
}

impl Recurrence {
    pub(crate) fn is_active(&self) -> bool {
        matches!(
            self.freq.as_str(),
            "daily" | "weekly" | "monthly" | "yearly"
        )
    }
    fn step(&self) -> i64 {
        self.interval.max(1)
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct ZoneObservance {
    onset: String,
    from: i32,
    to: i32,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct EventCard {
    #[serde(default)]
    pub uid: String,
    #[serde(default, rename = "calendarId")]
    pub calendar_id: Option<i64>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "allDay")]
    pub all_day: bool,
    #[serde(default, rename = "timeZone")]
    pub time_zone: String,
    #[serde(default, rename = "endTimeZone")]
    pub end_time_zone: String,
    #[serde(default, rename = "zoneObservances", skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub(crate) zone_observances: std::collections::BTreeMap<String, Vec<ZoneObservance>>,
    #[serde(skip)]
    pub(crate) recurrence_id: Option<i64>,
    #[serde(skip)]
    pub(crate) exclusions: Vec<i64>,
    /// ISO local start: "YYYY-MM-DDTHH:MM" (timed) or "YYYY-MM-DD" (all-day).
    #[serde(default)]
    pub start: String,
    /// ISO local end. For all-day events this is the inclusive last day.
    #[serde(default)]
    pub end: String,
    /// Optional per-event color override; empty falls back to the calendar color.
    #[serde(default)]
    pub color: String,
    /// "confirmed" | "tentative" | "cancelled"
    #[serde(default)]
    pub status: String,
    #[serde(default = "default_true")]
    pub busy: bool,
    #[serde(default)]
    pub attendees: Vec<Attendee>,
    /// Minutes-before reminders.
    #[serde(default)]
    pub reminders: Vec<i64>,
    #[serde(default)]
    pub recurrence: Recurrence,
}

impl Default for EventCard {
    fn default() -> Self {
        EventCard {
            uid: String::new(),
            calendar_id: None,
            title: String::new(),
            location: String::new(),
            description: String::new(),
            all_day: false,
            time_zone: String::new(),
            end_time_zone: String::new(),
            zone_observances: std::collections::BTreeMap::new(),
            recurrence_id: None,
            exclusions: Vec::new(),
            start: String::new(),
            end: String::new(),
            color: String::new(),
            status: "confirmed".to_string(),
            busy: true,
            attendees: Vec::new(),
            reminders: Vec::new(),
            recurrence: Recurrence::default(),
        }
    }
}

/// One concrete occurrence handed to the frontend. `card.start`/`card.end` are the
/// occurrence's own times; `start_ms`/`end_ms` are the naive-UTC millis used for
/// positioning; `id` always refers to the stored master row.
#[derive(Serialize)]
pub struct EventInstance {
    pub id: i64,
    #[serde(rename = "calendarId")]
    pub calendar_id: Option<i64>,
    #[serde(rename = "calendarName")]
    pub calendar_name: String,
    #[serde(rename = "calendarColor")]
    pub calendar_color: String,
    pub card: EventCard,
    #[serde(rename = "startMs")]
    pub start_ms: i64,
    #[serde(rename = "endMs")]
    pub end_ms: i64,
    #[serde(rename = "isRecurring")]
    pub is_recurring: bool,
    /// Stable key for an individual occurrence, e.g. "12@1753254000000".
    #[serde(rename = "instanceKey")]
    pub instance_key: String,
}

#[derive(Serialize)]
pub struct CalendarSummary {
    #[serde(rename = "calendarId")]
    pub calendar_id: i64,
    pub name: String,
    pub color: String,
    #[serde(rename = "isVisible")]
    pub is_visible: bool,
    #[serde(rename = "isDefault")]
    pub is_default: bool,
    #[serde(rename = "googleCalendar")]
    pub google_calendar: bool,
    pub count: i64,
}

// ─────────────────────────── Time helpers ───────────────────────────

const DAY_MS: i64 = 86_400_000;

/// Parse an ISO-local date or datetime as naive-UTC epoch millis.
fn parse_iso_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    for fmt in [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(dt.and_utc().timestamp_millis());
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return d.and_hms_opt(0, 0, 0).map(|dt| dt.and_utc().timestamp_millis());
    }
    None
}

fn dt_from_ms(ms: i64) -> NaiveDateTime {
    DateTime::from_timestamp_millis(ms)
        .map(|d| d.naive_utc())
        .unwrap_or_default()
}

fn iso_datetime(ms: i64) -> String {
    dt_from_ms(ms).format("%Y-%m-%dT%H:%M").to_string()
}

fn iso_date(ms: i64) -> String {
    dt_from_ms(ms).format("%Y-%m-%d").to_string()
}

/// Resolve a card's start/end into a naive-UTC millis range.
/// Timed events use their exact bounds (min 1h). All-day events span whole days:
/// start = start-date 00:00, end = (end-date + 1 day) 00:00, i.e. end-exclusive.
fn compute_bounds(card: &EventCard) -> (i64, i64) {
    let start = parse_iso_ms(&card.start).unwrap_or(0);
    if card.all_day {
        let end_day = parse_iso_ms(&card.end).unwrap_or(start);
        // Snap to midnight and make the end exclusive (last day fully included).
        let start_day = (start / DAY_MS) * DAY_MS;
        let mut end = ((end_day / DAY_MS) * DAY_MS) + DAY_MS;
        if end <= start_day {
            end = start_day + DAY_MS;
        }
        (start_day, end)
    } else {
        let mut end = parse_iso_ms(&card.end).unwrap_or(start);
        if end <= start {
            end = start + 3_600_000; // default 1h
        }
        (start, end)
    }
}

fn recur_until_ms(card: &EventCard) -> Option<i64> {
    if !card.recurrence.is_active() {
        return None;
    }
    let u = card.recurrence.until.trim();
    if u.is_empty() {
        return None;
    }
    // Inclusive UNTIL date → end of that day.
    parse_iso_ms(u).map(|ms| (ms / DAY_MS) * DAY_MS + DAY_MS - 1)
}

/// Advance a naive datetime by `k` periods of the given recurrence frequency.
fn advance(base: NaiveDateTime, freq: &str, step: i64, k: i64) -> NaiveDateTime {
    let n = step * k;
    match freq {
        "daily" => base + Duration::days(n),
        "weekly" => base + Duration::days(n * 7),
        "monthly" => {
            if n >= 0 {
                base.checked_add_months(Months::new(n as u32)).unwrap_or(base)
            } else {
                base.checked_sub_months(Months::new((-n) as u32))
                    .unwrap_or(base)
            }
        }
        "yearly" => {
            if n >= 0 {
                base.checked_add_months(Months::new((n * 12) as u32))
                    .unwrap_or(base)
            } else {
                base.checked_sub_months(Months::new((-n * 12) as u32))
                    .unwrap_or(base)
            }
        }
        _ => base,
    }
}

/// Expand a master event into concrete occurrences overlapping the window
/// `[from, to)` (naive-UTC millis). Non-recurring events yield at most one.
fn expand_occurrences(card: &EventCard, from: i64, to: i64) -> Vec<(i64, i64)> {
    let (start_ms, end_ms) = compute_bounds(card);
    let duration = (end_ms - start_ms).max(0);
    let rec = &card.recurrence;

    if !rec.is_active() {
        if start_ms < to && end_ms > from {
            return vec![(start_ms, end_ms)];
        }
        return Vec::new();
    }

    let until = recur_until_ms(card).unwrap_or(i64::MAX);
    let hard_to = to.min(until.saturating_add(duration));
    let base = dt_from_ms(start_ms);
    let step = rec.step();
    let mut out: Vec<(i64, i64)> = Vec::new();
    let mut produced = 0i64;
    let cap = 2000; // safety bound on iterations

    let weekly_by_days = rec.freq == "weekly" && !rec.by_weekday.is_empty();

    // Fast-forward the period index so the first candidate lands near `from`
    // instead of looping from a possibly-distant master start.
    let period_ms: i64 = match rec.freq.as_str() {
        "daily" => DAY_MS * step,
        "weekly" => DAY_MS * 7 * step,
        "monthly" => 30 * DAY_MS * step,
        "yearly" => 365 * DAY_MS * step,
        _ => DAY_MS,
    };
    let mut k: i64 = if from > start_ms && period_ms > 0 {
        (((from - start_ms) / period_ms) - 2).max(0)
    } else {
        0
    };

    let mut iterations = 0;
    loop {
        iterations += 1;
        if iterations > cap {
            break;
        }
        let occ_base = advance(base, &rec.freq, step, k);
        let occ_base_ms = occ_base.and_utc().timestamp_millis();

        // Candidate occurrence start times for this period.
        let candidates: Vec<i64> = if weekly_by_days {
            // Occurrences fall on selected weekdays within the week of `occ_base`.
            // Align to the Sunday of that week, then offset by each weekday.
            let wd = occ_base.weekday().num_days_from_sunday() as i64;
            let week_sunday_ms = occ_base_ms - wd * DAY_MS;
            let mut days: Vec<i64> = rec
                .by_weekday
                .iter()
                .filter(|d| (0..=6).contains(*d))
                .map(|d| week_sunday_ms + d * DAY_MS)
                .collect();
            days.sort_unstable();
            days
        } else {
            vec![occ_base_ms]
        };

        // Stop once even the earliest candidate is past the window.
        let min_candidate = candidates.iter().copied().min().unwrap_or(occ_base_ms);
        if min_candidate >= hard_to {
            break;
        }

        for cand in candidates {
            if cand < start_ms {
                continue; // never emit before the series start
            }
            if cand > until {
                continue;
            }
            let occ_start = cand;
            let occ_end = occ_start + duration;
            if occ_start < to && occ_end > from {
                out.push((occ_start, occ_end));
            }
            // COUNT limits total occurrences produced across the whole series,
            // not just the windowed ones, so count every candidate at/after start.
            produced += 1;
            if rec.count > 0 && produced >= rec.count {
                return dedup_sorted(out);
            }
        }
        k += 1;
    }
    dedup_sorted(out)
}

fn dedup_sorted(mut v: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    v.sort_unstable();
    v.dedup();
    v
}

// ─────────────────────────── DB helpers ───────────────────────────

const SELECT_COLS: &str =
    "event_id, calendar_id, uid, title, location, all_day, start_ms, end_ms, recurs, recur_until_ms, event_json, created_at, updated_at";

fn row_to_card(row: &sqlx::sqlite::SqliteRow) -> (i64, EventCard) {
    let id: i64 = row.try_get("event_id").unwrap_or_default();
    let card_json: Option<String> = row.try_get("event_json").ok().flatten();
    let mut card = card_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<EventCard>(s).ok())
        .unwrap_or_default();
    // Trust the stored calendar_id column as authoritative.
    if let Ok(cid) = row.try_get::<Option<i64>, _>("calendar_id") {
        card.calendar_id = cid;
    }
    (id, card)
}

async fn ensure_default_calendar(pool: &SqlitePool) -> Result<i64, AppError> {
    if let Some(id) =
        sqlx::query_scalar::<_, i64>("SELECT calendar_id FROM calendars ORDER BY is_default DESC, calendar_id ASC LIMIT 1")
            .fetch_optional(pool)
            .await?
    {
        return Ok(id);
    }
    let res = sqlx::query(
        "INSERT INTO calendars (name, color, is_visible, is_default, sort_order) VALUES (?, ?, 1, 1, 0)",
    )
    .bind("My Calendar")
    .bind("#246bce")
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

async fn calendar_lookup(pool: &SqlitePool) -> std::collections::HashMap<i64, (String, String)> {
    let mut map = std::collections::HashMap::new();
    if let Ok(rows) = sqlx::query("SELECT calendar_id, name, color FROM calendars")
        .fetch_all(pool)
        .await
    {
        for r in rows {
            let id: i64 = r.try_get("calendar_id").unwrap_or_default();
            let name: String = r.try_get("name").unwrap_or_default();
            let color: String = r.try_get("color").unwrap_or_default();
            map.insert(id, (name, color));
        }
    }
    map
}

async fn upsert_event(
    pool: &SqlitePool,
    id: Option<i64>,
    card: &EventCard,
) -> Result<i64, AppError> {
    let (start_ms, end_ms) = compute_bounds(card);
    let recurs = card.recurrence.is_active();
    let until_ms = recur_until_ms(card);
    let event_json = serde_json::to_string(card).unwrap_or_else(|_| "{}".to_string());
    // Local edits mark the row dirty so two-way sync pushes it to Google.
    let now_ms = chrono::Utc::now().timestamp_millis();

    if let Some(id) = id {
        sqlx::query(
            r#"
            UPDATE events SET
                calendar_id = ?, uid = ?, title = ?, location = ?, all_day = ?,
                start_ms = ?, end_ms = ?, recurs = ?, recur_until_ms = ?,
                event_json = ?, dirty = 1, local_updated_ms = ?, updated_at = CURRENT_TIMESTAMP
            WHERE event_id = ?
            "#,
        )
        .bind(card.calendar_id)
        .bind(&card.uid)
        .bind(&card.title)
        .bind(&card.location)
        .bind(card.all_day as i64)
        .bind(start_ms)
        .bind(end_ms)
        .bind(recurs as i64)
        .bind(until_ms)
        .bind(&event_json)
        .bind(now_ms)
        .bind(id)
        .execute(pool)
        .await?;
        Ok(id)
    } else {
        let res = sqlx::query(
            r#"
            INSERT INTO events
                (calendar_id, uid, title, location, all_day, start_ms, end_ms,
                 recurs, recur_until_ms, event_json, dirty, deleted, local_updated_ms,
                 created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, 0, ?, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
            "#,
        )
        .bind(card.calendar_id)
        .bind(&card.uid)
        .bind(&card.title)
        .bind(&card.location)
        .bind(card.all_day as i64)
        .bind(start_ms)
        .bind(end_ms)
        .bind(recurs as i64)
        .bind(until_ms)
        .bind(&event_json)
        .bind(now_ms)
        .execute(pool)
        .await?;
        Ok(res.last_insert_rowid())
    }
}

async fn fetch_card(pool: &SqlitePool, id: i64) -> Result<Option<(i64, EventCard)>, AppError> {
    let row = sqlx::query(&format!(
        "SELECT {SELECT_COLS} FROM events WHERE event_id = ? AND deleted = 0"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| row_to_card(&r)))
}

/// Find or create a calendar by name, keeping its color in sync. Used to mirror
/// remote (Google) calendars into local ones.
pub async fn ensure_named_calendar(
    pool: &SqlitePool,
    name: &str,
    color: &str,
) -> Result<i64, AppError> {
    if let Some(id) = sqlx::query_scalar::<_, i64>("SELECT calendar_id FROM calendars WHERE name = ? LIMIT 1")
        .bind(name)
        .fetch_optional(pool)
        .await?
    {
        let _ = sqlx::query("UPDATE calendars SET color = ? WHERE calendar_id = ?")
            .bind(sanitize_color(color))
            .bind(id)
            .execute(pool)
            .await;
        return Ok(id);
    }
    let res = sqlx::query(
        "INSERT INTO calendars (name, color, is_visible, is_default, sort_order) VALUES (?, ?, 1, 0, (SELECT COALESCE(MAX(sort_order), 0) + 1 FROM calendars))",
    )
    .bind(name)
    .bind(sanitize_color(color))
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

// ─────────────────────────── Event handlers ───────────────────────────

#[derive(Deserialize)]
pub struct RangeQuery {
    #[serde(default)]
    pub from: Option<i64>,
    #[serde(default)]
    pub to: Option<i64>,
    #[serde(default)]
    pub search: Option<String>,
    /// Comma-separated calendar ids to include (visible ones). Empty = all.
    #[serde(default)]
    pub calendars: Option<String>,
}

fn parse_id_csv(s: &Option<String>) -> Vec<i64> {
    s.as_deref()
        .map(|raw| {
            raw.split(',')
                .filter_map(|x| x.trim().parse::<i64>().ok())
                .collect()
        })
        .unwrap_or_default()
}

pub async fn list_events(
    State(state): State<Arc<AppState>>,
    Path(account_id): Path<i64>,
    Query(q): Query<RangeQuery>,
) -> Result<Json<Vec<EventInstance>>, AppError> {
    let pool = db::get_user_db_pool(&state, account_id).await?;
    ensure_default_calendar(&pool).await?;

    // Default window: a broad ± range so a bare request still returns something.
    let now = chrono::Utc::now().timestamp_millis();
    let from = q.from.unwrap_or(now - 365 * DAY_MS);
    let to = q.to.unwrap_or(now + 365 * DAY_MS);
    let cal_ids = parse_id_csv(&q.calendars);
    let search = q.search.as_deref().map(str::trim).unwrap_or("").to_lowercase();

    let mut sql = format!("SELECT {SELECT_COLS} FROM events WHERE deleted = 0 AND ");
    // Overlap for one-off events, or a live recurrence that could reach the window.
    sql.push_str(
        "((recurs = 0 AND start_ms < ? AND end_ms > ?) \
          OR (recurs = 1 AND start_ms < ? AND (recur_until_ms IS NULL OR recur_until_ms >= ?))
          OR (recurs = 1 AND EXISTS (SELECT 1 FROM calendar_occurrence_moves AS moved
              WHERE moved.event_id = events.event_id AND moved.start_ms < ? AND moved.end_ms > ?)))",
    );
    if !cal_ids.is_empty() {
        let placeholders = cal_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        sql.push_str(&format!(" AND calendar_id IN ({placeholders})"));
    }
    if !search.is_empty() {
        sql.push_str(
            " AND (LOWER(COALESCE(title,'')) LIKE ? OR LOWER(COALESCE(location,'')) LIKE ? OR LOWER(COALESCE(event_json,'')) LIKE ?)",
        );
    }
    sql.push_str(" ORDER BY start_ms ASC");

    let mut query = sqlx::query(&sql).bind(to).bind(from).bind(to).bind(from).bind(to).bind(from);
    for id in &cal_ids {
        query = query.bind(id);
    }
    if !search.is_empty() {
        let pat = format!("%{search}%");
        query = query.bind(pat.clone()).bind(pat.clone()).bind(pat);
    }

    let rows = query.fetch_all(&pool).await?;
    let cals = calendar_lookup(&pool).await;

    #[derive(sqlx::FromRow)]
    struct OccurrenceOverride {
        event_id: i64,
        occurrence_start: i64,
        start_ms: i64,
        end_ms: i64,
        override_json: Option<String>,
        is_deleted: bool,
    }
    let moves: Vec<OccurrenceOverride> = sqlx::query_as(
        "SELECT event_id, occurrence_start, start_ms, end_ms, override_json, is_deleted
         FROM calendar_occurrence_moves
         WHERE (start_ms < ? AND end_ms > ?) OR occurrence_start < ?",
    ).bind(to).bind(from).bind(to).fetch_all(&pool).await?;
    let mut moves_by_event = std::collections::HashMap::<i64, Vec<OccurrenceOverride>>::new();
    for entry in moves {
        moves_by_event.entry(entry.event_id).or_default().push(entry);
    }

    let mut out: Vec<EventInstance> = Vec::new();
    for row in &rows {
        let (id, card) = row_to_card(row);
        let (name, color) = card
            .calendar_id
            .and_then(|cid| cals.get(&cid).cloned())
            .unwrap_or_else(|| (String::new(), "#246bce".to_string()));
        let is_recurring = card.recurrence.is_active();
        let moved = moves_by_event.get(&id).map(Vec::as_slice).unwrap_or_default();
        let mut occurrences: Vec<_> = expand_occurrences(&card, from, to).into_iter()
            .filter(|(start, _)| !is_recurring || !moved.iter().any(|entry| entry.occurrence_start == *start))
            .map(|(start, end)| (start, start, end, None)).collect();
        if is_recurring {
            occurrences.extend(moved.iter().filter(|entry| {
                !entry.is_deleted && entry.start_ms < to && entry.end_ms > from
                    && occurrence_exists(&card, entry.occurrence_start)
            }).map(|entry| (entry.occurrence_start, entry.start_ms, entry.end_ms, entry.override_json.as_deref())));
        }
        for (original, occ_start, occ_end, override_json) in occurrences {
            let mut inst_card = override_json.and_then(|value| serde_json::from_str::<EventCard>(value).ok())
                .unwrap_or_else(|| card.clone());
            inst_card.calendar_id = card.calendar_id;
            inst_card.zone_observances.clear();
            if inst_card.all_day {
                inst_card.start = iso_date(occ_start);
                // stored end is inclusive last day → subtract the exclusive day
                inst_card.end = iso_date((occ_end - 1).max(occ_start));
            } else {
                if (is_recurring && override_json.is_none()) || parse_iso_ms(&inst_card.start).is_none() {
                    inst_card.start = iso_datetime(occ_start);
                }
                if (is_recurring && override_json.is_none()) || parse_iso_ms(&inst_card.end).is_none() {
                    inst_card.end = iso_datetime(occ_end);
                }
            }
            out.push(EventInstance {
                id,
                calendar_id: card.calendar_id,
                calendar_name: name.clone(),
                calendar_color: color.clone(),
                card: inst_card,
                start_ms: occ_start,
                end_ms: occ_end,
                is_recurring,
                instance_key: format!("{id}@{original}"),
            });
        }
    }
    out.sort_by_key(|e| e.start_ms);
    Ok(Json(out))
}

pub async fn get_event(
    State(state): State<Arc<AppState>>,
    Path((account_id, event_id)): Path<(i64, i64)>,
) -> Result<Json<EventInstance>, AppError> {
    let pool = db::get_user_db_pool(&state, account_id).await?;
    let (id, card) = fetch_card(&pool, event_id)
        .await?
        .ok_or_else(|| AppError::BadRequest("Event not found".into()))?;
    let cals = calendar_lookup(&pool).await;
    let (name, color) = card
        .calendar_id
        .and_then(|cid| cals.get(&cid).cloned())
        .unwrap_or_else(|| (String::new(), "#246bce".to_string()));
    let (start_ms, end_ms) = compute_bounds(&card);
    let is_recurring = card.recurrence.is_active();
    Ok(Json(EventInstance {
        id,
        calendar_id: card.calendar_id,
        calendar_name: name,
        calendar_color: color,
        card,
        start_ms,
        end_ms,
        is_recurring,
        instance_key: format!("{id}@{start_ms}"),
    }))
}

async fn resolve_calendar_id(pool: &SqlitePool, requested: Option<i64>) -> Result<i64, AppError> {
    if let Some(cid) = requested {
        let exists = sqlx::query_scalar::<_, i64>("SELECT 1 FROM calendars WHERE calendar_id = ?")
            .bind(cid)
            .fetch_optional(pool)
            .await?;
        if exists.is_some() {
            return Ok(cid);
        }
    }
    ensure_default_calendar(pool).await
}

pub async fn create_event(
    State(state): State<Arc<AppState>>,
    Path(account_id): Path<i64>,
    Json(mut card): Json<EventCard>,
) -> Result<Json<EventInstance>, AppError> {
    let pool = db::get_user_db_pool(&state, account_id).await?;
    if card.uid.trim().is_empty() {
        card.uid = new_uid();
    }
    card.calendar_id = Some(resolve_calendar_id(&pool, card.calendar_id).await?);
    let id = upsert_event(&pool, None, &card).await?;
    load_instance(&pool, id).await
}

pub async fn update_event(
    State(state): State<Arc<AppState>>,
    Path((account_id, event_id)): Path<(i64, i64)>,
    Json(mut card): Json<EventCard>,
) -> Result<Json<EventInstance>, AppError> {
    let pool = db::get_user_db_pool(&state, account_id).await?;
    let existing = fetch_card(&pool, event_id)
        .await?
        .ok_or_else(|| AppError::BadRequest("Event not found".into()))?;
    if card.uid.trim().is_empty() {
        card.uid = if existing.1.uid.trim().is_empty() {
            new_uid()
        } else {
            existing.1.uid.clone()
        };
    }
    card.calendar_id = Some(resolve_calendar_id(&pool, card.calendar_id).await?);
    upsert_event(&pool, Some(event_id), &card).await?;
    load_instance(&pool, event_id).await
}

#[derive(Clone, sqlx::FromRow)]
pub(crate) struct OccurrenceChange {
    pub occurrence_start: i64,
    pub start_ms: i64,
    pub end_ms: i64,
    pub override_json: Option<String>,
    pub is_deleted: bool,
    pub revision: i64,
}

impl OccurrenceChange {
    pub(crate) fn from_remote(master: &EventCard, original: i64, card: Option<EventCard>) -> Option<Self> {
        if !master.recurrence.is_active() || !occurrence_exists(master, original) {
            return None;
        }
        let (start_ms, end_ms, override_json, is_deleted) = match card {
            Some(mut card) => {
                card.uid = master.uid.clone();
                card.calendar_id = master.calendar_id;
                card.recurrence = Recurrence::default();
                let (start, end) = compute_bounds(&card);
                (start, end, Some(serde_json::to_string(&card).ok()?), false)
            }
            None => (original, original, None, true),
        };
        Some(Self { occurrence_start: original, start_ms, end_ms, override_json, is_deleted, revision: 0 })
    }

    pub(crate) fn card(&self, master: &EventCard) -> EventCard {
        let mut card: EventCard = self.override_json.as_deref().and_then(|raw| serde_json::from_str(raw).ok())
            .unwrap_or_else(|| master.clone());
        card.uid = master.uid.clone();
        card.recurrence = Recurrence::default();
        if self.override_json.is_none() {
            card.start = if card.all_day { iso_date(self.start_ms) } else { iso_datetime(self.start_ms) };
            card.end = if card.all_day { iso_date(self.end_ms - 1) } else { iso_datetime(self.end_ms) };
        }
        card
    }
}

pub(crate) async fn occurrence_changes(pool: &SqlitePool, event_id: i64, master: &EventCard) -> Result<Vec<OccurrenceChange>, AppError> {
    let mut rows: Vec<OccurrenceChange> = sqlx::query_as(
        "SELECT occurrence_start, start_ms, end_ms, override_json, is_deleted, revision
         FROM calendar_occurrence_moves WHERE event_id = ? ORDER BY occurrence_start",
    ).bind(event_id).fetch_all(pool).await?;
    rows.retain(|change| master.recurrence.is_active() && occurrence_exists(master, change.occurrence_start));
    Ok(rows)
}

pub(crate) async fn receive_occurrences(
    pool: &SqlitePool, event_id: i64, changes: &[OccurrenceChange], replace: bool,
) -> Result<(), AppError> {
    let mut tx = pool.begin().await?;
    if replace {
        sqlx::query("DELETE FROM calendar_occurrence_moves WHERE event_id = ? AND revision = 0")
            .bind(event_id).execute(&mut *tx).await?;
    }
    for change in changes {
        sqlx::query(
            "INSERT INTO calendar_occurrence_moves (event_id, occurrence_start, start_ms, end_ms, override_json, is_deleted, revision)
             VALUES (?, ?, ?, ?, ?, ?, 0) ON CONFLICT(event_id, occurrence_start)
             DO UPDATE SET start_ms = excluded.start_ms, end_ms = excluded.end_ms,
                           override_json = excluded.override_json, is_deleted = excluded.is_deleted
             WHERE calendar_occurrence_moves.revision = 0",
        ).bind(event_id).bind(change.occurrence_start).bind(change.start_ms).bind(change.end_ms)
            .bind(&change.override_json).bind(change.is_deleted).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn mark_occurrence_pushed(pool: &SqlitePool, event_id: i64, change: &OccurrenceChange) -> Result<(), AppError> {
    sqlx::query("UPDATE calendar_occurrence_moves SET revision = 0 WHERE event_id = ? AND occurrence_start = ? AND revision = ?")
        .bind(event_id).bind(change.occurrence_start).bind(change.revision).execute(pool).await?;
    Ok(())
}

pub(crate) fn occurrence_key(value: &str) -> Option<i64> {
    parse_iso_ms(value)
}

fn occurrence_exists(card: &EventCard, original: i64) -> bool {
    expand_occurrences(card, original, original.saturating_add(1))
        .iter().any(|(start, _)| *start == original)
}

#[derive(Deserialize)]
pub struct MoveOccurrence {
    original: i64,
    start: String,
    end: String,
}

pub async fn move_occurrence(
    State(state): State<Arc<AppState>>,
    Path((account_id, event_id)): Path<(i64, i64)>,
    Json(change): Json<MoveOccurrence>,
) -> Result<Json<serde_json::Value>, AppError> {
    let pool = db::get_user_db_pool(&state, account_id).await?;
    let mut card = fetch_card(&pool, event_id).await?
        .map(|(_, card)| card)
        .ok_or_else(|| AppError::BadRequest("Event not found".into()))?;
    if !card.recurrence.is_active() || !occurrence_exists(&card, change.original) {
        return Err(AppError::BadRequest("The event occurrence is no longer available.".into()));
    }
    if parse_iso_ms(&change.start).is_none() || parse_iso_ms(&change.end).is_none() {
        return Err(AppError::BadRequest("Invalid occurrence dates.".into()));
    }
    card.start = change.start;
    card.end = change.end;
    let (start, end) = compute_bounds(&card);
    sqlx::query(
        "INSERT INTO calendar_occurrence_moves (event_id, occurrence_start, start_ms, end_ms)
         VALUES (?, ?, ?, ?) ON CONFLICT(event_id, occurrence_start)
         DO UPDATE SET start_ms = excluded.start_ms, end_ms = excluded.end_ms,
                       revision = calendar_occurrence_moves.revision + 1
         WHERE calendar_occurrence_moves.is_deleted = 0",
    ).bind(event_id).bind(change.original).bind(start).bind(end).execute(&pool).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct ChangeOccurrence {
    original: i64,
    card: Option<EventCard>,
}

pub async fn change_occurrence(
    State(state): State<Arc<AppState>>,
    Path((account_id, event_id)): Path<(i64, i64)>,
    Json(change): Json<ChangeOccurrence>,
) -> Result<Json<serde_json::Value>, AppError> {
    let pool = db::get_user_db_pool(&state, account_id).await?;
    let master = fetch_card(&pool, event_id).await?.map(|(_, card)| card)
        .ok_or_else(|| AppError::BadRequest("Event not found".into()))?;
    if !master.recurrence.is_active() || !occurrence_exists(&master, change.original) {
        return Err(AppError::BadRequest("The event occurrence is no longer available.".into()));
    }
    let (start, end, json, deleted) = match change.card {
        Some(mut card) => {
            if parse_iso_ms(&card.start).is_none() || parse_iso_ms(&card.end).is_none() {
                return Err(AppError::BadRequest("Invalid occurrence dates.".into()));
            }
            card.calendar_id = master.calendar_id;
            card.uid = master.uid;
            card.recurrence = Recurrence::default();
            let (start, end) = compute_bounds(&card);
            let json = serde_json::to_string(&card).map_err(|error| AppError::BadRequest(error.to_string()))?;
            (start, end, Some(json), false)
        }
        None => (change.original, change.original, None, true),
    };
    sqlx::query(
        "INSERT INTO calendar_occurrence_moves (event_id, occurrence_start, start_ms, end_ms, override_json, is_deleted)
         VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(event_id, occurrence_start)
         DO UPDATE SET start_ms = excluded.start_ms, end_ms = excluded.end_ms,
                       override_json = excluded.override_json, is_deleted = excluded.is_deleted,
                       revision = calendar_occurrence_moves.revision + 1",
    ).bind(event_id).bind(change.original).bind(start).bind(end).bind(json).bind(deleted)
      .execute(&pool).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn load_instance(pool: &SqlitePool, id: i64) -> Result<Json<EventInstance>, AppError> {
    let (id, card) = fetch_card(pool, id)
        .await?
        .ok_or_else(|| AppError::BadRequest("Failed to load event".into()))?;
    let cals = calendar_lookup(pool).await;
    let (name, color) = card
        .calendar_id
        .and_then(|cid| cals.get(&cid).cloned())
        .unwrap_or_else(|| (String::new(), "#246bce".to_string()));
    let (start_ms, end_ms) = compute_bounds(&card);
    let is_recurring = card.recurrence.is_active();
    Ok(Json(EventInstance {
        id,
        calendar_id: card.calendar_id,
        calendar_name: name,
        calendar_color: color,
        card,
        start_ms,
        end_ms,
        is_recurring,
        instance_key: format!("{id}@{start_ms}"),
    }))
}

pub async fn delete_event(
    State(state): State<Arc<AppState>>,
    Path((account_id, event_id)): Path<(i64, i64)>,
) -> Result<Json<serde_json::Value>, AppError> {
    let pool = db::get_user_db_pool(&state, account_id).await?;
    soft_or_hard_delete(&pool, "events", "event_id", event_id).await?;
    Ok(Json(json!({ "status": "success" })))
}

/// Tombstone a row if it is linked to a remote (Google) item so the deletion can
/// be pushed; otherwise remove it outright. Shared shape across the syncable
/// stores.
pub async fn soft_or_hard_delete(
    pool: &SqlitePool,
    table: &str,
    id_col: &str,
    id: i64,
) -> Result<(), AppError> {
    let remote_id: Option<String> =
        sqlx::query_scalar(&format!("SELECT remote_id FROM {table} WHERE {id_col} = ?"))
            .bind(id)
            .fetch_optional(pool)
            .await?
            .flatten();
    if remote_id.as_deref().map(|s| !s.is_empty()).unwrap_or(false) {
        let now_ms = chrono::Utc::now().timestamp_millis();
        sqlx::query(&format!(
            "UPDATE {table} SET deleted = 1, dirty = 1, local_updated_ms = ? WHERE {id_col} = ?"
        ))
        .bind(now_ms)
        .bind(id)
        .execute(pool)
        .await?;
    } else {
        sqlx::query(&format!("DELETE FROM {table} WHERE {id_col} = ?"))
            .bind(id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

/// Hard-remove a row (used by sync for confirmed remote deletions / drops).
pub async fn sync_hard_delete(
    pool: &SqlitePool,
    table: &str,
    id_col: &str,
    id: i64,
) -> Result<(), AppError> {
    sqlx::query(&format!("DELETE FROM {table} WHERE {id_col} = ?"))
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// After pushing a local row to Google, record its remote identity and clear the
/// dirty flag so it isn't pushed again.
pub async fn sync_mark_pushed(
    pool: &SqlitePool,
    table: &str,
    id_col: &str,
    id: i64,
    remote_id: &str,
    etag: Option<&str>,
    remote_ms: i64,
) -> Result<(), AppError> {
    sqlx::query(&format!(
        "UPDATE {table} SET remote_id = ?, etag = ?, remote_updated_ms = ?, local_updated_ms = ?, dirty = 0 WHERE {id_col} = ?"
    ))
    .bind(remote_id)
    .bind(etag)
    .bind(remote_ms)
    .bind(remote_ms)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert or overwrite an event from an authoritative remote (Google) copy. The
/// row is written clean (dirty = 0, deleted = 0) with its remote identity.
pub async fn sync_write_event(
    pool: &SqlitePool,
    id: Option<i64>,
    card: &EventCard,
    remote_id: &str,
    etag: Option<&str>,
    remote_ms: i64,
) -> Result<i64, AppError> {
    let (start_ms, end_ms) = compute_bounds(card);
    let recurs = card.recurrence.is_active();
    let until_ms = recur_until_ms(card);
    let event_json = serde_json::to_string(card).unwrap_or_else(|_| "{}".to_string());
    if let Some(id) = id {
        sqlx::query(
            r#"
            UPDATE events SET
                calendar_id = ?, uid = ?, title = ?, location = ?, all_day = ?,
                start_ms = ?, end_ms = ?, recurs = ?, recur_until_ms = ?, event_json = ?,
                remote_id = ?, etag = ?, remote_updated_ms = ?, local_updated_ms = ?,
                dirty = 0, deleted = 0, updated_at = CURRENT_TIMESTAMP
            WHERE event_id = ?
            "#,
        )
        .bind(card.calendar_id).bind(&card.uid).bind(&card.title).bind(&card.location)
        .bind(card.all_day as i64).bind(start_ms).bind(end_ms).bind(recurs as i64).bind(until_ms)
        .bind(&event_json).bind(remote_id).bind(etag).bind(remote_ms).bind(remote_ms).bind(id)
        .execute(pool).await?;
        Ok(id)
    } else {
        let res = sqlx::query(
            r#"
            INSERT INTO events
                (calendar_id, uid, title, location, all_day, start_ms, end_ms, recurs,
                 recur_until_ms, event_json, remote_id, etag, remote_updated_ms, local_updated_ms,
                 dirty, deleted, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, 0, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
            "#,
        )
        .bind(card.calendar_id).bind(&card.uid).bind(&card.title).bind(&card.location)
        .bind(card.all_day as i64).bind(start_ms).bind(end_ms).bind(recurs as i64).bind(until_ms)
        .bind(&event_json).bind(remote_id).bind(etag).bind(remote_ms).bind(remote_ms)
        .execute(pool).await?;
        Ok(res.last_insert_rowid())
    }
}

// ─────────────────────────── Backend preference ───────────────────────────
//
// Which sync backend the account uses for the calendar: "", "google", "caldav" or
// "local". See [`crate::db::get_backend_pref`] for how (and where) the choice is
// stored; contacts and tasks have the same pair of routes.

pub async fn get_calendar_backend(
    State(state): State<Arc<AppState>>,
    Path(account_id): Path<i64>,
) -> Result<Json<serde_json::Value>, AppError> {
    let backend = db::get_backend_pref(&state, account_id, "calendar_backend", "caldav").await?;
    Ok(Json(json!({ "backend": backend })))
}

#[derive(Deserialize)]
pub struct BackendBody {
    #[serde(default)]
    pub backend: String,
}

pub async fn set_calendar_backend(
    State(state): State<Arc<AppState>>,
    Path(account_id): Path<i64>,
    Json(body): Json<BackendBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let backend =
        db::set_backend_pref(&state, account_id, "calendar_backend", "caldav", &body.backend)
            .await?;
    Ok(Json(json!({ "backend": backend })))
}

// ─────────────────────────── Calendar handlers ───────────────────────────

pub async fn get_calendars(
    State(state): State<Arc<AppState>>,
    Path(account_id): Path<i64>,
) -> Result<Json<serde_json::Value>, AppError> {
    let pool = db::get_user_db_pool(&state, account_id).await?;
    ensure_default_calendar(&pool).await?;

    let rows = sqlx::query(
        r#"
        SELECT c.calendar_id, c.name, c.color, c.is_visible, c.is_default, c.remote_id,
               (SELECT COUNT(*) FROM events e WHERE e.calendar_id = c.calendar_id) AS cnt
        FROM calendars c
        ORDER BY c.is_default DESC, c.sort_order ASC, LOWER(c.name) ASC
        "#,
    )
    .fetch_all(&pool)
    .await?;

    let mut calendars: Vec<CalendarSummary> = Vec::new();
    let mut total: i64 = 0;
    for r in &rows {
        let cnt: i64 = r.try_get("cnt").unwrap_or_default();
        total += cnt;
        calendars.push(CalendarSummary {
            calendar_id: r.try_get("calendar_id").unwrap_or_default(),
            name: r.try_get("name").unwrap_or_default(),
            color: r.try_get("color").unwrap_or_default(),
            is_visible: r.try_get::<i64, _>("is_visible").unwrap_or(1) != 0,
            is_default: r.try_get::<i64, _>("is_default").unwrap_or(0) != 0,
            google_calendar: r.try_get::<Option<String>, _>("remote_id").ok().flatten()
                .is_some_and(|id| !id.is_empty() && !id.contains("://")),
            count: cnt,
        });
    }

    Ok(Json(json!({ "calendars": calendars, "total": total })))
}

#[derive(Deserialize)]
pub struct CalendarBody {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default, rename = "isVisible")]
    pub is_visible: Option<bool>,
}

pub async fn create_calendar(
    State(state): State<Arc<AppState>>,
    Path(account_id): Path<i64>,
    Json(body): Json<CalendarBody>,
) -> Result<Json<CalendarSummary>, AppError> {
    let pool = db::get_user_db_pool(&state, account_id).await?;
    let name = body.name.as_deref().map(str::trim).unwrap_or("").to_string();
    if name.is_empty() {
        return Err(AppError::BadRequest("Calendar name is required".into()));
    }
    let color = sanitize_color(body.color.as_deref().unwrap_or("#8e44ad"));
    let res = sqlx::query(
        "INSERT INTO calendars (name, color, is_visible, is_default, sort_order) VALUES (?, ?, 1, 0, (SELECT COALESCE(MAX(sort_order), 0) + 1 FROM calendars))",
    )
    .bind(&name)
    .bind(&color)
    .execute(&pool)
    .await?;
    Ok(Json(CalendarSummary {
        calendar_id: res.last_insert_rowid(),
        name,
        color,
        is_visible: true,
        is_default: false,
        google_calendar: false,
        count: 0,
    }))
}

pub async fn update_calendar(
    State(state): State<Arc<AppState>>,
    Path((account_id, calendar_id)): Path<(i64, i64)>,
    Json(body): Json<CalendarBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let pool = db::get_user_db_pool(&state, account_id).await?;
    if let Some(name) = body.name.as_deref().map(str::trim) {
        if !name.is_empty() {
            sqlx::query("UPDATE calendars SET name = ? WHERE calendar_id = ?")
                .bind(name)
                .bind(calendar_id)
                .execute(&pool)
                .await?;
        }
    }
    if let Some(color) = body.color.as_deref() {
        sqlx::query("UPDATE calendars SET color = ? WHERE calendar_id = ?")
            .bind(sanitize_color(color))
            .bind(calendar_id)
            .execute(&pool)
            .await?;
    }
    if let Some(vis) = body.is_visible {
        sqlx::query("UPDATE calendars SET is_visible = ? WHERE calendar_id = ?")
            .bind(vis as i64)
            .bind(calendar_id)
            .execute(&pool)
            .await?;
    }
    Ok(Json(json!({ "status": "success" })))
}

pub async fn delete_calendar(
    State(state): State<Arc<AppState>>,
    Path((account_id, calendar_id)): Path<(i64, i64)>,
) -> Result<Json<serde_json::Value>, AppError> {
    let pool = db::get_user_db_pool(&state, account_id).await?;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM calendars")
        .fetch_one(&pool)
        .await
        .unwrap_or(0);
    if count <= 1 {
        return Err(AppError::BadRequest(
            "You can't delete your only calendar.".into(),
        ));
    }
    // Events cascade-delete via the foreign key (PRAGMA foreign_keys = ON).
    sqlx::query("DELETE FROM events WHERE calendar_id = ?")
        .bind(calendar_id)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM calendars WHERE calendar_id = ?")
        .bind(calendar_id)
        .execute(&pool)
        .await?;
    // If we removed the default, promote another calendar.
    let has_default: Option<i64> =
        sqlx::query_scalar("SELECT calendar_id FROM calendars WHERE is_default = 1 LIMIT 1")
            .fetch_optional(&pool)
            .await?;
    if has_default.is_none() {
        let _ = sqlx::query(
            "UPDATE calendars SET is_default = 1 WHERE calendar_id = (SELECT calendar_id FROM calendars ORDER BY calendar_id ASC LIMIT 1)",
        )
        .execute(&pool)
        .await;
    }
    Ok(Json(json!({ "status": "success" })))
}

fn sanitize_color(input: &str) -> String {
    let s = input.trim();
    if s.starts_with('#') && (s.len() == 7 || s.len() == 4) && s[1..].chars().all(|c| c.is_ascii_hexdigit()) {
        s.to_string()
    } else {
        "#246bce".to_string()
    }
}

// ─────────────────────────── ICS import / export ───────────────────────────

#[derive(Deserialize)]
pub struct ImportBody {
    #[serde(default)]
    pub ics: String,
    #[serde(default, rename = "calendarId")]
    pub calendar_id: Option<i64>,
}

pub async fn import_events(
    State(state): State<Arc<AppState>>,
    Path(account_id): Path<i64>,
    Json(body): Json<ImportBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let pool = db::get_user_db_pool(&state, account_id).await?;
    let target = resolve_calendar_id(&pool, body.calendar_id).await?;
    let cards = parse_ics(&body.ics);
    if cards.is_empty() {
        return Err(AppError::BadRequest(
            "No calendar events found in this file. Choose an .ics file exported from another calendar app.".into(),
        ));
    }
    let mut imported = 0i64;
    let mut skipped = 0i64;
    for mut card in cards.iter().filter(|card| card.recurrence_id.is_none()).cloned() {
        if card.start.trim().is_empty() {
            skipped += 1;
            continue;
        }
        if card.uid.trim().is_empty() {
            card.uid = new_uid();
        }
        card.calendar_id = Some(target);
        let id = upsert_event(&pool, None, &card).await?;
        receive_occurrences(&pool, id, &ics_occurrences(&card, &cards), false).await?;
        if card.recurrence.is_active() {
            sqlx::query("UPDATE calendar_occurrence_moves SET revision = 1 WHERE event_id = ?")
                .bind(id).execute(&pool).await?;
        }
        imported += 1;
    }
    Ok(Json(json!({ "imported": imported, "skipped": skipped })))
}

#[derive(Deserialize)]
pub struct ExportQuery {
    #[serde(default)]
    pub ids: Option<String>,
    #[serde(default)]
    pub calendars: Option<String>,
}

pub async fn export_events(
    State(state): State<Arc<AppState>>,
    Path(account_id): Path<i64>,
    Query(q): Query<ExportQuery>,
) -> Result<Response, AppError> {
    let pool = db::get_user_db_pool(&state, account_id).await?;

    let ids = parse_id_csv(&q.ids);
    let cal_ids = parse_id_csv(&q.calendars);
    let mut sql = format!("SELECT {SELECT_COLS} FROM events");
    let mut clauses: Vec<String> = vec!["deleted = 0".to_string()];
    if !ids.is_empty() {
        clauses.push(format!(
            "event_id IN ({})",
            ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",")
        ));
    }
    if !cal_ids.is_empty() {
        clauses.push(format!(
            "calendar_id IN ({})",
            cal_ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",")
        ));
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY start_ms ASC");

    let rows = sqlx::query(&sql).fetch_all(&pool).await?;
    let mut ics = ics_header();
    let mut zones = std::collections::HashSet::new();
    for row in &rows {
        let (id, card) = row_to_card(row);
        let changes = occurrence_changes(&pool, id, &card).await?;
        push_series_zones(&mut ics, &card, &changes, &mut zones);
        push_series(&mut ics, &card, &changes);
    }
    ics.push_str("END:VCALENDAR\r\n");

    let mut resp = Response::new(ics.into());
    resp.headers_mut()
        .insert(CONTENT_TYPE, "text/calendar; charset=utf-8".parse().unwrap());
    resp.headers_mut().insert(
        CONTENT_DISPOSITION,
        "attachment; filename=\"guvercin.ics\"".parse().unwrap(),
    );
    Ok(resp)
}

// ── ICS writing ──

fn ics_header() -> String {
    "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//guvercin//Calendar//EN\r\nCALSCALE:GREGORIAN\r\n".into()
}

pub(crate) fn build_ics_series(card: &EventCard, changes: &[OccurrenceChange]) -> String {
    let mut out = ics_header();
    push_series_zones(&mut out, card, changes, &mut std::collections::HashSet::new());
    push_series(&mut out, card, changes);
    out.push_str("END:VCALENDAR\r\n");
    out
}

/// Apply instance changes to the server's original resource so organizer
/// data, custom properties, timezone definitions, and other exceptions survive.
pub(crate) fn patch_ics_occurrences(
    source: &str, master: &EventCard, changes: &[OccurrenceChange],
) -> Option<String> {
    let edits: std::collections::BTreeMap<_, _> = changes.iter()
        .filter(|change| change.revision != 0).map(|change| (change.occurrence_start, change)).collect();
    let lines = ics_unfold(source);
    let mut zones: std::collections::HashSet<_> = lines.iter().filter_map(|line| {
        let prop = parse_prop(line)?;
        (prop.name == "TZID").then_some(prop.value)
    }).collect();
    let original_property = |property: &str, original: i64| {
        if master.all_day {
            format!("{property};VALUE=DATE:{}", ics_date(original))
        } else {
            time_property(property, original, &master.time_zone)
        }
    };
    let key = |value: &str| parse_ics_datetime(value, master.all_day).and_then(|(iso, _)| parse_iso_ms(&iso));
    let mut output = String::new();
    let mut event = Vec::new();
    let mut keep = Vec::new();
    let (mut in_event, mut master_found, mut wrote_calendar_end) = (false, false, false);
    for line in &lines {
        if line.eq_ignore_ascii_case("BEGIN:VEVENT") {
            in_event = true;
            event.clear();
        }
        if in_event {
            event.push(line.as_str());
            if !line.eq_ignore_ascii_case("END:VEVENT") { continue; }
            in_event = false;
            let properties: Vec<_> = event.iter().filter_map(|line| parse_prop(line)).collect();
            let matches = properties.iter().any(|prop| prop.name == "UID" && unescape_text(&prop.value) == master.uid);
            let recurrence_id = properties.iter().find(|prop| prop.name == "RECURRENCE-ID");
            if matches && recurrence_id.is_some_and(|prop| key(&prop.value).is_some_and(|id| edits.contains_key(&id))) {
                continue;
            }
            if matches && recurrence_id.is_none() {
                master_found = true;
                for line in &event {
                    match parse_prop(line).as_ref().map(|prop| prop.name.as_str()) {
                        Some("EXDATE") => {
                            let prop = parse_prop(line)?;
                            let values = prop.value.split(',').filter(|value| !key(value).is_some_and(|id| edits.contains_key(&id)))
                                .collect::<Vec<_>>();
                            if !values.is_empty() {
                                let head = line.split_once(':')?.0;
                                fold_line(&mut output, &format!("{head}:{}", values.join(",")));
                            }
                        }
                        Some("ORGANIZER" | "SEQUENCE") => {
                            keep.push((*line).to_owned());
                            fold_line(&mut output, line);
                        }
                        _ => {
                            if line.eq_ignore_ascii_case("END:VEVENT") {
                                for change in edits.values().filter(|change| change.is_deleted) {
                                    fold_line(&mut output, &original_property("EXDATE", change.occurrence_start));
                                }
                            }
                            fold_line(&mut output, line);
                        }
                    }
                }
            } else {
                for line in &event { fold_line(&mut output, line); }
            }
            continue;
        }
        if line.eq_ignore_ascii_case("END:VCALENDAR") {
            for change in edits.values().filter(|change| !change.is_deleted) {
                let card = change.card(master);
                push_zones(&mut output, &card, &mut zones);
                let mut extra = vec![original_property("RECURRENCE-ID", change.occurrence_start)];
                extra.extend(keep.iter().cloned());
                push_vevent(&mut output, &card, &extra);
            }
            wrote_calendar_end = true;
        }
        fold_line(&mut output, line);
    }
    (master_found && wrote_calendar_end).then_some(output)
}

fn push_series_zones(
    out: &mut String, master: &EventCard, changes: &[OccurrenceChange],
    written: &mut std::collections::HashSet<String>,
) {
    push_zones(out, master, written);
    for change in changes.iter().filter(|change| !change.is_deleted && change.override_json.is_some()) {
        push_zones(out, &change.card(master), written);
    }
}

fn push_zones(out: &mut String, card: &EventCard, written: &mut std::collections::HashSet<String>) {
    if card.all_day { return; }
    for zone in [&card.time_zone, &card.end_time_zone] {
        if zone == "UTC" || !valid_zone_id(zone) || written.contains(zone) { continue; }
        let Some(observances) = card.zone_observances.get(zone) else { continue };
        let mut lines = Vec::new();
        for rule in observances.iter().take(128) {
            let Some(onset) = parse_iso_ms(&rule.onset) else { continue };
            let (Some(from), Some(to)) = (ical_offset(rule.from), ical_offset(rule.to)) else { continue };
            let kind = if rule.to > rule.from { "DAYLIGHT" } else { "STANDARD" };
            lines.extend([format!("BEGIN:{kind}"), format!("DTSTART:{}", ics_datetime(onset)),
                format!("TZOFFSETFROM:{from}"), format!("TZOFFSETTO:{to}"), format!("END:{kind}")]);
        }
        if lines.is_empty() { continue; }
        written.insert(zone.clone());
        fold_line(out, "BEGIN:VTIMEZONE");
        fold_line(out, &format!("TZID:{zone}"));
        for line in &lines { fold_line(out, line); }
        fold_line(out, "END:VTIMEZONE");
    }
}

fn ical_offset(minutes: i32) -> Option<String> {
    if !(-1439..=1439).contains(&minutes) { return None; }
    let sign = if minutes < 0 { '-' } else { '+' };
    let absolute = minutes.abs();
    Some(format!("{sign}{:02}{:02}", absolute / 60, absolute % 60))
}

fn push_series(out: &mut String, master: &EventCard, changes: &[OccurrenceChange]) {
    let original_property = |property: &str, original: i64| {
        if master.all_day {
            format!("{property};VALUE=DATE:{}", ics_date(original))
        } else {
            time_property(property, original, &master.time_zone)
        }
    };
    let mut master_extra = Vec::new();
    for change in changes.iter().filter(|change| change.is_deleted) {
        master_extra.push(original_property("EXDATE", change.occurrence_start));
    }
    push_vevent(out, master, &master_extra);
    for change in changes.iter().filter(|change| !change.is_deleted) {
        push_vevent(out, &change.card(master), &[original_property("RECURRENCE-ID", change.occurrence_start)]);
    }
}

pub(crate) fn ics_occurrences(master: &EventCard, cards: &[EventCard]) -> Vec<OccurrenceChange> {
    let mut changes = std::collections::BTreeMap::new();
    for original in &master.exclusions {
        if let Some(change) = OccurrenceChange::from_remote(master, *original, None) {
            changes.insert(*original, change);
        }
    }
    for card in cards.iter().filter(|card| card.uid == master.uid) {
        if let Some(original) = card.recurrence_id {
            let override_card = (card.status != "cancelled" && !card.start.is_empty()).then(|| card.clone());
            if let Some(change) = OccurrenceChange::from_remote(master, original, override_card) {
                changes.insert(original, change);
            }
        }
    }
    changes.into_values().collect()
}

#[cfg(test)]
fn build_ics(cards: &[EventCard]) -> String {
    let mut out = ics_header();
    for card in cards {
        push_vevent(&mut out, card, &[]);
    }
    out.push_str("END:VCALENDAR\r\n");
    out
}

fn valid_zone_id(zone: &str) -> bool {
    !zone.is_empty() && zone.len() <= 128 && zone.bytes().all(|byte|
        byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'-' | b'+' | b'.'))
}

fn time_property(property: &str, ms: i64, zone: &str) -> String {
    let date = ics_datetime(ms);
    if zone == "UTC" {
        format!("{property}:{date}Z")
    } else if valid_zone_id(zone) {
        format!("{property};TZID={zone}:{date}")
    } else {
        format!("{property}:{date}")
    }
}

fn prop_time_zone(prop: &IcsProp) -> String {
    if prop.value.ends_with('Z') {
        return "UTC".into();
    }
    prop.params.iter().find(|(name, _)| name == "TZID").map(|(_, zone)| zone.trim_matches('"'))
        .filter(|zone| valid_zone_id(zone)).unwrap_or_default().to_owned()
}

fn push_vevent(out: &mut String, card: &EventCard, extra: &[String]) {
    out.push_str("BEGIN:VEVENT\r\n");
    let uid = if card.uid.trim().is_empty() {
        new_uid()
    } else {
        card.uid.clone()
    };
    fold_line(out, &format!("UID:{}", escape_text(&uid)));
    for property in extra {
        fold_line(out, property);
    }
    fold_line(out, &format!("DTSTAMP:{}", ics_utc_now()));

    let (start_ms, end_ms) = compute_bounds(card);
    if card.all_day {
        fold_line(out, &format!("DTSTART;VALUE=DATE:{}", ics_date(start_ms)));
        // ICS all-day DTEND is exclusive — our end_ms is already exclusive.
        fold_line(out, &format!("DTEND;VALUE=DATE:{}", ics_date(end_ms)));
    } else {
        let end_zone = if card.end_time_zone.is_empty() { &card.time_zone } else { &card.end_time_zone };
        fold_line(out, &time_property("DTSTART", start_ms, &card.time_zone));
        let end = if end_zone != &card.time_zone { parse_iso_ms(&card.end).unwrap_or(end_ms) } else { end_ms };
        fold_line(out, &time_property("DTEND", end, end_zone));
    }
    if !card.title.trim().is_empty() {
        fold_line(out, &format!("SUMMARY:{}", escape_text(&card.title)));
    }
    if !card.location.trim().is_empty() {
        fold_line(out, &format!("LOCATION:{}", escape_text(&card.location)));
    }
    if !card.description.trim().is_empty() {
        fold_line(out, &format!("DESCRIPTION:{}", escape_text(&card.description)));
    }
    match card.status.as_str() {
        "tentative" => fold_line(out, "STATUS:TENTATIVE"),
        "cancelled" => fold_line(out, "STATUS:CANCELLED"),
        _ => {}
    }
    for a in &card.attendees {
        if a.email.trim().is_empty() {
            continue;
        }
        let cn = if a.name.trim().is_empty() {
            String::new()
        } else {
            format!(";CN={}", escape_text(&a.name))
        };
        fold_line(out, &format!("ATTENDEE{cn}:mailto:{}", a.email.trim()));
    }
    if let Some(rrule) = build_rrule(card) {
        fold_line(out, &format!("RRULE:{rrule}"));
    }
    out.push_str("END:VEVENT\r\n");
}

pub fn build_rrule(card: &EventCard) -> Option<String> {
    let r = &card.recurrence;
    if !r.is_active() {
        return None;
    }
    let mut parts = vec![format!("FREQ={}", r.freq.to_uppercase())];
    if r.interval > 1 {
        parts.push(format!("INTERVAL={}", r.interval));
    }
    if r.count > 0 {
        parts.push(format!("COUNT={}", r.count));
    }
    if !r.until.trim().is_empty() {
        if let Some(ms) = parse_iso_ms(r.until.trim()) {
            let end = ms.saturating_add(DAY_MS - 1000);
            let until = if card.all_day {
                ics_date(ms)
            } else if card.time_zone == "UTC" {
                format!("{}Z", ics_datetime(end))
            } else if let Some(offset) = card.zone_observances.get(&card.time_zone).and_then(|rules|
                rules.iter().filter_map(|rule| parse_iso_ms(&rule.onset).map(|onset| (onset, rule.to)))
                    .filter(|(onset, _)| *onset <= end).max_by_key(|(onset, _)| *onset).map(|(_, offset)| offset))
            {
                format!("{}Z", ics_datetime(end.saturating_sub(i64::from(offset) * 60000)))
            } else {
                ics_datetime(end)
            };
            parts.push(format!("UNTIL={until}"));
        }
    }
    if r.freq == "weekly" && !r.by_weekday.is_empty() {
        let days: Vec<&str> = r
            .by_weekday
            .iter()
            .filter_map(|d| weekday_to_ics(*d))
            .collect();
        if !days.is_empty() {
            parts.push(format!("BYDAY={}", days.join(",")));
        }
    }
    Some(parts.join(";"))
}

fn weekday_to_ics(d: i64) -> Option<&'static str> {
    match d {
        0 => Some("SU"),
        1 => Some("MO"),
        2 => Some("TU"),
        3 => Some("WE"),
        4 => Some("TH"),
        5 => Some("FR"),
        6 => Some("SA"),
        _ => None,
    }
}

fn ics_from_weekday(s: &str) -> Option<i64> {
    match s.trim().to_uppercase().as_str() {
        "SU" => Some(0),
        "MO" => Some(1),
        "TU" => Some(2),
        "WE" => Some(3),
        "TH" => Some(4),
        "FR" => Some(5),
        "SA" => Some(6),
        _ => None,
    }
}

fn ics_date(ms: i64) -> String {
    dt_from_ms(ms).format("%Y%m%d").to_string()
}
fn ics_datetime(ms: i64) -> String {
    dt_from_ms(ms).format("%Y%m%dT%H%M%S").to_string()
}
pub(crate) fn ics_utc_now() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

pub(crate) fn escape_text(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "")
        .replace(',', "\\,")
        .replace(';', "\\;")
}

pub(crate) fn unescape_text(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') | Some('N') => out.push('\n'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Fold a logical line at 74 octets per RFC 5545 (continuation lines start with a space).
pub(crate) fn fold_line(out: &mut String, line: &str) {
    let bytes = line.as_bytes();
    if bytes.len() <= 74 {
        out.push_str(line);
        out.push_str("\r\n");
        return;
    }
    let mut start = 0;
    let mut first = true;
    while start < bytes.len() {
        // Respect UTF-8 boundaries when slicing.
        let mut end = (start + if first { 74 } else { 73 }).min(bytes.len());
        while end < bytes.len() && (bytes[end] & 0xC0) == 0x80 {
            end -= 1;
        }
        if !first {
            out.push(' ');
        }
        out.push_str(&line[start..end]);
        out.push_str("\r\n");
        start = end;
        first = false;
    }
}

// ── ICS parsing ──

pub(crate) fn ics_unfold(input: &str) -> Vec<String> {
    let mut logical: Vec<String> = Vec::new();
    for raw in input.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if (line.starts_with(' ') || line.starts_with('\t')) && !logical.is_empty() {
            if let Some(last) = logical.last_mut() {
                last.push_str(&line[1..]);
                continue;
            }
        }
        logical.push(line.to_string());
    }
    logical
}

pub(crate) struct IcsProp {
    pub name: String,
    pub params: Vec<(String, String)>,
    pub value: String,
}

pub(crate) fn parse_prop(line: &str) -> Option<IcsProp> {
    let colon = line.find(':')?;
    let (head, value) = line.split_at(colon);
    let value = &value[1..];
    let mut segs = head.split(';');
    let name = segs.next()?.trim().to_uppercase();
    let mut params = Vec::new();
    for seg in segs {
        if let Some(eq) = seg.find('=') {
            params.push((seg[..eq].trim().to_uppercase(), seg[eq + 1..].trim().to_string()));
        } else {
            params.push((seg.trim().to_uppercase(), String::new()));
        }
    }
    Some(IcsProp {
        name,
        params,
        value: value.to_string(),
    })
}

/// Convert an ICS DATE/DATE-TIME value into (iso_string, is_date_only).
pub(crate) fn parse_ics_datetime(value: &str, is_date: bool) -> Option<(String, bool)> {
    let v = value.trim().trim_end_matches('Z');
    if is_date || (v.len() == 8 && !v.contains('T')) {
        // YYYYMMDD
        let d = NaiveDate::parse_from_str(v, "%Y%m%d").ok()?;
        return Some((d.format("%Y-%m-%d").to_string(), true));
    }
    // YYYYMMDDTHHMMSS or YYYYMMDDTHHMM
    for fmt in ["%Y%m%dT%H%M%S", "%Y%m%dT%H%M"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(v, fmt) {
            return Some((dt.format("%Y-%m-%dT%H:%M").to_string(), false));
        }
    }
    // Fall back to date-only if it looks like a bare date.
    if let Ok(d) = NaiveDate::parse_from_str(v, "%Y%m%d") {
        return Some((d.format("%Y-%m-%d").to_string(), true));
    }
    None
}

pub fn parse_rrule(value: &str) -> Recurrence {
    let mut r = Recurrence::default();
    for part in value.split(';') {
        let mut kv = part.splitn(2, '=');
        let key = kv.next().unwrap_or("").trim().to_uppercase();
        let val = kv.next().unwrap_or("").trim();
        match key.as_str() {
            "FREQ" => {
                r.freq = match val.to_uppercase().as_str() {
                    "DAILY" => "daily",
                    "WEEKLY" => "weekly",
                    "MONTHLY" => "monthly",
                    "YEARLY" => "yearly",
                    _ => "none",
                }
                .to_string();
            }
            "INTERVAL" => r.interval = val.parse().unwrap_or(1).max(1),
            "COUNT" => r.count = val.parse().unwrap_or(0).max(0),
            "UNTIL" => {
                if let Some((iso, _)) = parse_ics_datetime(val, false) {
                    // Keep just the date portion for our simplified UNTIL.
                    r.until = iso.split('T').next().unwrap_or("").to_string();
                }
            }
            "BYDAY" => {
                r.by_weekday = val
                    .split(',')
                    .filter_map(|tok| {
                        // Strip any leading ordinal like "2MO" → "MO".
                        let t = tok.trim_start_matches(|c: char| c == '+' || c == '-' || c.is_ascii_digit());
                        ics_from_weekday(t)
                    })
                    .collect();
            }
            _ => {}
        }
    }
    r
}

fn read_zone_observances(lines: &[String]) -> std::collections::BTreeMap<String, Vec<ZoneObservance>> {
    fn read_offset(value: &str) -> Option<i32> {
        let sign = match value.as_bytes().first()? { b'-' => -1, b'+' => 1, _ => return None };
        let digits = value.get(1..)?;
        if digits.len() != 4 || !digits.bytes().all(|byte| byte.is_ascii_digit()) { return None; }
        Some(sign * (digits.get(..2)?.parse::<i32>().ok()? * 60 + digits.get(2..)?.parse::<i32>().ok()?))
    }
    let mut zones = std::collections::BTreeMap::new();
    let mut inside = false;
    let mut zone = String::new();
    let mut rules = Vec::new();
    let (mut onset, mut from, mut to) = (None, None, None);
    let mut simple = true;
    for line in lines {
        let Some(prop) = parse_prop(line) else { continue };
        match (prop.name.as_str(), prop.value.as_str()) {
            ("BEGIN", "VTIMEZONE") => {
                inside = true;
                simple = true;
                rules.clear();
                zone.clear();
            }
            ("END", "VTIMEZONE") if inside => {
                inside = false;
                if simple && valid_zone_id(&zone) && !rules.is_empty() && zones.len() < 128 {
                    zones.insert(std::mem::take(&mut zone), std::mem::take(&mut rules));
                }
            }
            ("BEGIN", "STANDARD" | "DAYLIGHT") if inside => {
                (onset, from, to) = (None, None, None);
            }
            ("END", "STANDARD" | "DAYLIGHT") if inside && rules.len() < 128 => {
                if let (Some(onset), Some(from), Some(to)) = (onset.take(), from, to) {
                    rules.push(ZoneObservance { onset, from, to });
                }
            }
            ("TZID", value) if inside => zone = value.to_owned(),
            ("DTSTART", value) if inside => onset = parse_ics_datetime(value, false).map(|(iso, _)| iso),
            ("TZOFFSETFROM", value) if inside => from = read_offset(value),
            ("TZOFFSETTO", value) if inside => to = read_offset(value),
            ("RRULE" | "RDATE", _) if inside => simple = false,
            _ => (),
        }
    }
    zones
}

pub fn parse_ics(input: &str) -> Vec<EventCard> {
    let lines = ics_unfold(input);
    let observances = read_zone_observances(&lines);
    let mut cards: Vec<EventCard> = Vec::new();
    let mut cur: Option<EventCard> = None;
    let mut end_is_date = false;

    for line in &lines {
        let upper = line.to_uppercase();
        if upper.starts_with("BEGIN:VEVENT") {
            cur = Some(EventCard::default());
            end_is_date = false;
            continue;
        }
        if upper.starts_with("END:VEVENT") {
            if let Some(mut card) = cur.take() {
                if card.status.is_empty() {
                    card.status = "confirmed".to_string();
                }
                // For all-day events the ICS DTEND is exclusive; convert back to
                // our inclusive last-day representation.
                if card.all_day && end_is_date {
                    if let (Some(s), Some(e)) =
                        (parse_iso_ms(&card.start), parse_iso_ms(&card.end))
                    {
                        if e > s {
                            card.end = iso_date(e - DAY_MS);
                        }
                    }
                }
                for zone in [&card.time_zone, &card.end_time_zone] {
                    if let Some(rules) = observances.get(zone) {
                        card.zone_observances.insert(zone.clone(), rules.clone());
                    }
                }
                cards.push(card);
            }
            continue;
        }
        let Some(card) = cur.as_mut() else { continue };
        let Some(prop) = parse_prop(line) else { continue };
        let is_date_param = prop
            .params
            .iter()
            .any(|(k, v)| k == "VALUE" && v.eq_ignore_ascii_case("DATE"));
        match prop.name.as_str() {
            "UID" => card.uid = unescape_text(prop.value.trim()),
            "SUMMARY" => card.title = unescape_text(&prop.value),
            "LOCATION" => card.location = unescape_text(&prop.value),
            "DESCRIPTION" => card.description = unescape_text(&prop.value),
            "DTSTART" => {
                if let Some((iso, is_date)) = parse_ics_datetime(&prop.value, is_date_param) {
                    card.start = iso;
                    card.time_zone = prop_time_zone(&prop);
                    if is_date {
                        card.all_day = true;
                    }
                }
            }
            "DTEND" | "DTEND;VALUE=DATE" => {
                if let Some((iso, is_date)) = parse_ics_datetime(&prop.value, is_date_param) {
                    card.end = iso;
                    card.end_time_zone = prop_time_zone(&prop);
                    end_is_date = is_date;
                }
            }
            "STATUS" => {
                card.status = match prop.value.trim().to_uppercase().as_str() {
                    "TENTATIVE" => "tentative",
                    "CANCELLED" => "cancelled",
                    _ => "confirmed",
                }
                .to_string();
            }
            "RRULE" => card.recurrence = parse_rrule(&prop.value),
            "RECURRENCE-ID" => {
                card.recurrence_id = parse_ics_datetime(&prop.value, is_date_param)
                    .and_then(|(iso, _)| parse_iso_ms(&iso));
            }
            "EXDATE" => {
                card.exclusions.extend(prop.value.split(',')
                    .filter_map(|value| parse_ics_datetime(value, is_date_param))
                    .filter_map(|(iso, _)| parse_iso_ms(&iso)));
            }
            "ATTENDEE" => {
                let email = prop
                    .value
                    .trim()
                    .trim_start_matches("mailto:")
                    .trim_start_matches("MAILTO:")
                    .trim()
                    .to_string();
                let name = prop
                    .params
                    .iter()
                    .find(|(k, _)| k == "CN")
                    .map(|(_, v)| unescape_text(v.trim_matches('"')))
                    .unwrap_or_default();
                if !email.is_empty() {
                    card.attendees.push(Attendee {
                        name,
                        email,
                        status: String::new(),
                    });
                }
            }
            _ => {}
        }
    }
    cards
}

// ─────────────────────────── misc ───────────────────────────

pub fn new_uid() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!(
        "guvercin-{}-{:08x}",
        chrono::Utc::now().timestamp_millis(),
        nanos
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(start: &str, end: &str, all_day: bool) -> EventCard {
        EventCard {
            start: start.into(),
            end: end.into(),
            all_day,
            ..EventCard::default()
        }
    }

    #[test]
    fn series_ics_round_trips_moved_and_cancelled_occurrences() {
        let master = EventCard {
            uid: "agenda-42".into(), start: "2026-11-28T09:00".into(), end: "2026-11-28T10:00".into(),
            time_zone: "Europe/Istanbul".into(),
            recurrence: Recurrence { freq: "daily".into(), interval: 1, count: 4, ..Default::default() },
            ..Default::default()
        };
        let original = occurrence_key("2026-11-29T09:00").unwrap();
        let mut modified = master.clone();
        modified.start = "2026-11-29T13:00".into();
        modified.end = "2026-11-29T14:00".into();
        modified.title = "Moved and edited".into();
        let change = OccurrenceChange::from_remote(&master, original, Some(modified)).unwrap();
        let cancelled = OccurrenceChange::from_remote(&master, occurrence_key("2026-11-30T09:00").unwrap(), None).unwrap();
        let ics = build_ics_series(&master, &[change, cancelled]);
        assert!(ics.contains("RECURRENCE-ID;TZID=Europe/Istanbul:20261129T090000"));
        assert!(ics.contains("EXDATE;TZID=Europe/Istanbul:20261130T090000"));
        let cards = parse_ics(&ics);
        let changes = ics_occurrences(&cards[0], &cards);
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].card(&cards[0]).title, "Moved and edited");
        assert_eq!(changes[0].card(&cards[0]).start, "2026-11-29T13:00");
        assert!(changes[1].is_deleted);

        let original = build_ics_series(&master, &[]).replace("UID:agenda-42",
            "UID:agenda-42\r\nORGANIZER:mailto:owner@example.test\r\nX-MEETING-KEY:example");
        let edits: Vec<_> = changes.into_iter().map(|mut change| { change.revision = 1; change }).collect();
        let patched = patch_ics_occurrences(&original, &master, &edits).unwrap();
        assert!(patched.contains("ORGANIZER:mailto:owner@example.test"));
        assert!(patched.contains("X-MEETING-KEY:example"));
        let round_trip = parse_ics(&patched);
        assert_eq!(ics_occurrences(&round_trip[0], &round_trip).len(), 2);
    }

    #[test]
    fn timed_bounds_default_hour() {
        let (s, e) = compute_bounds(&ev("2026-07-23T09:00", "", false));
        assert_eq!(e - s, 3_600_000); // empty end → +1h
    }

    #[test]
    fn all_day_end_is_exclusive() {
        // A single all-day event should span exactly one day.
        let (s, e) = compute_bounds(&ev("2026-07-23", "2026-07-23", true));
        assert_eq!(e - s, DAY_MS);
    }

    #[test]
    fn non_recurring_in_window() {
        let c = ev("2026-07-23T09:00", "2026-07-23T10:00", false);
        let from = parse_iso_ms("2026-07-01T00:00").unwrap();
        let to = parse_iso_ms("2026-08-01T00:00").unwrap();
        assert_eq!(expand_occurrences(&c, from, to).len(), 1);
        // Outside the window → nothing.
        let from2 = parse_iso_ms("2026-09-01T00:00").unwrap();
        let to2 = parse_iso_ms("2026-10-01T00:00").unwrap();
        assert_eq!(expand_occurrences(&c, from2, to2).len(), 0);
    }

    #[test]
    fn daily_with_count() {
        let mut c = ev("2026-07-01T09:00", "2026-07-01T10:00", false);
        c.recurrence = Recurrence { freq: "daily".into(), interval: 1, count: 5, ..Default::default() };
        let from = parse_iso_ms("2026-07-01T00:00").unwrap();
        let to = parse_iso_ms("2026-08-01T00:00").unwrap();
        // COUNT=5 caps the series regardless of the (larger) window.
        assert_eq!(expand_occurrences(&c, from, to).len(), 5);
    }

    #[test]
    fn weekly_byweekday_within_window() {
        // Every Mon/Wed/Fri for one week.
        let mut c = ev("2026-07-06T09:00", "2026-07-06T10:00", false); // Monday
        c.recurrence = Recurrence {
            freq: "weekly".into(),
            interval: 1,
            by_weekday: vec![1, 3, 5],
            ..Default::default()
        };
        let from = parse_iso_ms("2026-07-06T00:00").unwrap();
        let to = parse_iso_ms("2026-07-13T00:00").unwrap();
        assert_eq!(expand_occurrences(&c, from, to).len(), 3);
    }

    #[test]
    fn recurrence_far_start_is_fast_forwarded() {
        // Master starts years before the window; expansion must still be cheap
        // and land the right daily occurrences inside a narrow window.
        let mut c = ev("2000-01-01T09:00", "2000-01-01T10:00", false);
        c.recurrence = Recurrence { freq: "daily".into(), interval: 1, ..Default::default() };
        let from = parse_iso_ms("2026-07-01T00:00").unwrap();
        let to = parse_iso_ms("2026-07-08T00:00").unwrap();
        assert_eq!(expand_occurrences(&c, from, to).len(), 7);
    }

    #[test]
    fn ics_roundtrip() {
        let mut c = ev("2026-07-23T09:00", "2026-07-23T10:30", false);
        c.title = "Team sync, weekly".into(); // comma must survive escaping
        c.location = "Room 5".into();
        c.uid = "abc-123".into();
        c.recurrence = Recurrence { freq: "weekly".into(), interval: 1, by_weekday: vec![1, 3], ..Default::default() };
        let ics = build_ics(std::slice::from_ref(&c));
        assert!(ics.contains("BEGIN:VEVENT"));
        assert!(ics.contains("RRULE:FREQ=WEEKLY"));
        let parsed = parse_ics(&ics);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].title, "Team sync, weekly");
        assert_eq!(parsed[0].start, "2026-07-23T09:00");
        assert_eq!(parsed[0].recurrence.freq, "weekly");
        assert_eq!(parsed[0].recurrence.by_weekday, vec![1, 3]);
    }

    #[test]
    fn ics_all_day_roundtrip() {
        let c = ev("2026-07-23", "2026-07-24", true); // 2-day all-day event (inclusive 23–24)
        let ics = build_ics(std::slice::from_ref(&c));
        // DTEND should be exclusive (the 25th) in the ICS output.
        assert!(ics.contains("DTSTART;VALUE=DATE:20260723"));
        assert!(ics.contains("DTEND;VALUE=DATE:20260725"));
        let parsed = parse_ics(&ics);
        assert_eq!(parsed[0].start, "2026-07-23");
        assert_eq!(parsed[0].end, "2026-07-24"); // back to inclusive
        assert!(parsed[0].all_day);
    }
}
