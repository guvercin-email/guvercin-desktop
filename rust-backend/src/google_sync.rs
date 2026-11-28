//! Two-way sync between the local stores and Google (Calendar v3, People v1,
//! Tasks v1) for accounts signed in with Google (`provider_type = 'gmail'`).
//!
//! Each run: fetch the remote set, reconcile it against local rows (last-write-
//! wins by timestamp — see [`crate::sync_reconcile`]), then push local creates,
//! edits and deletions back to Google. Rows are matched by a stable remote id
//! (stored in `remote_id`, with a fallback to the legacy `uid` written by the
//! earlier pull-only sync). Deletions are conservative: a remote item is deleted
//! only for an explicit local tombstone, and a local row is dropped for a remote
//! deletion only when it is clean and inside the fetched scope.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    Json,
};
use chrono::{Duration, Local, NaiveDate, NaiveDateTime, TimeZone};
use reqwest::{Method, StatusCode, Url};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{Row, SqlitePool};

use crate::{
    calendar_routes as cal, contacts_routes as con, db, db::AppState, error::AppError, oauth,
    sync_reconcile::{reconcile_matched, reconcile_orphan, LocalState, MatchAction, OrphanAction},
    todo_routes as todo,
};

const CAL_LIST_URL: &str = "https://www.googleapis.com/calendar/v3/users/me/calendarList";
const PEOPLE_URL: &str = "https://people.googleapis.com/v1/people/me/connections";
const TASKLISTS_URL: &str = "https://tasks.googleapis.com/tasks/v1/users/@me/lists";
const DAY_MS: i64 = 86_400_000;
const RECONNECT_MSG: &str =
    "Reconnect your Google account to allow Calendar/Contacts access — sign out and sign in with Google again.";

// ─────────────────────────── shared HTTP ───────────────────────────

async fn gmail_token(state: &Arc<AppState>, account_id: i64) -> Result<String, AppError> {
    let general = state.ensure_ready(false).await?.general_pool.clone();
    let provider: Option<String> =
        sqlx::query_scalar("SELECT provider_type FROM accounts WHERE account_id = ?")
            .bind(account_id)
            .fetch_optional(&general)
            .await?;
    if provider.as_deref() != Some(oauth::PROVIDER_GMAIL) {
        return Err(AppError::BadRequest(
            "This account is not a Google account, so there is nothing to sync.".into(),
        ));
    }
    oauth::access_token_for_account(&general, account_id)
        .await
        .map_err(AppError::BadRequest)
}

async fn google_get<T: DeserializeOwned>(
    http: &reqwest::Client,
    url: &str,
    token: &str,
) -> Result<T, AppError> {
    google_send(http, Method::GET, url, token, None).await
}

#[derive(Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
struct GooglePage<T> {
    #[serde(default, alias = "connections")]
    items: Vec<T>,
    #[serde(default, rename = "nextPageToken")]
    next_page_token: Option<String>,
}

async fn google_pages<T: DeserializeOwned>(
    http: &reqwest::Client,
    token: &str,
    endpoint: &str,
    parameters: &[(&str, &str)],
    page_limit: usize,
) -> Result<Vec<T>, AppError> {
    let mut base = Url::parse(endpoint)
        .map_err(|error| AppError::BadRequest(format!("Invalid Google endpoint: {error}")))?;
    base.query_pairs_mut().extend_pairs(parameters.iter().copied());
    let mut page_token: Option<String> = None;
    let mut items = Vec::new();
    for _ in 0..page_limit {
        let mut url = base.clone();
        if let Some(page_token) = &page_token {
            url.query_pairs_mut().append_pair("pageToken", page_token);
        }
        let page: GooglePage<T> = google_get(http, url.as_str(), token).await?;
        items.extend(page.items);
        page_token = page.next_page_token.filter(|value| !value.is_empty());
        if page_token.is_none() {
            return Ok(items);
        }
    }
    Err(AppError::BadRequest(
        "Google returned more pages than the sync limit. Existing items were kept.".into(),
    ))
}

async fn google_send<T: DeserializeOwned>(
    http: &reqwest::Client,
    method: Method,
    url: &str,
    token: &str,
    body: Option<&Value>,
) -> Result<T, AppError> {
    let mut req = http.request(method, url).bearer_auth(token);
    if let Some(b) = body {
        req = req.json(b);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| AppError::BadRequest(format!("Google request failed: {e}")))?;
    let status = resp.status();
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(AppError::BadRequest(RECONNECT_MSG.into()));
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let snippet: String = body.chars().take(200).collect();
        return Err(AppError::BadRequest(format!(
            "Google API error ({}): {}",
            status.as_u16(),
            snippet
        )));
    }
    resp.json::<T>()
        .await
        .map_err(|e| AppError::BadRequest(format!("Could not parse Google response: {e}")))
}

/// DELETE that tolerates already-gone (404/410) as success.
async fn google_delete(http: &reqwest::Client, url: &str, token: &str) -> Result<(), AppError> {
    let resp = http
        .delete(url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| AppError::BadRequest(format!("Google request failed: {e}")))?;
    let status = resp.status();
    if status.is_success() || status == StatusCode::NOT_FOUND || status == StatusCode::GONE {
        Ok(())
    } else if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        Err(AppError::BadRequest(RECONNECT_MSG.into()))
    } else {
        Err(AppError::BadRequest(format!(
            "Google delete failed ({})",
            status.as_u16()
        )))
    }
}

fn label_of(t: &Option<String>) -> String {
    match t.as_deref() {
        Some(s) if !s.trim().is_empty() => s.trim().to_lowercase(),
        _ => "other".to_string(),
    }
}

fn rfc3339_to_ms(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(s.trim())
        .map(|d| d.timestamp_millis())
        .unwrap_or(0)
}

/// Interpret a naive local ISO string as a timezone-aware RFC3339 timestamp.
fn local_rfc3339(iso: &str) -> String {
    for fmt in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"] {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(iso.trim(), fmt) {
            if let Some(dt) = Local.from_local_datetime(&ndt).earliest() {
                return dt.to_rfc3339();
            }
        }
    }
    iso.to_string()
}

fn plus_one_day_date(date: &str) -> String {
    NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d")
        .map(|d| (d + Duration::days(1)).format("%Y-%m-%d").to_string())
        .unwrap_or_else(|_| date.to_string())
}

fn minus_one_day(date: &str) -> String {
    NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d")
        .map(|d| (d - Duration::days(1)).format("%Y-%m-%d").to_string())
        .unwrap_or_else(|_| date.to_string())
}

fn v_str(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

async fn set_remote_id(pool: &SqlitePool, table: &str, id_col: &str, id: i64, remote_id: &str) {
    let _ = sqlx::query(&format!("UPDATE {table} SET remote_id = ? WHERE {id_col} = ?"))
        .bind(remote_id)
        .bind(id)
        .execute(pool)
        .await;
}

// ─────────────────────────── status ───────────────────────────

pub async fn google_status(
    State(state): State<Arc<AppState>>,
    Path(account_id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let general = state.ensure_ready(false).await?.general_pool.clone();
    let provider: Option<String> =
        sqlx::query_scalar("SELECT provider_type FROM accounts WHERE account_id = ?")
            .bind(account_id)
            .fetch_optional(&general)
            .await?;
    let is_gmail = provider.as_deref() == Some(oauth::PROVIDER_GMAIL);
    Ok(Json(json!({
        "provider": provider,
        "gmail": is_gmail,
        "configured": oauth::is_configured(),
        "available": is_gmail && oauth::is_configured(),
    })))
}

/// Probe whether the stored Google token actually grants access to one service.
///
/// `google_status` only reports whether the account is Gmail and OAuth is built
/// in; it can't tell whether the *calendar* / *contacts* / *tasks* scope was
/// granted. Accounts that signed in before a scope was added keep a token without
/// it, so a real API call is the only reliable signal. Each probe is one cheap
/// list request (a 401/403 ⇒ not granted). Used by the three tabs to decide whether
/// to prompt the user to reconnect.
async fn service_access(
    state: &Arc<AppState>,
    account_id: i64,
    probe_url: &str,
) -> Result<Json<Value>, AppError> {
    let general = state.ensure_ready(false).await?.general_pool.clone();
    let provider: Option<String> =
        sqlx::query_scalar("SELECT provider_type FROM accounts WHERE account_id = ?")
            .bind(account_id)
            .fetch_optional(&general)
            .await?;
    let is_gmail = provider.as_deref() == Some(oauth::PROVIDER_GMAIL);
    let configured = oauth::is_configured();
    if !is_gmail || !configured {
        return Ok(Json(json!({ "gmail": is_gmail, "configured": configured, "granted": false })));
    }
    // Best-effort: any failure (no token, revoked scope, transient) ⇒ not granted.
    let granted = match oauth::access_token_for_account(&general, account_id).await {
        Ok(token) => {
            let http = reqwest::Client::new();
            google_get::<Value>(&http, probe_url, &token).await.is_ok()
        }
        Err(_) => false,
    };
    Ok(Json(json!({ "gmail": true, "configured": true, "granted": granted })))
}

pub async fn calendar_access(
    State(state): State<Arc<AppState>>,
    Path(account_id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    service_access(&state, account_id, &format!("{CAL_LIST_URL}?maxResults=1&fields=kind")).await
}

pub async fn contacts_access(
    State(state): State<Arc<AppState>>,
    Path(account_id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    service_access(
        &state,
        account_id,
        &format!("{PEOPLE_URL}?pageSize=1&personFields=names"),
    )
    .await
}

pub async fn tasks_access(
    State(state): State<Arc<AppState>>,
    Path(account_id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    service_access(&state, account_id, &format!("{TASKLISTS_URL}?maxResults=1")).await
}

// ─────────────────────────── Calendar ───────────────────────────

#[derive(Deserialize, Default)]
struct CalListItem {
    #[serde(default)]
    id: String,
    #[serde(default)]
    summary: String,
    #[serde(default, rename = "backgroundColor")]
    background_color: Option<String>,
    #[serde(default, rename = "accessRole")]
    access_role: Option<String>,
}
#[derive(Deserialize, Default)]
struct GEvent {
    #[serde(default)]
    id: String,
    #[serde(default)]
    etag: Option<String>,
    #[serde(default)]
    updated: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    location: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    start: Option<GTime>,
    #[serde(default)]
    end: Option<GTime>,
    #[serde(default)]
    recurrence: Option<Vec<String>>,
    #[serde(default, rename = "recurringEventId")]
    recurring_event_id: Option<String>,
    #[serde(default, rename = "originalStartTime")]
    original_start_time: Option<GTime>,
    #[serde(default)]
    attendees: Option<Vec<GAttendee>>,
}
#[derive(Deserialize, Default)]
struct GTime {
    #[serde(default)]
    date: Option<String>,
    #[serde(default, rename = "dateTime")]
    date_time: Option<String>,
    #[serde(default, rename = "timeZone")]
    time_zone: Option<String>,
}
#[derive(Deserialize, Default)]
struct GAttendee {
    #[serde(default)]
    email: Option<String>,
    #[serde(default, rename = "displayName")]
    display_name: Option<String>,
    #[serde(default, rename = "responseStatus")]
    response_status: Option<String>,
}

fn gtime_to_iso(g: &GTime) -> Option<(String, bool)> {
    if let Some(date) = &g.date {
        if !date.trim().is_empty() {
            return Some((date.trim().to_string(), true));
        }
    }
    if let Some(dt) = &g.date_time {
        let parsed = chrono::DateTime::parse_from_rfc3339(dt.trim()).ok()?;
        let wall = if g.time_zone.as_deref().is_some_and(|zone| !zone.is_empty()) {
            parsed.naive_local()
        } else {
            parsed.naive_utc()
        };
        return Some((wall.format("%Y-%m-%dT%H:%M").to_string(), false));
    }
    None
}

fn map_event(g: &GEvent, calendar_id: i64) -> Option<cal::EventCard> {
    let start = g.start.as_ref()?;
    let (start_iso, all_day) = gtime_to_iso(start)?;
    let end_iso = g.end.as_ref().and_then(gtime_to_iso).map(|(s, _)| s);
    let mut card = cal::EventCard {
        uid: format!("google-{}", g.id),
        calendar_id: Some(calendar_id),
        title: g.summary.clone().unwrap_or_default(),
        location: g.location.clone().unwrap_or_default(),
        description: g.description.clone().unwrap_or_default(),
        all_day,
        start: start_iso.clone(),
        ..Default::default()
    };
    if all_day {
        card.end = match end_iso {
            Some(e) if e != start_iso => minus_one_day(&e),
            _ => start_iso,
        };
    } else {
        card.end = end_iso.unwrap_or_else(|| card.start.clone());
        card.time_zone = start.time_zone.clone().filter(|zone| !zone.is_empty()).unwrap_or_else(|| "UTC".into());
        if let Some(end) = &g.end {
            card.end_time_zone = end.time_zone.clone().filter(|zone| !zone.is_empty()).unwrap_or_else(|| "UTC".into());
        }
    }
    card.status = match g.status.as_deref() {
        Some("tentative") => "tentative",
        _ => "confirmed",
    }
    .to_string();
    if let Some(lines) = &g.recurrence {
        for line in lines {
            if let Some(rest) = line.strip_prefix("RRULE:") {
                card.recurrence = cal::parse_rrule(rest);
                break;
            }
        }
    }
    if let Some(atts) = &g.attendees {
        card.attendees = atts
            .iter()
            .filter_map(|a| {
                a.email.clone().map(|email| cal::Attendee {
                    name: a.display_name.clone().unwrap_or_default(),
                    email,
                    status: a.response_status.clone().unwrap_or_default(),
                })
            })
            .collect();
    }
    Some(card)
}

fn event_to_google(card: &cal::EventCard) -> Value {
    let mut obj = json!({
        "summary": card.title,
        "location": card.location,
        "description": card.description,
        "status": if card.status == "tentative" { "tentative" } else { "confirmed" },
    });
    if card.all_day {
        let start = card.start.get(0..10).unwrap_or(&card.start).to_string();
        let end_inclusive = if card.end.trim().is_empty() { start.clone() } else { card.end.get(0..10).unwrap_or(&card.end).to_string() };
        obj["start"] = json!({ "date": start });
        obj["end"] = json!({ "date": plus_one_day_date(&end_inclusive) });
    } else {
        let as_google_time = |wall: &str, zone: &str| {
            if zone.is_empty() {
                json!({ "dateTime": local_rfc3339(wall) })
            } else {
                let wall = if wall.len() == 16 { format!("{wall}:00") } else { wall.to_owned() };
                json!({ "dateTime": wall, "timeZone": zone })
            }
        };
        let end = if card.end.trim().is_empty() { &card.start } else { &card.end };
        let end_zone = if card.end_time_zone.is_empty() { &card.time_zone } else { &card.end_time_zone };
        obj["start"] = as_google_time(&card.start, &card.time_zone);
        obj["end"] = as_google_time(end, end_zone);
    }
    if let Some(rrule) = cal::build_rrule(card) {
        obj["recurrence"] = json!([format!("RRULE:{rrule}")]);
    }
    let attendees: Vec<Value> = card
        .attendees
        .iter()
        .filter(|a| !a.email.trim().is_empty())
        .map(|a| json!({ "email": a.email, "displayName": a.name }))
        .collect();
    if !attendees.is_empty() {
        obj["attendees"] = json!(attendees);
    }
    obj
}

struct LocalRow {
    id: i64,
    remote_id: Option<String>,
    uid: String,
    etag: Option<String>,
    state: LocalState,
    card_json: String,
    aux: i64, // events: calendar_id ; tasks: list_id ; contacts: unused
    scope_ms: i64, // events: start_ms ; others: 0
    recurs: bool,
}

fn parse_state(row: &sqlx::sqlite::SqliteRow) -> LocalState {
    let remote_id: Option<String> = row.try_get("remote_id").ok().flatten();
    LocalState {
        dirty: row.try_get::<i64, _>("dirty").unwrap_or(0) != 0,
        deleted: row.try_get::<i64, _>("deleted").unwrap_or(0) != 0,
        has_remote_id: remote_id.as_deref().map(|s| !s.is_empty()).unwrap_or(false),
        remote_updated_ms: row.try_get::<Option<i64>, _>("remote_updated_ms").ok().flatten().unwrap_or(0),
        local_updated_ms: row.try_get::<Option<i64>, _>("local_updated_ms").ok().flatten().unwrap_or(0),
    }
}

/// Resolve the Google calendar id a local event should be written to: its local
/// calendar's mirror, or the primary calendar for local-only calendars.
async fn target_gcal(pool: &SqlitePool, calendar_id: i64) -> String {
    sqlx::query_scalar::<_, Option<String>>("SELECT remote_id FROM calendars WHERE calendar_id = ?")
        .bind(calendar_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .flatten()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "primary".to_string())
}

fn events_url(gcal: &str, event: Option<&str>) -> String {
    let mut url = Url::parse("https://www.googleapis.com/").unwrap();
    {
        let mut segs = url.path_segments_mut().unwrap();
        segs.extend(&["calendar", "v3", "calendars", gcal, "events"]);
        if let Some(ev) = event {
            segs.push(ev);
        }
    }
    url.to_string()
}

fn original_occurrence_key(original: &GTime, master: &cal::EventCard) -> Option<i64> {
    if let Some(date) = &original.date {
        return cal::occurrence_key(date);
    }
    let date = chrono::DateTime::parse_from_rfc3339(original.date_time.as_deref()?).ok()?;
    let wall = if master.time_zone.is_empty() {
        date.with_timezone(&Local).naive_local()
    } else if master.time_zone == "UTC" {
        date.naive_utc()
    } else {
        date.naive_local()
    };
    Some(wall.and_utc().timestamp_millis())
}

async fn sync_event_occurrences(
    http: &reqwest::Client,
    token: &str,
    pool: &SqlitePool,
    local_id: i64,
    gcal: &str,
    event_id: &str,
    master: &cal::EventCard,
    remote: &[GEvent],
) -> Result<usize, AppError> {
    if !master.recurrence.is_active() {
        return Ok(0);
    }
    let changes: Vec<_> = remote.iter().filter_map(|instance| {
        let original = original_occurrence_key(instance.original_start_time.as_ref()?, master)?;
        let override_card = if instance.status.as_deref() == Some("cancelled") {
            None
        } else {
            Some(map_event(instance, master.calendar_id.unwrap_or_default())?)
        };
        cal::OccurrenceChange::from_remote(master, original, override_card)
    }).collect();
    cal::receive_occurrences(pool, local_id, &changes, false).await?;
    let edits = cal::occurrence_changes(pool, local_id, master).await?;
    let mut count = 0;
    for change in edits.iter().filter(|change| change.revision != 0) {
        let mut target = remote.iter().find(|event| event.original_start_time.as_ref()
            .and_then(|original| original_occurrence_key(original, master)) == Some(change.occurrence_start));
        let mut instances = Vec::new();
        if target.is_none() {
            let mut url = Url::parse(&events_url(gcal, Some(event_id))).expect("Google events URL is valid");
            url.path_segments_mut().expect("Google events URL is hierarchical").push("instances");
            let original = chrono::DateTime::from_timestamp_millis(change.occurrence_start)
                .ok_or_else(|| AppError::BadRequest("Invalid occurrence date".into()))?;
            let from = (original - Duration::days(2)).to_rfc3339();
            let to = (original + Duration::days(2)).to_rfc3339();
            let mut query = vec![("timeMin", from.as_str()), ("timeMax", to.as_str()), ("showDeleted", "true")];
            if !master.time_zone.is_empty() {
                query.push(("timeZone", master.time_zone.as_str()));
            }
            instances.extend(google_pages::<GEvent>(http, token, url.as_str(), &query, 10).await?);
            target = instances.iter().find(|event| event.original_start_time.as_ref()
                .and_then(|original| original_occurrence_key(original, master)) == Some(change.occurrence_start));
        }
        let Some(instance) = target.filter(|event| !event.id.is_empty()) else {
            // The provider may have changed the series since the edit was made.
            continue;
        };
        let url = events_url(gcal, Some(&instance.id));
        if change.is_deleted {
            google_delete(http, &url, token).await?;
        } else {
            let mut body = event_to_google(&change.card(master));
            if let Some(fields) = body.as_object_mut() {
                fields.remove("recurrence");
            }
            let _: Value = google_send(http, Method::PATCH, &url, token, Some(&body)).await?;
        }
        cal::mark_occurrence_pushed(pool, local_id, change).await?;
        count += 1;
    }
    Ok(count)
}

pub async fn sync_calendar(
    State(state): State<Arc<AppState>>,
    Path(account_id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let lock = state.workspace_sync_lock(account_id, "calendar").await;
    let _guard = lock.lock().await;
    let token = gmail_token(&state, account_id).await?;
    let pool = db::get_user_db_pool(&state, account_id).await?;
    let http = reqwest::Client::new();

    // Fetch window (absolute for the API, naive for local in-scope checks).
    let now = chrono::Utc::now();
    let time_min = (now - Duration::days(365)).to_rfc3339();
    let time_max = (now + Duration::days(365)).to_rfc3339();
    let naive_now = Local::now().naive_local().and_utc().timestamp_millis();
    let win_from = naive_now - 365 * DAY_MS;
    let win_to = naive_now + 365 * DAY_MS;

    // 1. Mirror remote calendars → local, recording remote ids.
    let calendars = google_pages::<CalListItem>(&http, &token, CAL_LIST_URL, &[], 20).await?;
    let mut gcal_to_local: HashMap<String, i64> = HashMap::new();
    for item in &calendars {
        if item.access_role.as_deref() == Some("freeBusyReader") {
            continue;
        }
        let summary = if item.summary.trim().is_empty() { item.id.clone() } else { item.summary.clone() };
        let color = item.background_color.clone().unwrap_or_else(|| "#246bce".into());
        let local_cal = cal::ensure_named_calendar(&pool, &format!("{summary} (Google)"), &color).await?;
        set_remote_id(&pool, "calendars", "calendar_id", local_cal, &item.id).await;
        gcal_to_local.insert(item.id.clone(), local_cal);
    }

    // 2. Fetch all remote events (per calendar), remember which gcal each came from.
    let mut remote: Vec<(String, GEvent)> = Vec::new();
    let mut exceptions: HashMap<(String, String), Vec<GEvent>> = HashMap::new();
    for gcal in gcal_to_local.keys() {
        let events = google_pages::<GEvent>(
            &http, &token, &events_url(gcal, None),
            &[
                ("timeMin", &time_min), ("timeMax", &time_max),
                ("singleEvents", "false"), ("maxResults", "2500"), ("showDeleted", "true"),
            ],
            20,
        ).await?;
        for event in events {
            if let Some(master) = &event.recurring_event_id {
                exceptions.entry((gcal.clone(), master.clone())).or_default().push(event);
            } else if event.status.as_deref() != Some("cancelled") {
                remote.push((gcal.clone(), event));
            }
        }
    }

    // 3. Load local events.
    let rows = sqlx::query(
        "SELECT event_id, calendar_id, event_json, uid, remote_id, etag, dirty, deleted, remote_updated_ms, local_updated_ms, start_ms, recurs FROM events",
    )
    .fetch_all(&pool)
    .await?;
    let locals: Vec<LocalRow> = rows
        .iter()
        .map(|r| LocalRow {
            id: r.try_get("event_id").unwrap_or_default(),
            remote_id: r.try_get("remote_id").ok().flatten(),
            uid: r.try_get::<Option<String>, _>("uid").ok().flatten().unwrap_or_default(),
            etag: r.try_get("etag").ok().flatten(),
            state: parse_state(r),
            card_json: r.try_get::<Option<String>, _>("event_json").ok().flatten().unwrap_or_default(),
            aux: r.try_get::<Option<i64>, _>("calendar_id").ok().flatten().unwrap_or(0),
            scope_ms: r.try_get::<i64, _>("start_ms").unwrap_or(0),
            recurs: r.try_get::<i64, _>("recurs").unwrap_or(0) != 0,
        })
        .collect();

    let mut by_remote: HashMap<String, usize> = HashMap::new();
    let mut by_uid: HashMap<String, usize> = HashMap::new();
    for (i, l) in locals.iter().enumerate() {
        if let Some(rid) = l.remote_id.as_deref().filter(|s| !s.is_empty()) {
            by_remote.insert(rid.to_string(), i);
        }
        if !l.uid.is_empty() {
            by_uid.insert(l.uid.clone(), i);
        }
    }

    let mut seen: HashSet<usize> = HashSet::new();
    let mut pulled = 0i64;
    let mut pushed = 0i64;
    let mut deleted_remote = 0i64;

    let mut occurrence_parents = Vec::new();

    // 4. Reconcile each remote event against local.
    for (gcal, gev) in &remote {
        let local_cal = *gcal_to_local.get(gcal).unwrap_or(&0);
        let Some(card) = map_event(gev, local_cal) else { continue };
        let remote_ms = gev.updated.as_deref().map(rfc3339_to_ms).unwrap_or(0);
        let idx = by_remote.get(&gev.id).or_else(|| by_uid.get(&format!("google-{}", gev.id))).copied();
        match idx {
            None => {
                let local_id = cal::sync_write_event(&pool, None, &card, &gev.id, gev.etag.as_deref(), remote_ms).await?;
                occurrence_parents.push((local_id, gcal.clone(), gev.id.clone(), card));
                pulled += 1;
            }
            Some(i) => {
                seen.insert(i);
                let local = &locals[i];
                match reconcile_matched(local.state, remote_ms) {
                    MatchAction::Noop => {
                        occurrence_parents.push((local.id, gcal.clone(), gev.id.clone(), card));
                    }
                    MatchAction::PullOverwrite => {
                        cal::sync_write_event(&pool, Some(local.id), &card, &gev.id, gev.etag.as_deref(), remote_ms).await?;
                        occurrence_parents.push((local.id, gcal.clone(), gev.id.clone(), card));
                        pulled += 1;
                    }
                    MatchAction::UpdateRemote => {
                        if let Ok(lcard) = serde_json::from_str::<cal::EventCard>(&local.card_json) {
                            let body = event_to_google(&lcard);
                            let resp: Value = google_send(&http, Method::PUT, &events_url(gcal, Some(&gev.id)), &token, Some(&body)).await?;
                            let ms = rfc3339_to_ms(&v_str(&resp, "updated"));
                            cal::sync_mark_pushed(&pool, "events", "event_id", local.id, &gev.id, resp.get("etag").and_then(|x| x.as_str()), ms).await?;
                            occurrence_parents.push((local.id, gcal.clone(), gev.id.clone(), lcard));
                            pushed += 1;
                        }
                    }
                    MatchAction::DeleteRemote => {
                        google_delete(&http, &events_url(gcal, Some(&gev.id)), &token).await?;
                        cal::sync_hard_delete(&pool, "events", "event_id", local.id).await?;
                        deleted_remote += 1;
                    }
                }
            }
        }
    }

    let synced_calendars: HashSet<_> = gcal_to_local.values().copied().collect();

    // 5. Orphans: local events with no matching remote item this round.
    for (i, local) in locals.iter().enumerate() {
        if seen.contains(&i) {
            continue;
        }
        let in_scope = synced_calendars.contains(&local.aux)
            && !local.recurs && local.scope_ms >= win_from && local.scope_ms < win_to;
        match reconcile_orphan(local.state, in_scope) {
            OrphanAction::Noop => {}
            OrphanAction::DropLocal | OrphanAction::DeleteLocal => {
                cal::sync_hard_delete(&pool, "events", "event_id", local.id).await?;
            }
            OrphanAction::CreateRemote | OrphanAction::RecreateRemote => {
                if let Ok(lcard) = serde_json::from_str::<cal::EventCard>(&local.card_json) {
                    let gcal = target_gcal(&pool, local.aux).await;
                    let body = event_to_google(&lcard);
                    let resp: Value = google_send(&http, Method::POST, &events_url(&gcal, None), &token, Some(&body)).await?;
                    let id = v_str(&resp, "id");
                    let ms = rfc3339_to_ms(&v_str(&resp, "updated"));
                    if !id.is_empty() {
                        cal::sync_mark_pushed(&pool, "events", "event_id", local.id, &id, resp.get("etag").and_then(|x| x.as_str()), ms).await?;
                        occurrence_parents.push((local.id, gcal, id, lcard));
                        pushed += 1;
                    }
                }
            }
        }
    }

    for (local_id, calendar, event, card) in occurrence_parents {
        let instances = exceptions.remove(&(calendar.clone(), event.clone())).unwrap_or_default();
        pushed += sync_event_occurrences(&http, &token, &pool, local_id, &calendar, &event, &card, &instances).await? as i64;
    }
    Ok(Json(json!({ "pulled": pulled, "pushed": pushed, "deletedRemote": deleted_remote })))
}

#[derive(Deserialize, Serialize)]
pub struct GoogleAclRule {
    id: String,
    role: String,
    scope: GoogleAclScope,
}

#[derive(Deserialize, Serialize)]
struct GoogleAclScope {
    #[serde(rename = "type")]
    scope_type: String,
    #[serde(default)]
    value: Option<String>,
}

#[derive(Deserialize)]
pub struct CalendarGrant {
    email: String,
    role: String,
}

async fn google_acl_target(state: &Arc<AppState>, account_id: i64, calendar_id: i64) -> Result<(String, String), AppError> {
    let token = gmail_token(state, account_id).await?;
    let pool = db::get_user_db_pool(state, account_id).await?;
    let remote: Option<String> = sqlx::query_scalar("SELECT remote_id FROM calendars WHERE calendar_id = ?")
        .bind(calendar_id).fetch_optional(&pool).await?.flatten();
    let remote = remote.filter(|id| !id.is_empty() && !id.contains("://"))
        .ok_or_else(|| AppError::BadRequest("This is not a Google calendar.".into()))?;
    Ok((token, remote))
}

fn acl_url(calendar: &str, rule: Option<&str>) -> Url {
    let mut url = Url::parse("https://www.googleapis.com").expect("Google API URL is valid");
    let mut path = url.path_segments_mut().expect("Google API URL is hierarchical");
    path.extend(["calendar", "v3", "calendars", calendar, "acl"]);
    if let Some(rule) = rule { path.push(rule); }
    drop(path);
    url
}

pub async fn list_calendar_access(
    State(state): State<Arc<AppState>>,
    Path((account_id, calendar_id)): Path<(i64, i64)>,
) -> Result<Json<Vec<GoogleAclRule>>, AppError> {
    let (token, calendar) = google_acl_target(&state, account_id, calendar_id).await?;
    let rules = google_pages(&reqwest::Client::new(), &token, acl_url(&calendar, None).as_str(), &[], 10).await?;
    Ok(Json(rules))
}

pub async fn grant_calendar_access(
    State(state): State<Arc<AppState>>,
    Path((account_id, calendar_id)): Path<(i64, i64)>,
    Json(grant): Json<CalendarGrant>,
) -> Result<Json<GoogleAclRule>, AppError> {
    let email = grant.email.trim();
    if email.parse::<lettre::Address>().is_err() || !matches!(grant.role.as_str(), "freeBusyReader" | "reader" | "writer") {
        return Err(AppError::BadRequest("Invalid calendar recipient or access level.".into()));
    }
    let (token, calendar) = google_acl_target(&state, account_id, calendar_id).await?;
    let http = reqwest::Client::new();
    let mut list_url = acl_url(&calendar, None);
    let rules: Vec<GoogleAclRule> = google_pages(&http, &token, list_url.as_str(), &[], 10).await?;
    let existing = rules.iter().find(|rule| rule.scope.scope_type == "user"
        && rule.scope.value.as_deref().is_some_and(|value| value.eq_ignore_ascii_case(email)));
    let (method, url, body) = if let Some(rule) = existing {
        if rule.role == "owner" {
            return Err(AppError::BadRequest("Owner access cannot be changed here.".into()));
        }
        (Method::PATCH, acl_url(&calendar, Some(&rule.id)), json!({ "role": grant.role }))
    } else {
        list_url.query_pairs_mut().append_pair("sendNotifications", "true");
        (Method::POST, list_url, json!({ "role": grant.role, "scope": { "type": "user", "value": email } }))
    };
    Ok(Json(google_send(&http, method, url.as_str(), &token, Some(&body)).await?))
}

pub async fn revoke_calendar_access(
    State(state): State<Arc<AppState>>,
    Path((account_id, calendar_id, rule_id)): Path<(i64, i64, String)>,
) -> Result<Json<Value>, AppError> {
    let (token, calendar) = google_acl_target(&state, account_id, calendar_id).await?;
    let http = reqwest::Client::new();
    let url = acl_url(&calendar, Some(&rule_id));
    let rule: GoogleAclRule = google_get(&http, url.as_str(), &token).await?;
    if rule.role == "owner" || rule.scope.scope_type != "user" {
        return Err(AppError::BadRequest("This calendar rule cannot be removed here.".into()));
    }
    google_delete(&http, url.as_str(), &token).await?;
    Ok(Json(json!({ "ok": true })))
}

// ─────────────────────────── Contacts (People) ───────────────────────────

#[derive(Deserialize, Default)]
struct Person {
    #[serde(default, rename = "resourceName")]
    resource_name: String,
    #[serde(default)]
    etag: Option<String>,
    #[serde(default)]
    names: Vec<PName>,
    #[serde(default, rename = "emailAddresses")]
    emails: Vec<PVal>,
    #[serde(default, rename = "phoneNumbers")]
    phones: Vec<PVal>,
    #[serde(default)]
    organizations: Vec<POrg>,
    #[serde(default)]
    addresses: Vec<PAddr>,
    #[serde(default)]
    biographies: Vec<PBio>,
    #[serde(default)]
    urls: Vec<PVal>,
    #[serde(default)]
    nicknames: Vec<PNick>,
}
#[derive(Deserialize, Default)]
struct PName {
    #[serde(default, rename = "givenName")]
    given_name: Option<String>,
    #[serde(default, rename = "familyName")]
    family_name: Option<String>,
    #[serde(default, rename = "middleName")]
    middle_name: Option<String>,
    #[serde(default, rename = "displayName")]
    display_name: Option<String>,
}
#[derive(Deserialize, Default)]
struct PVal {
    #[serde(default)]
    value: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
}
#[derive(Deserialize, Default)]
struct POrg {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    department: Option<String>,
}
#[derive(Deserialize, Default)]
struct PAddr {
    #[serde(default, rename = "streetAddress")]
    street_address: Option<String>,
    #[serde(default)]
    city: Option<String>,
    #[serde(default)]
    region: Option<String>,
    #[serde(default, rename = "postalCode")]
    postal_code: Option<String>,
    #[serde(default)]
    country: Option<String>,
    #[serde(default, rename = "poBox")]
    po_box: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
}
#[derive(Deserialize, Default)]
struct PBio {
    #[serde(default)]
    value: Option<String>,
}
#[derive(Deserialize, Default)]
struct PNick {
    #[serde(default)]
    value: Option<String>,
}

fn map_person(p: &Person) -> con::ContactCard {
    let mut card = con::ContactCard {
        uid: p.resource_name.clone(),
        ..Default::default()
    };
    if let Some(n) = p.names.first() {
        card.name.first = n.given_name.clone().unwrap_or_default();
        card.name.middle = n.middle_name.clone().unwrap_or_default();
        card.name.last = n.family_name.clone().unwrap_or_default();
        card.display_name = n.display_name.clone().unwrap_or_default();
    }
    if let Some(nick) = p.nicknames.first() {
        card.name.nickname = nick.value.clone().unwrap_or_default();
    }
    card.emails = p.emails.iter().filter_map(|e| e.value.clone().map(|value| con::LabeledValue { label: label_of(&e.kind), value })).collect();
    card.phones = p.phones.iter().filter_map(|e| e.value.clone().map(|value| con::LabeledValue { label: label_of(&e.kind), value })).collect();
    card.websites = p.urls.iter().filter_map(|e| e.value.clone().map(|value| con::LabeledValue { label: label_of(&e.kind), value })).collect();
    card.addresses = p.addresses.iter().map(|a| con::ContactAddress {
        label: label_of(&a.kind),
        po_box: a.po_box.clone().unwrap_or_default(),
        street: a.street_address.clone().unwrap_or_default(),
        city: a.city.clone().unwrap_or_default(),
        state: a.region.clone().unwrap_or_default(),
        postal_code: a.postal_code.clone().unwrap_or_default(),
        country: a.country.clone().unwrap_or_default(),
    }).collect();
    if let Some(o) = p.organizations.first() {
        card.organization.company = o.name.clone().unwrap_or_default();
        card.organization.job_title = o.title.clone().unwrap_or_default();
        card.organization.department = o.department.clone().unwrap_or_default();
    }
    if let Some(b) = p.biographies.first() {
        card.personal.notes = b.value.clone().unwrap_or_default();
    }
    card
}

const PERSON_FIELDS: &str = "names,emailAddresses,phoneNumbers,addresses,organizations,biographies,urls,nicknames";

fn person_to_google(card: &con::ContactCard, etag: Option<&str>) -> Value {
    let mut obj = json!({
        "names": [{ "givenName": card.name.first, "familyName": card.name.last, "middleName": card.name.middle }],
    });
    if let Some(e) = etag {
        obj["etag"] = json!(e);
    }
    if !card.name.nickname.trim().is_empty() {
        obj["nicknames"] = json!([{ "value": card.name.nickname }]);
    }
    let map = |list: &Vec<con::LabeledValue>| -> Vec<Value> {
        list.iter().filter(|x| !x.value.trim().is_empty()).map(|x| json!({ "value": x.value, "type": x.label })).collect()
    };
    let emails = map(&card.emails);
    if !emails.is_empty() { obj["emailAddresses"] = json!(emails); }
    let phones = map(&card.phones);
    if !phones.is_empty() { obj["phoneNumbers"] = json!(phones); }
    let urls = map(&card.websites);
    if !urls.is_empty() { obj["urls"] = json!(urls); }
    let addrs: Vec<Value> = card.addresses.iter().map(|a| json!({
        "streetAddress": a.street, "city": a.city, "region": a.state,
        "postalCode": a.postal_code, "country": a.country, "type": a.label,
    })).collect();
    if !addrs.is_empty() { obj["addresses"] = json!(addrs); }
    let org = &card.organization;
    if !(org.company.trim().is_empty() && org.job_title.trim().is_empty() && org.department.trim().is_empty()) {
        obj["organizations"] = json!([{ "name": org.company, "title": org.job_title, "department": org.department }]);
    }
    if !card.personal.notes.trim().is_empty() {
        obj["biographies"] = json!([{ "value": card.personal.notes, "contentType": "TEXT_PLAIN" }]);
    }
    obj
}

pub async fn sync_contacts(
    State(state): State<Arc<AppState>>,
    Path(account_id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let lock = state.workspace_sync_lock(account_id, "contacts").await;
    let _guard = lock.lock().await;
    let token = gmail_token(&state, account_id).await?;
    let pool = db::get_user_db_pool(&state, account_id).await?;
    let http = reqwest::Client::new();

    // 1. Fetch all remote connections.
    let remote = google_pages::<Person>(
        &http, &token, PEOPLE_URL, &[("personFields", PERSON_FIELDS), ("pageSize", "1000")], 25,
    ).await?;

    // 2. Load local contacts.
    let rows = sqlx::query(
        "SELECT contact_id, card_json, uid, remote_id, etag, dirty, deleted, remote_updated_ms, local_updated_ms FROM contacts",
    )
    .fetch_all(&pool)
    .await?;
    let locals: Vec<LocalRow> = rows
        .iter()
        .map(|r| LocalRow {
            id: r.try_get("contact_id").unwrap_or_default(),
            remote_id: r.try_get("remote_id").ok().flatten(),
            uid: r.try_get::<Option<String>, _>("uid").ok().flatten().unwrap_or_default(),
            etag: r.try_get("etag").ok().flatten(),
            state: parse_state(r),
            card_json: r.try_get::<Option<String>, _>("card_json").ok().flatten().unwrap_or_default(),
            aux: 0,
            scope_ms: 0,
            recurs: false,
        })
        .collect();
    let mut by_remote: HashMap<String, usize> = HashMap::new();
    let mut by_uid: HashMap<String, usize> = HashMap::new();
    for (i, l) in locals.iter().enumerate() {
        if let Some(rid) = l.remote_id.as_deref().filter(|s| !s.is_empty()) {
            by_remote.insert(rid.to_string(), i);
        }
        if !l.uid.is_empty() {
            by_uid.insert(l.uid.clone(), i);
        }
    }

    let mut seen: HashSet<usize> = HashSet::new();
    let (mut pulled, mut pushed, mut deleted_remote) = (0i64, 0i64, 0i64);

    // 3. Reconcile remote → local (contacts use etag-change as the remote signal).
    for p in &remote {
        let card = map_person(p);
        let idx = by_remote.get(&p.resource_name).or_else(|| by_uid.get(&p.resource_name)).copied();
        let remote_ms = rfc3339_to_ms(""); // People has no simple updated ts; etag drives change.
        match idx {
            None => {
                con::sync_write_contact(&pool, None, &card, &p.resource_name, p.etag.as_deref(), remote_ms).await?;
                pulled += 1;
            }
            Some(i) => {
                seen.insert(i);
                let local = &locals[i];
                if local.state.deleted {
                    let url = format!("https://people.googleapis.com/v1/{}:deleteContact", p.resource_name);
                    google_delete(&http, &url, &token).await?;
                    cal::sync_hard_delete(&pool, "contacts", "contact_id", local.id).await?;
                    deleted_remote += 1;
                } else if local.state.dirty {
                    // Local wins; update remote using the freshly-fetched etag.
                    if let Ok(lcard) = serde_json::from_str::<con::ContactCard>(&local.card_json) {
                        let body = person_to_google(&lcard, p.etag.as_deref());
                        let url = format!("https://people.googleapis.com/v1/{}:updateContact?updatePersonFields={}&personFields={}", p.resource_name, PERSON_FIELDS, PERSON_FIELDS);
                        let resp: Value = google_send(&http, Method::PATCH, &url, &token, Some(&body)).await?;
                        cal::sync_mark_pushed(&pool, "contacts", "contact_id", local.id, &p.resource_name, resp.get("etag").and_then(|x| x.as_str()), remote_ms).await?;
                        pushed += 1;
                    }
                } else if local.etag.as_deref() != p.etag.as_deref() {
                    // Remote changed and we have no local edits → pull.
                    con::sync_write_contact(&pool, Some(local.id), &card, &p.resource_name, p.etag.as_deref(), remote_ms).await?;
                    pulled += 1;
                }
            }
        }
    }

    // 4. Orphans.
    for (i, local) in locals.iter().enumerate() {
        if seen.contains(&i) {
            continue;
        }
        match reconcile_orphan(local.state, true) {
            OrphanAction::Noop => {}
            OrphanAction::DropLocal | OrphanAction::DeleteLocal => {
                cal::sync_hard_delete(&pool, "contacts", "contact_id", local.id).await?;
            }
            OrphanAction::CreateRemote | OrphanAction::RecreateRemote => {
                if let Ok(lcard) = serde_json::from_str::<con::ContactCard>(&local.card_json) {
                    let body = person_to_google(&lcard, None);
                    let url = format!("https://people.googleapis.com/v1/people:createContact?personFields={PERSON_FIELDS}");
                    let resp: Value = google_send(&http, Method::POST, &url, &token, Some(&body)).await?;
                    let rid = v_str(&resp, "resourceName");
                    if !rid.is_empty() {
                        cal::sync_mark_pushed(&pool, "contacts", "contact_id", local.id, &rid, resp.get("etag").and_then(|x| x.as_str()), 0).await?;
                        pushed += 1;
                    }
                }
            }
        }
    }

    Ok(Json(json!({ "pulled": pulled, "pushed": pushed, "deletedRemote": deleted_remote })))
}

// ─────────────────────────── Tasks ───────────────────────────

#[derive(Deserialize, Default)]
struct GTaskList {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
}
#[derive(Deserialize, Default)]
struct GTask {
    #[serde(default)]
    id: String,
    #[serde(default)]
    etag: Option<String>,
    #[serde(default)]
    updated: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    due: Option<String>,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    position: Option<String>,
}

const TASK_NOTE_METADATA: &str = "\n\n[Guvercin task metadata v1]\n";

#[derive(Deserialize, Serialize)]
struct TaskNoteMetadata {
    priority: String,
    starred: bool,
    subtasks: Vec<todo::Subtask>,
    due_time: Option<String>,
}

fn map_task(g: &GTask, list_id: i64) -> Option<todo::TaskCard> {
    let title = g.title.clone().unwrap_or_default();
    if title.trim().is_empty() {
        return None;
    }
    let mut card = todo::TaskCard {
        uid: format!("gtasks-{}", g.id),
        list_id: Some(list_id),
        title,
        notes: g.notes.clone().unwrap_or_default(),
        completed: g.status.as_deref() == Some("completed"),
        ..Default::default()
    };
    if let Some(due) = &g.due {
        if let Some(date) = due.get(..10) {
            card.due = date.to_owned();
        }
    }
    if let Some((notes, metadata)) = card.notes.rsplit_once(TASK_NOTE_METADATA) {
        if let Ok(metadata) = serde_json::from_str::<TaskNoteMetadata>(metadata) {
            card.notes = notes.to_owned();
            card.priority = metadata.priority;
            card.starred = metadata.starred;
            card.subtasks = metadata.subtasks;
            if let Some(time) = metadata.due_time.filter(|time| chrono::NaiveTime::parse_from_str(time, "%H:%M").is_ok()) {
                if !card.due.is_empty() {
                    card.due.push('T');
                    card.due.push_str(&time);
                    card.has_due_time = true;
                }
            }
        }
    }
    Some(card)
}

fn task_to_google(card: &todo::TaskCard) -> Result<Value, AppError> {
    let pending_subtasks = if has_native_tasks(&card.subtasks) { Vec::new() } else { card.subtasks.clone() };
    let due_time = if card.has_due_time { card.due.get(11..16).map(str::to_owned) } else { None };
    let mut notes = card.notes.clone();
    if !matches!(card.priority.as_str(), "" | "none") || card.starred || !pending_subtasks.is_empty() || due_time.is_some() {
        let metadata = TaskNoteMetadata {
            priority: card.priority.clone(),
            starred: card.starred,
            subtasks: pending_subtasks,
            due_time,
        };
        notes.push_str(TASK_NOTE_METADATA);
        notes.push_str(&serde_json::to_string(&metadata)
            .map_err(|error| AppError::BadRequest(error.to_string()))?);
    }
    if notes.encode_utf16().count() > 8192 {
        return Err(AppError::BadRequest("Task notes and Guvercin metadata exceed the Google Tasks limit.".into()));
    }
    let mut obj = json!({
        "title": card.title,
        "notes": notes,
        "status": if card.completed { "completed" } else { "needsAction" },
    });
    let date = card.due.get(0..10).unwrap_or("");
    if !date.is_empty() {
        obj["due"] = json!(format!("{date}T00:00:00.000Z"));
    }
    Ok(obj)
}

fn tasks_url(tasklist: &str, task: Option<&str>) -> String {
    let mut url = Url::parse("https://tasks.googleapis.com/").unwrap();
    {
        let mut segs = url.path_segments_mut().unwrap();
        segs.extend(&["tasks", "v1", "lists", tasklist, "tasks"]);
        if let Some(t) = task {
            segs.push(t);
        }
    }
    url.to_string()
}

async fn target_tasklist(pool: &SqlitePool, list_id: i64) -> String {
    sqlx::query_scalar::<_, Option<String>>("SELECT remote_id FROM task_lists WHERE list_id = ?")
        .bind(list_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .flatten()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "@default".to_string())
}

fn native_children(parent: &str, tasks: &[GTask], depth: usize) -> (Vec<todo::Subtask>, i64) {
    if depth >= 20 {
        return (Vec::new(), 0);
    }
    let mut children: Vec<&GTask> = tasks.iter().filter(|task| task.parent.as_deref() == Some(parent)).collect();
    children.sort_by(|a, b| a.position.cmp(&b.position));
    let mut updated = 0;
    let cards = children.into_iter().map(|child| {
        let (children, latest) = native_children(&child.id, tasks, depth + 1);
        updated = updated.max(latest).max(child.updated.as_deref().map(rfc3339_to_ms).unwrap_or(0));
        todo::Subtask {
            title: child.title.clone().unwrap_or_default(),
            done: child.status.as_deref() == Some("completed"),
            children,
            google_id: Some(child.id.clone()),
        }
    }).collect();
    (cards, updated)
}

fn merge_legacy_children(native: &mut Vec<todo::Subtask>, legacy: Vec<todo::Subtask>) {
    let mut used = HashSet::new();
    for entry in legacy {
        let matching = native.iter().enumerate().position(|(index, subtask)| {
            !used.contains(&index) && (entry.google_id.as_ref().is_some_and(|id| subtask.google_id.as_ref() == Some(id))
                || (entry.google_id.is_none() && subtask.title == entry.title && subtask.done == entry.done))
        });
        if let Some(index) = matching {
            used.insert(index);
            merge_legacy_children(&mut native[index].children, entry.children);
        } else if entry.google_id.is_none() {
            let _ = used.insert(native.len());
            native.push(entry);
        }
    }
}

fn collect_child_ids(children: &[todo::Subtask], ids: &mut HashSet<String>) {
    for child in children {
        if let Some(id) = &child.google_id {
            ids.insert(id.clone());
        }
        collect_child_ids(&child.children, ids);
    }
}

struct GoogleChildSync<'a> {
    http: &'a reqwest::Client,
    token: &'a str,
    list: &'a str,
    remote: &'a [GTask],
}

impl GoogleChildSync<'_> {
    fn push<'a>(
        &'a self, parent: &'a str, children: &'a mut [todo::Subtask], depth: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), AppError>> + Send + 'a>> {
        Box::pin(async move {
            if depth >= 20 {
                return Err(AppError::BadRequest("Google task nesting is too deep".into()));
            }
            let mut siblings: Vec<&GTask> = self.remote.iter()
                .filter(|task| task.parent.as_deref() == Some(parent)).collect();
            siblings.sort_by(|a, b| a.position.cmp(&b.position));
            let mut order: Vec<_> = siblings.iter().map(|task| task.id.clone()).collect();
            let mut previous: Option<String> = None;
            let mut handled = HashSet::new();
            for (position, child) in children.iter_mut().enumerate() {
                if child.title.trim().is_empty() {
                    continue;
                }
                use sha2::{Digest, Sha256};
                let marker = format!("[Guvercin subtask {}]", hex::encode(Sha256::digest(
                    format!("{parent}:{position}:{}", child.title).as_bytes()
                )));
                let candidate = siblings.iter().copied().find(|remote| !handled.contains(&remote.id)
                    && (child.google_id.as_deref() == Some(remote.id.as_str()) || remote.notes.as_deref() == Some(&marker)));
                let mut payload = json!({
                    "title": child.title,
                    "status": if child.done { "completed" } else { "needsAction" },
                });
                let id = if let Some(existing) = candidate {
                    let url = tasks_url(self.list, Some(&existing.id));
                    if existing.title.as_deref() != Some(child.title.as_str())
                        || (existing.status.as_deref() == Some("completed")) != child.done
                    {
                        let _: Value = google_send(self.http, Method::PATCH, &url, self.token, Some(&payload)).await?;
                    }
                    let existing_position = order.iter().position(|id| id == &existing.id).unwrap_or_default();
                    let predecessor = existing_position.checked_sub(1).map(|index| order[index].as_str());
                    if predecessor != previous.as_deref() {
                        let mut url = Url::parse(&url).expect("Google task URL is valid");
                        url.path_segments_mut().expect("Google task URL is hierarchical").push("move");
                        url.query_pairs_mut().append_pair("parent", parent);
                        if let Some(previous) = &previous {
                            url.query_pairs_mut().append_pair("previous", previous);
                        }
                        let _: Value = google_send(self.http, Method::POST, url.as_str(), self.token, None).await?;
                    }
                    existing.id.clone()
                } else {
                    let mut url = Url::parse(&tasks_url(self.list, None)).expect("Google task URL is valid");
                    url.query_pairs_mut().append_pair("parent", parent);
                    if let Some(previous) = &previous {
                        url.query_pairs_mut().append_pair("previous", previous);
                    }
                    payload["notes"] = json!(marker);
                    let response: GTask = google_send(self.http, Method::POST, url.as_str(), self.token, Some(&payload)).await?;
                    if response.id.is_empty() {
                        return Err(AppError::BadRequest("Google did not return a subtask identifier".into()));
                    }
                    response.id
                };
                order.retain(|entry| entry != &id);
                let next = previous.as_deref().and_then(|previous|
                    order.iter().position(|entry| entry == previous).map(|index| index + 1)).unwrap_or(0);
                order.insert(next, id.clone());
                child.google_id = Some(id.clone());
                handled.insert(id.clone());
                if !child.children.is_empty() {
                    self.push(&id, &mut child.children, depth + 1).await?;
                }
                previous = Some(id);
            }
            Ok(())
        })
    }
}

async fn task_child_ids(pool: &SqlitePool, list: &str, root: &str) -> Result<HashSet<String>, AppError> {
    let saved: Option<String> = sqlx::query_scalar(
        "SELECT children FROM google_task_subtasks WHERE list_id = ? AND task_id = ?",
    ).bind(list).bind(root).fetch_optional(pool).await?;
    Ok(saved.and_then(|raw| serde_json::from_str(&raw).ok()).unwrap_or_default())
}

async fn remember_task_children(pool: &SqlitePool, list: &str, root: &str, children: &[todo::Subtask]) -> Result<(), AppError> {
    let mut ids = HashSet::new();
    collect_child_ids(children, &mut ids);
    sqlx::query("INSERT OR REPLACE INTO google_task_subtasks (list_id, task_id, children) VALUES (?, ?, ?)")
        .bind(list).bind(root).bind(serde_json::to_string(&ids).expect("string set is serializable"))
        .execute(pool).await?;
    Ok(())
}

fn has_native_tasks(children: &[todo::Subtask]) -> bool {
    children.iter().all(|child| child.google_id.is_some() && has_native_tasks(&child.children))
}

pub async fn sync_tasks(
    State(state): State<Arc<AppState>>,
    Path(account_id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let lock = state.workspace_sync_lock(account_id, "tasks").await;
    let _guard = lock.lock().await;
    let token = gmail_token(&state, account_id).await?;
    let pool = db::get_user_db_pool(&state, account_id).await?;
    let http = reqwest::Client::new();

    // 1. Mirror remote task lists → local, recording remote ids.
    let lists = google_pages::<GTaskList>(&http, &token, TASKLISTS_URL, &[], 20).await?;
    let mut gl_to_local: HashMap<String, i64> = HashMap::new();
    for gl in &lists {
        let title = if gl.title.trim().is_empty() { "Tasks".to_string() } else { gl.title.clone() };
        let local_list = todo::ensure_named_list(&pool, &format!("{title} (Google)")).await?;
        set_remote_id(&pool, "task_lists", "list_id", local_list, &gl.id).await;
        gl_to_local.insert(gl.id.clone(), local_list);
    }

    // 2. Fetch all remote tasks.
    let mut remote: HashMap<String, Vec<GTask>> = HashMap::new();
    for gl in gl_to_local.keys() {
        let tasks = google_pages::<GTask>(
            &http, &token, &tasks_url(gl, None),
            &[("showCompleted", "true"), ("showHidden", "true"), ("maxResults", "100")], 20,
        ).await?;
        remote.insert(gl.clone(), tasks);

    }

    // 3. Load local tasks.
    let rows = sqlx::query(
        "SELECT task_id, list_id, task_json, uid, remote_id, etag, dirty, deleted, remote_updated_ms, local_updated_ms FROM tasks",
    )
    .fetch_all(&pool)
    .await?;
    let locals: Vec<LocalRow> = rows
        .iter()
        .map(|r| LocalRow {
            id: r.try_get("task_id").unwrap_or_default(),
            remote_id: r.try_get("remote_id").ok().flatten(),
            uid: r.try_get::<Option<String>, _>("uid").ok().flatten().unwrap_or_default(),
            etag: r.try_get("etag").ok().flatten(),
            state: parse_state(r),
            card_json: r.try_get::<Option<String>, _>("task_json").ok().flatten().unwrap_or_default(),
            aux: r.try_get::<Option<i64>, _>("list_id").ok().flatten().unwrap_or(0),
            scope_ms: 0,
            recurs: false,
        })
        .collect();
    let mut by_remote: HashMap<String, usize> = HashMap::new();
    let mut by_uid: HashMap<String, usize> = HashMap::new();
    for (i, l) in locals.iter().enumerate() {
        if let Some(rid) = l.remote_id.as_deref().filter(|s| !s.is_empty()) {
            by_remote.insert(rid.to_string(), i);
        }
        if !l.uid.is_empty() {
            by_uid.insert(l.uid.clone(), i);
        }
    }

    let mut seen: HashSet<usize> = HashSet::new();
    let (mut pulled, mut pushed, mut deleted_remote) = (0i64, 0i64, 0i64);

    // Only top-level tasks have their own local rows. Children use Google's
    // parent relation and are projected into the local task tree.
    for (gl, tasks) in &remote {
        let local_list = *gl_to_local.get(gl).unwrap_or(&0);
        let child_sync = GoogleChildSync { http: &http, token: &token, list: gl, remote: tasks };
        for gt in tasks {
            let idx = by_remote.get(&gt.id).or_else(|| by_uid.get(&format!("gtasks-{}", gt.id))).copied();
            if let Some(index) = idx {
                seen.insert(index);
            }
            if gt.parent.is_some() {
                if let Some(index) = idx {
                    let legacy = &locals[index];
                    if !legacy.state.dirty {
                        cal::sync_hard_delete(&pool, "tasks", "task_id", legacy.id).await?;
                    }
                }
                continue;
            }
            let Some(mut card) = map_task(gt, local_list) else { continue };
            let (mut children, child_updated) = native_children(&gt.id, tasks, 0);
            merge_legacy_children(&mut children, std::mem::take(&mut card.subtasks));
            card.subtasks = children;
            let remote_ms = gt.updated.as_deref().map(rfc3339_to_ms).unwrap_or(0).max(child_updated);

            let Some(local) = idx.map(|i| &locals[i]) else {
                todo::sync_write_task(&pool, None, &card, &gt.id, gt.etag.as_deref(), remote_ms).await?;
                remember_task_children(&pool, gl, &gt.id, &card.subtasks).await?;
                pulled += 1;
                continue;
            };
            let local_card = serde_json::from_str::<todo::TaskCard>(&local.card_json).ok();
            let needs_child_migration = !has_native_tasks(&card.subtasks);
            match reconcile_matched(local.state, remote_ms) {
                MatchAction::DeleteRemote => {
                    google_delete(&http, &tasks_url(gl, Some(&gt.id)), &token).await?;
                    sqlx::query("DELETE FROM google_task_subtasks WHERE list_id = ? AND task_id = ?")
                        .bind(gl).bind(&gt.id).execute(&pool).await?;
                    cal::sync_hard_delete(&pool, "tasks", "task_id", local.id).await?;
                    deleted_remote += 1;
                }
                action => {
                    if action == MatchAction::UpdateRemote || needs_child_migration {
                        let mut desired = if action == MatchAction::UpdateRemote {
                            local_card.unwrap_or_else(|| card.clone())
                        } else {
                            card.clone()
                        };
                        let known = task_child_ids(&pool, gl, &gt.id).await?;
                        let mut extant = HashSet::new();
                        collect_child_ids(&card.subtasks, &mut extant);
                        child_sync.push(&gt.id, &mut desired.subtasks, 0).await?;
                        let mut retained = HashSet::new();
                        collect_child_ids(&desired.subtasks, &mut retained);
                        for id in known.difference(&retained).filter(|id| extant.contains(*id)) {
                            google_delete(&http, &tasks_url(gl, Some(id)), &token).await?;
                        }
                        let body = task_to_google(&desired)?;
                        let response: GTask = google_send(&http, Method::PATCH, &tasks_url(gl, Some(&gt.id)), &token, Some(&body)).await?;
                        let updated = response.updated.as_deref().map(rfc3339_to_ms).unwrap_or(remote_ms);
                        todo::sync_write_task(&pool, Some(local.id), &desired, &gt.id, response.etag.as_deref(), updated).await?;
                        remember_task_children(&pool, gl, &gt.id, &desired.subtasks).await?;
                        pushed += 1;
                    } else if action == MatchAction::PullOverwrite
                        || local_card.as_ref().is_some_and(|local| local.subtasks != card.subtasks)
                    {
                        todo::sync_write_task(&pool, Some(local.id), &card, &gt.id, gt.etag.as_deref(), remote_ms).await?;
                        remember_task_children(&pool, gl, &gt.id, &card.subtasks).await?;
                        pulled += 1;
                    }
                }
            }
        }
    }

    let synced_lists: HashSet<_> = gl_to_local.values().copied().collect();

    // 5. Orphans.
    for (i, local) in locals.iter().enumerate() {
        if seen.contains(&i) {
            continue;
        }
        match reconcile_orphan(local.state, synced_lists.contains(&local.aux)) {
            OrphanAction::Noop => {}
            OrphanAction::DropLocal | OrphanAction::DeleteLocal => {
                cal::sync_hard_delete(&pool, "tasks", "task_id", local.id).await?;
            }
            OrphanAction::CreateRemote | OrphanAction::RecreateRemote => {
                if let Ok(lcard) = serde_json::from_str::<todo::TaskCard>(&local.card_json) {
                    let tasklist = target_tasklist(&pool, local.aux).await;
                    let body = task_to_google(&lcard)?;
                    let resp: Value = google_send(&http, Method::POST, &tasks_url(&tasklist, None), &token, Some(&body)).await?;
                    let id = v_str(&resp, "id");
                    let ms = rfc3339_to_ms(&v_str(&resp, "updated"));
                    if !id.is_empty() {
                        cal::sync_mark_pushed(&pool, "tasks", "task_id", local.id, &id, resp.get("etag").and_then(|x| x.as_str()), ms).await?;
                        pushed += 1;
                    }
                }
            }
        }
    }

    Ok(Json(json!({ "pulled": pulled, "pushed": pushed, "deletedRemote": deleted_remote })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::Query, routing::get, Router};

    #[test]
    fn native_google_subtasks_keep_their_identity_and_sibling_order() {
        let tasks: Vec<GTask> = serde_json::from_value(json!([
            {"id": "third", "parent": "root", "position": "003", "title": "Third"},
            {"id": "first", "parent": "root", "position": "001", "title": "First", "status": "completed"},
            {"id": "nested", "parent": "first", "title": "Nested"},
            {"id": "other", "parent": "unrelated", "title": "Other"},
        ])).unwrap();
        let (mut children, _) = native_children("root", &tasks, 0);
        assert_eq!(children.iter().map(|child| child.title.as_str()).collect::<Vec<_>>(), ["First", "Third"]);
        assert!(children[0].done);
        assert_eq!(children[0].children[0].google_id.as_deref(), Some("nested"));
        merge_legacy_children(&mut children, vec![todo::Subtask { title: "First".into(), done: true, ..Default::default() }]);
        assert_eq!(children.len(), 2);
        let card = todo::TaskCard { title: "Root".into(), subtasks: children, ..Default::default() };
        assert_eq!(task_to_google(&card).unwrap()["notes"], "");
    }

    #[test]
    fn google_task_notes_round_trip_guvercin_fields() {
        let local = todo::TaskCard {
            title: "Review".into(),
            notes: "Keep the user's notes".into(),
            due: "2026-11-29T16:45".into(),
            has_due_time: true,
            priority: "high".into(),
            starred: true,
            subtasks: vec![todo::Subtask { title: "Read".into(), ..Default::default() }],
            ..Default::default()
        };
        let body = task_to_google(&local).unwrap();
        let remote: GTask = serde_json::from_value(body).unwrap();
        let restored = map_task(&remote, 1).unwrap();
        assert_eq!(restored.notes, local.notes);
        assert_eq!(restored.due, local.due);
        assert_eq!(restored.priority, local.priority);
        assert!(restored.starred && restored.has_due_time);
        assert_eq!(restored.subtasks[0].title, "Read");
    }

    #[tokio::test]
    async fn incomplete_google_pages_never_return_a_partial_snapshot() {
        let app = Router::new().route(
            "/fixture",
            get(|Query(params): Query<HashMap<String, String>>| async move {
                assert_eq!(params.get("pageSize").map(String::as_str), Some("2"));
                match params.get("pageToken").map(String::as_str) {
                    None => Json(json!({ "connections": ["first"], "nextPageToken": "second+/=" })),
                    Some("second+/=") => Json(json!({ "connections": ["last"] })),
                    _ => panic!("Unexpected page token"),
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/fixture", listener.local_addr().unwrap());
        let server = tokio::spawn(async { axum::serve(listener, app).await });
        let client = reqwest::Client::new();
        let parameters = [("pageSize", "2")];

        let incomplete = google_pages::<String>(&client, "fixture", &endpoint, &parameters, 1).await;
        assert!(incomplete.is_err());
        let complete = google_pages::<String>(&client, "fixture", &endpoint, &parameters, 2)
            .await
            .unwrap();
        assert_eq!(complete, ["first", "last"]);
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}
