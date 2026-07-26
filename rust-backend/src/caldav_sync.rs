//! Two-way sync between the local calendar store and a CalDAV server (RFC 4791).
//!
//! This mirrors [`crate::google_sync`] but speaks CalDAV instead of the Calendar v3
//! REST API. Connection details (a discovery/base URL, username and password — the
//! last usually an app-specific password) are stored on the account row and entered
//! through a dedicated form; there is no OAuth. The transport, the discovery chain
//! (`current-user-principal` → `calendar-home-set` → the calendar collections) and
//! the XML reader live in [`crate::dav`], shared with the CardDAV contacts sync and
//! the CalDAV task sync. The same credentials serve
//! [`crate::caldav_tasks`], which syncs VTODOs from the same server.
//!
//! CalDAV has no simple per-item `updated` timestamp, so change detection is
//! ETag-based: the reconcile mirrors the contacts (People) path in `google_sync`
//! rather than the timestamp-driven [`crate::sync_reconcile::reconcile_matched`].
//! Events are exchanged as iCalendar, reusing the calendar module's ICS
//! builder/parser so CalDAV round-trips exactly like Import/Export. Deletes stay
//! conservative and are scoped to CalDAV-mirrored calendars so a second backend
//! (e.g. Google) configured on the same account is never touched.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    Json,
};
use chrono::Duration;
use reqwest::Url;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::{Row, SqlitePool};

use crate::{
    calendar_routes as cal, db, db::AppState,
    dav::{self, DavConfig, DavKind},
    error::AppError,
    sync_reconcile::{reconcile_orphan, LocalState, OrphanAction},
};

const DAY_MS: i64 = 86_400_000;

// ─────────────────────────── config storage ───────────────────────────

/// Load a complete CalDAV config for the account, or `None` if not fully set up.
/// Shared with [`crate::caldav_tasks`] — one CalDAV account serves both stores.
pub async fn load_config(
    state: &Arc<AppState>,
    account_id: i64,
) -> Result<Option<DavConfig>, AppError> {
    let general = state.ensure_ready(false).await?.general_pool.clone();
    let row = sqlx::query(
        "SELECT caldav_url, caldav_username, caldav_password FROM accounts WHERE account_id = ?",
    )
    .bind(account_id)
    .fetch_optional(&general)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let url: String = row.try_get::<Option<String>, _>("caldav_url").ok().flatten().unwrap_or_default();
    let username: String = row.try_get::<Option<String>, _>("caldav_username").ok().flatten().unwrap_or_default();
    let password: String = row.try_get::<Option<String>, _>("caldav_password").ok().flatten().unwrap_or_default();
    if url.trim().is_empty() || username.trim().is_empty() || password.is_empty() {
        return Ok(None);
    }
    Ok(Some(DavConfig {
        kind: DavKind::CalDav,
        url: url.trim().to_string(),
        username,
        password,
    }))
}

pub async fn caldav_status(
    State(state): State<Arc<AppState>>,
    Path(account_id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let general = state.ensure_ready(false).await?.general_pool.clone();
    let row = sqlx::query(
        "SELECT caldav_url, caldav_username FROM accounts WHERE account_id = ?",
    )
    .bind(account_id)
    .fetch_optional(&general)
    .await?;
    let cfg = load_config(&state, account_id).await?;
    let (url, username) = row
        .map(|r| {
            (
                r.try_get::<Option<String>, _>("caldav_url").ok().flatten().unwrap_or_default(),
                r.try_get::<Option<String>, _>("caldav_username").ok().flatten().unwrap_or_default(),
            )
        })
        .unwrap_or_default();
    Ok(Json(json!({
        "configured": cfg.is_some(),
        "available": cfg.is_some(),
        "url": url,
        "username": username,
    })))
}

pub async fn caldav_get_config(
    State(state): State<Arc<AppState>>,
    Path(account_id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let general = state.ensure_ready(false).await?.general_pool.clone();
    let row = sqlx::query(
        "SELECT caldav_url, caldav_username, caldav_password, email_address FROM accounts WHERE account_id = ?",
    )
    .bind(account_id)
    .fetch_optional(&general)
    .await?;
    let (url, username, has_password, suggested) = row
        .map(|r| {
            let pw: Option<String> = r.try_get("caldav_password").ok().flatten();
            (
                r.try_get::<Option<String>, _>("caldav_url").ok().flatten().unwrap_or_default(),
                r.try_get::<Option<String>, _>("caldav_username").ok().flatten().unwrap_or_default(),
                pw.map(|p| !p.is_empty()).unwrap_or(false),
                r.try_get::<Option<String>, _>("email_address").ok().flatten().unwrap_or_default(),
            )
        })
        .unwrap_or_default();
    Ok(Json(json!({
        "url": url,
        "username": username,
        "hasPassword": has_password,
        // Pre-fill hint for an empty form: most servers use the email address.
        "suggestedUsername": suggested,
    })))
}

#[derive(Deserialize, Default)]
pub struct ConfigBody {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
}

/// Save (or clear) the CalDAV config. A non-empty URL is validated by running
/// discovery with the resolved credentials before it is persisted, so the form
/// gets immediate feedback; an empty URL disconnects and clears all three fields.
pub async fn caldav_put_config(
    State(state): State<Arc<AppState>>,
    Path(account_id): Path<i64>,
    Json(body): Json<ConfigBody>,
) -> Result<Json<Value>, AppError> {
    let general = state.ensure_ready(false).await?.general_pool.clone();
    let url = body.url.unwrap_or_default().trim().to_string();
    let username = body.username.unwrap_or_default().trim().to_string();

    if url.is_empty() {
        sqlx::query(
            "UPDATE accounts SET caldav_url = NULL, caldav_username = NULL, caldav_password = NULL WHERE account_id = ?",
        )
        .bind(account_id)
        .execute(&general)
        .await?;
        return Ok(Json(json!({ "configured": false })));
    }

    // Resolve the password: use the supplied one, else keep what is stored.
    let password = match body.password {
        Some(p) if !p.is_empty() => p,
        _ => sqlx::query_scalar::<_, Option<String>>(
            "SELECT caldav_password FROM accounts WHERE account_id = ?",
        )
        .bind(account_id)
        .fetch_optional(&general)
        .await?
        .flatten()
        .unwrap_or_default(),
    };
    if username.is_empty() || password.is_empty() {
        return Err(AppError::BadRequest(
            "CalDAV needs a server URL, a username and a password.".into(),
        ));
    }

    let cfg = DavConfig {
        kind: DavKind::CalDav,
        url: url.clone(),
        username: username.clone(),
        password: password.clone(),
    };
    let http = dav::dav_client();
    let calendars = dav::discover(&http, &cfg, &dav::EVENT_CALENDARS).await?;
    // A tasks-only CalDAV account is legitimate, so accept the connection when
    // either kind of collection is there.
    let tasks = dav::discover(&http, &cfg, &dav::TASK_CALENDARS).await?;
    if calendars.is_empty() && tasks.is_empty() {
        return Err(AppError::BadRequest(
            "Connected, but no calendars were found at that URL.".into(),
        ));
    }

    sqlx::query(
        "UPDATE accounts SET caldav_url = ?, caldav_username = ?, caldav_password = ? WHERE account_id = ?",
    )
    .bind(&url)
    .bind(&username)
    .bind(&password)
    .bind(account_id)
    .execute(&general)
    .await?;

    Ok(Json(json!({
        "configured": true,
        "calendars": calendars.len() as i64,
        "taskLists": tasks.len() as i64,
    })))
}

// ─────────────────────────── events ───────────────────────────

struct RemoteEvent {
    href: String,
    etag: Option<String>,
    ics: String,
}

/// Fetch events in a calendar collection within [start, end] via calendar-query.
async fn fetch_events(
    http: &reqwest::Client,
    cfg: &DavConfig,
    collection: &Url,
    start: &str,
    end: &str,
) -> Result<Vec<RemoteEvent>, AppError> {
    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?><c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><d:getetag/><c:calendar-data/></d:prop><c:filter><c:comp-filter name="VCALENDAR"><c:comp-filter name="VEVENT"><c:time-range start="{start}" end="{end}"/></c:comp-filter></c:comp-filter></c:filter></c:calendar-query>"#
    );
    let resp = dav::dav_request(http, cfg, dav::report(), collection.as_str(), Some("1"), Some(dav::XML_CT), &[], Some(body)).await?;
    if !dav::is_dav_success(resp.status) {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for r in dav::responses(&resp.body) {
        let Some(href) = dav::elem_text(r, "href").filter(|s| !s.trim().is_empty()) else { continue };
        let ics = dav::elem_text(r, "calendar-data").unwrap_or_default();
        if !ics.to_uppercase().contains("BEGIN:VEVENT") {
            continue;
        }
        let abs = collection.join(&href).map(|u| u.to_string()).unwrap_or(href);
        out.push(RemoteEvent { href: abs, etag: dav::elem_text(r, "getetag"), ics });
    }
    Ok(out)
}

async fn put_event(
    http: &reqwest::Client,
    cfg: &DavConfig,
    href: &str,
    ics: &str,
    if_match: Option<&str>,
) -> Result<Option<String>, AppError> {
    dav::put_resource(http, cfg, href, dav::ICAL_CT, ics, if_match).await
}

// ─────────────────────────── local rows ───────────────────────────

struct LocalRow {
    id: i64,
    remote_id: Option<String>,
    uid: String,
    etag: Option<String>,
    state: LocalState,
    card_json: String,
    calendar_id: i64,
    start_ms: i64,
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

/// Build a single-VEVENT iCalendar body for `card`, guaranteeing a stable UID.
fn card_to_ics(card: &cal::EventCard) -> String {
    let mut c = card.clone();
    if c.uid.trim().is_empty() {
        c.uid = cal::new_uid();
    }
    cal::build_ics(std::slice::from_ref(&c))
}

// ─────────────────────────── sync ───────────────────────────

pub async fn sync_caldav(
    State(state): State<Arc<AppState>>,
    Path(account_id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let Some(cfg) = load_config(&state, account_id).await? else {
        return Err(AppError::BadRequest(
            "CalDAV is not configured for this account.".into(),
        ));
    };
    let pool = db::get_user_db_pool(&state, account_id).await?;
    let http = dav::dav_client();

    // Fetch window (±1 year), mirroring the Google integration.
    let now = chrono::Utc::now();
    let time_start = (now - Duration::days(365)).format("%Y%m%dT%H%M%SZ").to_string();
    let time_end = (now + Duration::days(365)).format("%Y%m%dT%H%M%SZ").to_string();
    let naive_now = chrono::Local::now().naive_local().and_utc().timestamp_millis();
    let win_from = naive_now - 365 * DAY_MS;
    let win_to = naive_now + 365 * DAY_MS;

    // 1. Discover + mirror calendars, remembering which local ids are CalDAV-backed.
    let collections = dav::discover(&http, &cfg, &dav::EVENT_CALENDARS).await?;
    let mut caldav_cal_ids: HashSet<i64> = HashSet::new();
    let mut col_by_local: HashMap<i64, Url> = HashMap::new();
    let mut remote: Vec<(i64, RemoteEvent)> = Vec::new();
    for col in &collections {
        let local_cal = cal::ensure_named_calendar(&pool, &format!("{} (CalDAV)", col.display_name), &col.color).await?;
        set_remote_id(&pool, local_cal, col.url.as_str()).await;
        caldav_cal_ids.insert(local_cal);
        col_by_local.insert(local_cal, col.url.clone());
        for ev in fetch_events(&http, &cfg, &col.url, &time_start, &time_end).await? {
            remote.push((local_cal, ev));
        }
    }

    // 2. Load local events.
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
            calendar_id: r.try_get::<Option<i64>, _>("calendar_id").ok().flatten().unwrap_or(0),
            start_ms: r.try_get::<i64, _>("start_ms").unwrap_or(0),
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
            by_uid.entry(l.uid.clone()).or_insert(i);
        }
    }

    let mut seen: HashSet<usize> = HashSet::new();
    let mut pulled = 0i64;
    let mut pushed = 0i64;
    let mut deleted_remote = 0i64;

    // 3. Reconcile each remote event against local (ETag drives change detection).
    for (local_cal, ev) in &remote {
        let cards = cal::parse_ics(&ev.ics);
        let Some(mut card) = cards.into_iter().find(|c| !c.start.trim().is_empty()) else { continue };
        card.calendar_id = Some(*local_cal);
        let remote_etag = ev.etag.as_deref();

        let idx = by_remote
            .get(&ev.href)
            .or_else(|| if card.uid.is_empty() { None } else { by_uid.get(&card.uid) })
            .copied();
        match idx {
            None => {
                cal::sync_write_event(&pool, None, &card, &ev.href, remote_etag, 0).await?;
                pulled += 1;
            }
            Some(i) => {
                seen.insert(i);
                let local = &locals[i];
                if local.state.deleted {
                    dav::delete_resource(&http, &cfg, &ev.href, local.etag.as_deref()).await?;
                    cal::sync_hard_delete(&pool, "events", "event_id", local.id).await?;
                    deleted_remote += 1;
                } else if local.state.dirty {
                    // Local wins; overwrite the remote using the freshly-fetched
                    // ETag (mirrors the contacts path in `google_sync`). Using the
                    // stored ETag here would 412 on a genuine conflict and wedge
                    // every later sync.
                    if let Ok(lcard) = serde_json::from_str::<cal::EventCard>(&local.card_json) {
                        let new_etag = put_event(&http, &cfg, &ev.href, &card_to_ics(&lcard), remote_etag).await?;
                        cal::sync_mark_pushed(&pool, "events", "event_id", local.id, &ev.href, new_etag.as_deref(), 0).await?;
                        pushed += 1;
                    }
                } else if local.etag.as_deref() != remote_etag {
                    cal::sync_write_event(&pool, Some(local.id), &card, &ev.href, remote_etag, 0).await?;
                    pulled += 1;
                }
            }
        }
    }

    // 4. Orphans: local events with no matching remote item this round. Deletions
    //    are confined to CalDAV-backed calendars so a co-configured backend
    //    (e.g. Google) is never disturbed.
    for (i, local) in locals.iter().enumerate() {
        if seen.contains(&i) {
            continue;
        }
        let is_caldav = caldav_cal_ids.contains(&local.calendar_id);
        let in_scope = is_caldav && !local.recurs && local.start_ms >= win_from && local.start_ms < win_to;
        match reconcile_orphan(local.state, in_scope) {
            OrphanAction::Noop => {}
            OrphanAction::DropLocal => {
                // A tombstone we never pushed (or belongs to us): safe to drop.
                if !local.state.has_remote_id || is_caldav {
                    cal::sync_hard_delete(&pool, "events", "event_id", local.id).await?;
                }
            }
            OrphanAction::DeleteLocal => {
                if is_caldav {
                    cal::sync_hard_delete(&pool, "events", "event_id", local.id).await?;
                }
            }
            OrphanAction::CreateRemote | OrphanAction::RecreateRemote => {
                // Only push brand-new local events, or recreate events that live in
                // a CalDAV calendar; never re-home another backend's rows.
                if local.state.has_remote_id && !is_caldav {
                    continue;
                }
                if let Ok(lcard) = serde_json::from_str::<cal::EventCard>(&local.card_json) {
                    let Some(collection) = col_by_local
                        .get(&local.calendar_id)
                        .cloned()
                        .or_else(|| collections.first().map(|c| c.url.clone()))
                    else { continue };
                    let ics = card_to_ics(&lcard);
                    let uid = if lcard.uid.trim().is_empty() { cal::new_uid() } else { lcard.uid.clone() };
                    let name = dav::sanitize_name(&uid, "event");
                    let Ok(href) = collection.join(&format!("{name}.ics")) else { continue };
                    let new_etag = put_event(&http, &cfg, href.as_str(), &ics, None).await?;
                    cal::sync_mark_pushed(&pool, "events", "event_id", local.id, href.as_str(), new_etag.as_deref(), 0).await?;
                    pushed += 1;
                }
            }
        }
    }

    Ok(Json(json!({ "pulled": pulled, "pushed": pushed, "deletedRemote": deleted_remote })))
}

async fn set_remote_id(pool: &SqlitePool, calendar_id: i64, remote_id: &str) {
    let _ = sqlx::query("UPDATE calendars SET remote_id = ? WHERE calendar_id = ?")
        .bind(remote_id)
        .bind(calendar_id)
        .execute(pool)
        .await;
}
