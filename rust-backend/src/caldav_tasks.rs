//! Two-way sync between the local task store and a CalDAV server's to-do
//! collections (VTODO, RFC 5545 §3.6.2 over RFC 4791).
//!
//! Tasks ride on the same CalDAV account as the calendar — one URL, one username,
//! one password, stored by [`crate::caldav_sync`] and loaded here through
//! [`crate::caldav_sync::load_config`]. Discovery only differs in the component it
//! asks for (`VTODO` instead of `VEVENT`), so a server that keeps to-dos in their
//! own collection (Nextcloud "Tasks", iCloud Reminders) and a server that mixes
//! both in one calendar are handled the same way.
//!
//! Change detection is ETag-based, exactly like the events path: CalDAV has no
//! usable per-item `updated` timestamp. Local edits win over a changed remote (the
//! user just typed them), remote changes win when the local row is clean, and
//! deletions are conservative — confined to CalDAV-mirrored task lists so a
//! co-configured Google Tasks account is never disturbed.
//!
//! `TaskCard` carries two fields iCalendar has no home for — `starred` and
//! `subtasks` — so they travel in `X-GUVERCIN-*` properties. Other clients ignore
//! them; round-tripping through this app keeps them intact.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    Json,
};
use reqwest::Url;
use serde_json::{json, Value};
use sqlx::{Row, SqlitePool};

use crate::{
    caldav_sync, calendar_routes as cal, db, db::AppState,
    dav::{self, DavConfig},
    error::AppError,
    sync_reconcile::{reconcile_orphan, LocalState, OrphanAction},
    todo_routes as todo,
};

// ─────────────────────────── VTODO ↔ TaskCard ───────────────────────────

const STARRED_PROP: &str = "X-GUVERCIN-STARRED";
const SUBTASKS_PROP: &str = "X-GUVERCIN-SUBTASKS";

/// iCalendar PRIORITY (1 = highest … 9 = lowest, 0 = undefined) → our buckets.
fn priority_from_ics(value: &str) -> &'static str {
    match value.trim().parse::<i64>().unwrap_or(0) {
        1..=4 => "high",
        5 => "medium",
        6..=9 => "low",
        _ => "none",
    }
}

fn priority_to_ics(priority: &str) -> Option<i64> {
    match priority {
        "high" => Some(1),
        "medium" => Some(5),
        "low" => Some(9),
        _ => None,
    }
}

fn ics_date_compact(iso: &str) -> String {
    iso.chars().filter(|c| c.is_ascii_digit()).collect()
}

/// Render a task's due date as an iCalendar DUE property, or `None` when unset.
/// Times are floating local, matching the event builder in `calendar_routes`.
fn due_property(card: &todo::TaskCard) -> Option<String> {
    let due = card.due.trim();
    if due.is_empty() {
        return None;
    }
    match due.split_once('T') {
        Some((date, time)) => {
            let d = ics_date_compact(date);
            let mut t: String = time.chars().filter(|c| c.is_ascii_digit()).collect();
            while t.len() < 6 {
                t.push('0');
            }
            t.truncate(6);
            if d.len() == 8 {
                Some(format!("DUE:{d}T{t}"))
            } else {
                None
            }
        }
        None => {
            let d = ics_date_compact(due);
            if d.len() == 8 {
                Some(format!("DUE;VALUE=DATE:{d}"))
            } else {
                None
            }
        }
    }
}

/// Build a single-VTODO iCalendar body for `card`.
pub fn card_to_vtodo(card: &todo::TaskCard) -> String {
    let mut out = String::new();
    out.push_str("BEGIN:VCALENDAR\r\n");
    out.push_str("VERSION:2.0\r\n");
    out.push_str("PRODID:-//guvercin//Tasks//EN\r\n");
    out.push_str("CALSCALE:GREGORIAN\r\n");
    out.push_str("BEGIN:VTODO\r\n");

    let uid = if card.uid.trim().is_empty() { new_uid() } else { card.uid.trim().to_string() };
    cal::fold_line(&mut out, &format!("UID:{uid}"));
    cal::fold_line(&mut out, &format!("DTSTAMP:{}", cal::ics_utc_now()));
    if !card.title.trim().is_empty() {
        cal::fold_line(&mut out, &format!("SUMMARY:{}", cal::escape_text(&card.title)));
    }
    if !card.notes.trim().is_empty() {
        cal::fold_line(&mut out, &format!("DESCRIPTION:{}", cal::escape_text(&card.notes)));
    }
    if let Some(due) = due_property(card) {
        cal::fold_line(&mut out, &due);
    }
    if let Some(p) = priority_to_ics(&card.priority) {
        cal::fold_line(&mut out, &format!("PRIORITY:{p}"));
    }
    if card.completed {
        cal::fold_line(&mut out, "STATUS:COMPLETED");
        cal::fold_line(&mut out, "PERCENT-COMPLETE:100");
        cal::fold_line(&mut out, &format!("COMPLETED:{}", cal::ics_utc_now()));
    } else {
        cal::fold_line(&mut out, "STATUS:NEEDS-ACTION");
    }
    if card.starred {
        cal::fold_line(&mut out, &format!("{STARRED_PROP}:1"));
    }
    if !card.subtasks.is_empty() {
        if let Ok(json) = serde_json::to_string(&card.subtasks) {
            cal::fold_line(&mut out, &format!("{SUBTASKS_PROP}:{}", cal::escape_text(&json)));
        }
    }

    out.push_str("END:VTODO\r\n");
    out.push_str("END:VCALENDAR\r\n");
    out
}

/// Parse every VTODO in an iCalendar body into task cards.
pub fn parse_vtodos(input: &str) -> Vec<todo::TaskCard> {
    let mut out: Vec<todo::TaskCard> = Vec::new();
    let mut current: Option<todo::TaskCard> = None;
    for line in cal::ics_unfold(input) {
        let upper = line.trim().to_uppercase();
        if upper == "BEGIN:VTODO" {
            current = Some(todo::TaskCard::default());
            continue;
        }
        if upper == "END:VTODO" {
            if let Some(card) = current.take() {
                out.push(card);
            }
            continue;
        }
        let Some(card) = current.as_mut() else { continue };
        let Some(prop) = cal::parse_prop(&line) else { continue };
        let value = prop.value.trim().to_string();
        match prop.name.as_str() {
            "UID" => card.uid = value,
            "SUMMARY" => card.title = cal::unescape_text(&value),
            "DESCRIPTION" => card.notes = cal::unescape_text(&value),
            "DUE" | "DTSTART" => {
                // DTSTART is a fallback: some clients only set a start date. Never
                // let it overwrite a real DUE.
                if prop.name == "DTSTART" && !card.due.is_empty() {
                    continue;
                }
                let is_date = prop
                    .params
                    .iter()
                    .any(|(k, v)| k == "VALUE" && v.eq_ignore_ascii_case("DATE"));
                if let Some((iso, date_only)) = cal::parse_ics_datetime(&value, is_date) {
                    card.due = iso;
                    card.has_due_time = !date_only;
                }
            }
            "PRIORITY" => card.priority = priority_from_ics(&value).to_string(),
            "STATUS" => {
                let v = value.to_uppercase();
                if v == "COMPLETED" {
                    card.completed = true;
                }
            }
            "PERCENT-COMPLETE" => {
                if value.trim().parse::<i64>().unwrap_or(0) >= 100 {
                    card.completed = true;
                }
            }
            "COMPLETED" => card.completed = true,
            _ if prop.name == STARRED_PROP => {
                let v = value.to_uppercase();
                card.starred = !(v.is_empty() || v == "0" || v == "FALSE");
            }
            _ if prop.name == SUBTASKS_PROP => {
                if let Ok(subs) = serde_json::from_str::<Vec<todo::Subtask>>(&cal::unescape_text(&value)) {
                    card.subtasks = subs;
                }
            }
            _ => {}
        }
    }
    out.retain(|c| !c.title.trim().is_empty() || !c.notes.trim().is_empty());
    out
}

fn new_uid() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("guvercin-task-{}-{:08x}", chrono::Utc::now().timestamp_millis(), nanos)
}

// ─────────────────────────── remote fetch ───────────────────────────

struct RemoteTodo {
    href: String,
    etag: Option<String>,
    ics: String,
}

/// Fetch every VTODO in a collection. Unlike events there is no time window —
/// a to-do with no due date still matters, and lists stay small.
async fn fetch_todos(
    http: &reqwest::Client,
    cfg: &DavConfig,
    collection: &Url,
) -> Result<Vec<RemoteTodo>, AppError> {
    const BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?><c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><d:getetag/><c:calendar-data/></d:prop><c:filter><c:comp-filter name="VCALENDAR"><c:comp-filter name="VTODO"/></c:comp-filter></c:filter></c:calendar-query>"#;
    let resp = dav::dav_request(http, cfg, dav::report(), collection.as_str(), Some("1"), Some(dav::XML_CT), &[], Some(BODY.to_string())).await?;
    if !dav::is_dav_success(resp.status) {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for r in dav::responses(&resp.body) {
        let Some(href) = dav::elem_text(r, "href").filter(|s| !s.trim().is_empty()) else { continue };
        let ics = dav::elem_text(r, "calendar-data").unwrap_or_default();
        if !ics.to_uppercase().contains("BEGIN:VTODO") {
            continue;
        }
        let abs = collection.join(&href).map(|u| u.to_string()).unwrap_or(href);
        out.push(RemoteTodo { href: abs, etag: dav::elem_text(r, "getetag"), ics });
    }
    Ok(out)
}

// ─────────────────────────── local rows ───────────────────────────

struct LocalRow {
    id: i64,
    remote_id: Option<String>,
    uid: String,
    etag: Option<String>,
    state: LocalState,
    card_json: String,
    list_id: i64,
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

async fn set_list_remote_id(pool: &SqlitePool, list_id: i64, remote_id: &str) {
    let _ = sqlx::query("UPDATE task_lists SET remote_id = ? WHERE list_id = ?")
        .bind(remote_id)
        .bind(list_id)
        .execute(pool)
        .await;
}

// ─────────────────────────── sync ───────────────────────────

pub async fn sync_caldav_tasks(
    State(state): State<Arc<AppState>>,
    Path(account_id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let Some(cfg) = caldav_sync::load_config(&state, account_id).await? else {
        return Err(AppError::BadRequest(
            "CalDAV is not configured for this account.".into(),
        ));
    };
    let pool = db::get_user_db_pool(&state, account_id).await?;
    let http = dav::dav_client();
    Ok(Json(run_sync(&http, &cfg, &pool).await?))
}

/// One full reconcile pass. Split from the handler so it can be driven against a
/// stand-in server with a real (in-memory) task store — see the tests below.
async fn run_sync(
    http: &reqwest::Client,
    cfg: &DavConfig,
    pool: &SqlitePool,
) -> Result<Value, AppError> {
    // 1. Discover + mirror to-do collections as local task lists.
    let collections = dav::discover(http, cfg, &dav::TASK_CALENDARS).await?;
    if collections.is_empty() {
        return Err(AppError::BadRequest(
            "No task lists were found on that CalDAV server.".into(),
        ));
    }
    let mut caldav_list_ids: HashSet<i64> = HashSet::new();
    let mut col_by_local: HashMap<i64, Url> = HashMap::new();
    let mut remote: Vec<(i64, RemoteTodo)> = Vec::new();
    for col in &collections {
        let local_list = todo::ensure_named_list(pool, &format!("{} (CalDAV)", col.display_name)).await?;
        set_list_remote_id(pool, local_list, col.url.as_str()).await;
        caldav_list_ids.insert(local_list);
        col_by_local.insert(local_list, col.url.clone());
        for td in fetch_todos(http, cfg, &col.url).await? {
            remote.push((local_list, td));
        }
    }

    // 2. Load local tasks.
    let rows = sqlx::query(
        "SELECT task_id, list_id, task_json, uid, remote_id, etag, dirty, deleted, remote_updated_ms, local_updated_ms FROM tasks",
    )
    .fetch_all(pool)
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
            list_id: r.try_get::<Option<i64>, _>("list_id").ok().flatten().unwrap_or(0),
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
    let (mut pulled, mut pushed, mut deleted_remote) = (0i64, 0i64, 0i64);

    // 3. Reconcile each remote to-do against local (ETag drives change detection).
    for (local_list, td) in &remote {
        let Some(mut card) = parse_vtodos(&td.ics).into_iter().next() else { continue };
        card.list_id = Some(*local_list);
        let remote_etag = td.etag.as_deref();

        let idx = by_remote
            .get(&td.href)
            .or_else(|| if card.uid.is_empty() { None } else { by_uid.get(&card.uid) })
            .copied();
        match idx {
            None => {
                todo::sync_write_task(pool, None, &card, &td.href, remote_etag, 0).await?;
                pulled += 1;
            }
            Some(i) => {
                seen.insert(i);
                let local = &locals[i];
                if local.state.deleted {
                    dav::delete_resource(http, cfg, &td.href, local.etag.as_deref()).await?;
                    cal::sync_hard_delete(pool, "tasks", "task_id", local.id).await?;
                    deleted_remote += 1;
                } else if local.state.dirty {
                    // Local wins; overwrite using the freshly-fetched ETag so a
                    // genuine conflict can't 412 and wedge every later sync.
                    if let Ok(lcard) = serde_json::from_str::<todo::TaskCard>(&local.card_json) {
                        let new_etag = dav::put_resource(http, cfg, &td.href, dav::ICAL_CT, &card_to_vtodo(&lcard), remote_etag).await?;
                        cal::sync_mark_pushed(pool, "tasks", "task_id", local.id, &td.href, new_etag.as_deref(), 0).await?;
                        pushed += 1;
                    }
                } else if local.etag.as_deref() != remote_etag {
                    todo::sync_write_task(pool, Some(local.id), &card, &td.href, remote_etag, 0).await?;
                    pulled += 1;
                }
            }
        }
    }

    // 4. Orphans: local tasks with no matching remote item this round. Confined to
    //    CalDAV-mirrored lists so a co-configured Google Tasks account is safe.
    for (i, local) in locals.iter().enumerate() {
        if seen.contains(&i) {
            continue;
        }
        let is_caldav = caldav_list_ids.contains(&local.list_id);
        match reconcile_orphan(local.state, is_caldav) {
            OrphanAction::Noop => {}
            OrphanAction::DropLocal => {
                if !local.state.has_remote_id || is_caldav {
                    cal::sync_hard_delete(pool, "tasks", "task_id", local.id).await?;
                }
            }
            OrphanAction::DeleteLocal => {
                if is_caldav {
                    cal::sync_hard_delete(pool, "tasks", "task_id", local.id).await?;
                }
            }
            OrphanAction::CreateRemote | OrphanAction::RecreateRemote => {
                if local.state.has_remote_id && !is_caldav {
                    continue;
                }
                if let Ok(mut lcard) = serde_json::from_str::<todo::TaskCard>(&local.card_json) {
                    let Some(collection) = col_by_local
                        .get(&local.list_id)
                        .cloned()
                        .or_else(|| collections.first().map(|c| c.url.clone()))
                    else { continue };
                    if lcard.uid.trim().is_empty() {
                        lcard.uid = new_uid();
                    }
                    let name = dav::sanitize_name(&lcard.uid, "task");
                    let Ok(href) = collection.join(&format!("{name}.ics")) else { continue };
                    let new_etag = dav::put_resource(http, cfg, href.as_str(), dav::ICAL_CT, &card_to_vtodo(&lcard), None).await?;
                    cal::sync_mark_pushed(pool, "tasks", "task_id", local.id, href.as_str(), new_etag.as_deref(), 0).await?;
                    pushed += 1;
                }
            }
        }
    }

    Ok(json!({ "pulled": pulled, "pushed": pushed, "deletedRemote": deleted_remote }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card() -> todo::TaskCard {
        todo::TaskCard {
            uid: "task-1".into(),
            list_id: None,
            title: "Buy milk, and bread".into(),
            notes: "Line one\nline two".into(),
            due: "2026-07-30".into(),
            has_due_time: false,
            priority: "high".into(),
            completed: false,
            starred: true,
            subtasks: vec![
                todo::Subtask { title: "semi; comma,".into(), done: true },
                todo::Subtask { title: "second".into(), done: false },
            ],
        }
    }

    #[test]
    fn vtodo_round_trips_every_field() {
        let ics = card_to_vtodo(&card());
        let back = parse_vtodos(&ics);
        assert_eq!(back.len(), 1);
        let got = &back[0];
        assert_eq!(got.uid, "task-1");
        assert_eq!(got.title, "Buy milk, and bread");
        assert_eq!(got.notes, "Line one\nline two");
        assert_eq!(got.due, "2026-07-30");
        assert!(!got.has_due_time);
        assert_eq!(got.priority, "high");
        assert!(!got.completed);
        assert!(got.starred);
        assert_eq!(got.subtasks.len(), 2);
        assert_eq!(got.subtasks[0].title, "semi; comma,");
        assert!(got.subtasks[0].done);
    }

    #[test]
    fn timed_due_round_trips() {
        let mut c = card();
        c.due = "2026-07-30T14:05".into();
        c.has_due_time = true;
        let ics = card_to_vtodo(&c);
        assert!(ics.contains("DUE:20260730T140500"), "{ics}");
        let got = parse_vtodos(&ics).remove(0);
        assert_eq!(got.due, "2026-07-30T14:05");
        assert!(got.has_due_time);
    }

    #[test]
    fn completion_maps_both_ways() {
        let mut c = card();
        c.completed = true;
        let ics = card_to_vtodo(&c);
        assert!(ics.contains("STATUS:COMPLETED"));
        assert!(parse_vtodos(&ics)[0].completed);
        let open = card_to_vtodo(&card());
        assert!(open.contains("STATUS:NEEDS-ACTION"));
        assert!(!parse_vtodos(&open)[0].completed);
    }

    #[test]
    fn foreign_vtodo_is_understood() {
        // A minimal to-do from another client: no X- properties, priority 5,
        // percent-complete instead of STATUS.
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:abc\r\nSUMMARY:Water plants\r\nDUE;VALUE=DATE:20260801\r\nPRIORITY:5\r\nPERCENT-COMPLETE:100\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let got = parse_vtodos(ics);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].uid, "abc");
        assert_eq!(got[0].title, "Water plants");
        assert_eq!(got[0].due, "2026-08-01");
        assert_eq!(got[0].priority, "medium");
        assert!(got[0].completed);
        assert!(!got[0].starred);
        assert!(got[0].subtasks.is_empty());
    }

    #[test]
    fn dtstart_only_fills_the_due_date() {
        let ics = "BEGIN:VTODO\r\nUID:x\r\nSUMMARY:s\r\nDTSTART;VALUE=DATE:20260801\r\nEND:VTODO";
        assert_eq!(parse_vtodos(ics)[0].due, "2026-08-01");
        // A real DUE wins over DTSTART regardless of order.
        let both = "BEGIN:VTODO\r\nUID:x\r\nSUMMARY:s\r\nDUE;VALUE=DATE:20260805\r\nDTSTART;VALUE=DATE:20260801\r\nEND:VTODO";
        assert_eq!(parse_vtodos(both)[0].due, "2026-08-05");
    }

    #[test]
    fn priority_buckets_cover_the_whole_range() {
        assert_eq!(priority_from_ics("1"), "high");
        assert_eq!(priority_from_ics("4"), "high");
        assert_eq!(priority_from_ics("5"), "medium");
        assert_eq!(priority_from_ics("9"), "low");
        assert_eq!(priority_from_ics("0"), "none");
        assert_eq!(priority_from_ics(""), "none");
    }

    #[test]
    fn empty_titles_are_dropped() {
        let ics = "BEGIN:VTODO\r\nUID:x\r\nEND:VTODO";
        assert!(parse_vtodos(ics).is_empty());
    }

    // ── Full reconcile against a stand-in CalDAV server ──
    //
    // Proves the wire format a real server would see (a VTODO it stores and hands
    // back), that to-do collections become local lists, and that a completed-here
    // task lands on the server as STATUS:COMPLETED.

    use axum::{extract::Request, response::Response, routing::any, Router};
    use sqlx::sqlite::SqlitePoolOptions;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Server {
        todos: std::collections::BTreeMap<String, (String, String)>,
        puts: usize,
    }
    type Shared = Arc<Mutex<Server>>;

    const LIST: &str = "/cal/tasks/";

    fn xml(body: String) -> Response {
        Response::builder()
            .status(207)
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(axum::body::Body::from(body))
            .unwrap()
    }

    async fn mock_caldav(
        axum::extract::State(state): axum::extract::State<Shared>,
        req: Request,
    ) -> Response {
        let path = req.uri().path().to_string();
        let method = req.method().as_str().to_string();
        let body = axum::body::to_bytes(req.into_body(), 256 * 1024).await.unwrap();
        let body = String::from_utf8_lossy(&body).to_string();
        let mut srv = state.lock().unwrap();
        match (method.as_str(), path.as_str()) {
            ("PROPFIND", "/") => xml(r#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/</d:href><d:propstat><d:prop>
                <d:current-user-principal><d:href>/p/</d:href></d:current-user-principal>
                </d:prop></d:propstat></d:response></d:multistatus>"#.into()),
            ("PROPFIND", "/p/") => xml(r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:response><d:href>/p/</d:href>
                <d:propstat><d:prop><c:calendar-home-set><d:href>/cal/</d:href></c:calendar-home-set>
                </d:prop></d:propstat></d:response></d:multistatus>"#.into()),
            ("PROPFIND", "/cal/") => xml(format!(
                r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
                   <d:response><d:href>/cal/events/</d:href><d:propstat><d:prop>
                     <d:resourcetype><d:collection/><c:calendar/></d:resourcetype><d:displayname>Events</d:displayname>
                     <c:supported-calendar-component-set><c:comp name="VEVENT"/></c:supported-calendar-component-set>
                   </d:prop></d:propstat></d:response>
                   <d:response><d:href>{LIST}</d:href><d:propstat><d:prop>
                     <d:resourcetype><d:collection/><c:calendar/></d:resourcetype><d:displayname>Errands</d:displayname>
                     <c:supported-calendar-component-set><c:comp name="VTODO"/></c:supported-calendar-component-set>
                   </d:prop></d:propstat></d:response></d:multistatus>"#
            )),
            ("REPORT", p) if p == LIST => {
                let mut out = String::from(r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">"#);
                for (name, (ics, etag)) in &srv.todos {
                    out.push_str(&format!(
                        r#"<d:response><d:href>{LIST}{name}</d:href><d:propstat><d:prop><d:getetag>{etag}</d:getetag>
                           <c:calendar-data>{}</c:calendar-data></d:prop></d:propstat></d:response>"#,
                        ics.replace('&', "&amp;").replace('<', "&lt;")
                    ));
                }
                out.push_str("</d:multistatus>");
                xml(out)
            }
            ("PUT", p) if p.starts_with(LIST) => {
                srv.puts += 1;
                let etag = format!("\"p{}\"", srv.puts);
                srv.todos.insert(p.trim_start_matches(LIST).to_string(), (body, etag.clone()));
                Response::builder().status(201).header("ETag", etag).body(axum::body::Body::empty()).unwrap()
            }
            ("DELETE", p) if p.starts_with(LIST) => {
                srv.todos.remove(p.trim_start_matches(LIST));
                Response::builder().status(204).body(axum::body::Body::empty()).unwrap()
            }
            _ => Response::builder().status(404).body(axum::body::Body::empty()).unwrap(),
        }
    }

    async fn spawn(state: Shared) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().fallback(any(mock_caldav)).with_state(state);
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://127.0.0.1:{port}/")
    }

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        for stmt in [
            r#"CREATE TABLE task_lists (
                list_id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE,
                color TEXT, is_default INTEGER NOT NULL DEFAULT 0, sort_order INTEGER DEFAULT 0,
                remote_id TEXT)"#,
            r#"CREATE TABLE tasks (
                task_id INTEGER PRIMARY KEY AUTOINCREMENT, list_id INTEGER, uid TEXT,
                title TEXT, notes TEXT, due_ms INTEGER, has_due_time INTEGER DEFAULT 0,
                priority INTEGER DEFAULT 0, completed INTEGER DEFAULT 0, starred INTEGER DEFAULT 0,
                completed_at DATETIME, position INTEGER DEFAULT 0, task_json TEXT,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP, updated_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                remote_id TEXT, etag TEXT, dirty INTEGER NOT NULL DEFAULT 0,
                deleted INTEGER NOT NULL DEFAULT 0, remote_updated_ms INTEGER, local_updated_ms INTEGER)"#,
        ] {
            sqlx::query(stmt).execute(&pool).await.unwrap();
        }
        pool
    }

    #[tokio::test]
    async fn tasks_pull_into_a_mirrored_list_and_local_edits_push_back() {
        let srv: Shared = Arc::new(Mutex::new(Server::default()));
        {
            let mut s = srv.lock().unwrap();
            s.todos.insert(
                "shopping.ics".into(),
                (
                    "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:shopping\r\nSUMMARY:Buy bread\r\nDUE;VALUE=DATE:20260801\r\nPRIORITY:1\r\nSTATUS:NEEDS-ACTION\r\nEND:VTODO\r\nEND:VCALENDAR\r\n".into(),
                    "\"t1\"".into(),
                ),
            );
        }
        let base = spawn(srv.clone()).await;
        let pool = test_pool().await;
        let cfg = DavConfig { kind: dav::DavKind::CalDav, url: base.clone(), username: "u".into(), password: "p".into() };
        let http = dav::dav_client();

        // First pass: the VTODO collection becomes a local list holding the task.
        let stats = run_sync(&http, &cfg, &pool).await.unwrap();
        assert_eq!(stats["pulled"], 1, "{stats}");
        let (name, remote): (String, Option<String>) =
            sqlx::query_as("SELECT name, remote_id FROM task_lists").fetch_one(&pool).await.unwrap();
        assert_eq!(name, "Errands (CalDAV)");
        assert!(remote.unwrap().ends_with(LIST), "the list should remember its collection");
        let (title, priority, due): (String, i64, i64) =
            sqlx::query_as("SELECT title, priority, due_ms FROM tasks WHERE uid = 'shopping'")
                .fetch_one(&pool).await.unwrap();
        assert_eq!(title, "Buy bread");
        assert_eq!(priority, 3, "PRIORITY:1 is our 'high' bucket");
        assert!(due > 0);

        // Nothing changed ⇒ a second pass is a no-op (ETags match).
        let idle = run_sync(&http, &cfg, &pool).await.unwrap();
        assert_eq!((idle["pulled"].as_i64(), idle["pushed"].as_i64()), (Some(0), Some(0)), "{idle}");

        // Complete it locally, as the UI does, then sync: the server sees COMPLETED.
        sqlx::query(
            "UPDATE tasks SET completed = 1, task_json = ?, dirty = 1 WHERE uid = 'shopping'",
        )
        .bind(serde_json::to_string(&todo::TaskCard {
            uid: "shopping".into(),
            title: "Buy bread".into(),
            due: "2026-08-01".into(),
            priority: "high".into(),
            completed: true,
            ..Default::default()
        }).unwrap())
        .execute(&pool)
        .await
        .unwrap();
        let pushed = run_sync(&http, &cfg, &pool).await.unwrap();
        assert_eq!(pushed["pushed"], 1, "{pushed}");
        let stored = srv.lock().unwrap().todos["shopping.ics"].0.clone();
        assert!(stored.contains("STATUS:COMPLETED"), "{stored}");
        assert!(stored.contains("SUMMARY:Buy bread"), "{stored}");

        // A brand-new local task is created in the mirrored collection.
        sqlx::query("INSERT INTO tasks (list_id, uid, title, task_json, dirty) VALUES ((SELECT list_id FROM task_lists), 'fresh', 'Call plumber', ?, 1)")
            .bind(serde_json::to_string(&todo::TaskCard {
                uid: "fresh".into(),
                title: "Call plumber".into(),
                ..Default::default()
            }).unwrap())
            .execute(&pool)
            .await
            .unwrap();
        let created = run_sync(&http, &cfg, &pool).await.unwrap();
        assert_eq!(created["pushed"], 1, "{created}");
        let s = srv.lock().unwrap();
        assert!(s.todos.contains_key("fresh.ics"), "{:?}", s.todos.keys().collect::<Vec<_>>());
        assert!(s.todos["fresh.ics"].0.contains("SUMMARY:Call plumber"));
    }

    #[tokio::test]
    async fn a_local_tombstone_deletes_the_remote_todo() {
        let srv: Shared = Arc::new(Mutex::new(Server::default()));
        {
            let mut s = srv.lock().unwrap();
            s.todos.insert(
                "doomed.ics".into(),
                ("BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:doomed\r\nSUMMARY:Old thing\r\nEND:VTODO\r\nEND:VCALENDAR\r\n".into(), "\"t1\"".into()),
            );
        }
        let base = spawn(srv.clone()).await;
        let pool = test_pool().await;
        let cfg = DavConfig { kind: dav::DavKind::CalDav, url: base.clone(), username: "u".into(), password: "p".into() };
        let http = dav::dav_client();
        run_sync(&http, &cfg, &pool).await.unwrap();
        sqlx::query("UPDATE tasks SET deleted = 1 WHERE uid = 'doomed'").execute(&pool).await.unwrap();

        let stats = run_sync(&http, &cfg, &pool).await.unwrap();
        assert_eq!(stats["deletedRemote"], 1, "{stats}");
        assert!(srv.lock().unwrap().todos.is_empty());
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks").fetch_one(&pool).await.unwrap();
        assert_eq!(left, 0, "the tombstone row should be dropped once the remote is gone");
    }
}
