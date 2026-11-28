//! Two-way sync between the local address book and a CardDAV server (RFC 6352).
//!
//! The CalDAV sibling of this module is [`crate::caldav_sync`]; everything they
//! share — transport, redirect handling, the XML reader, the
//! `current-user-principal` → `addressbook-home-set` → collections discovery chain
//! — lives in [`crate::dav`]. Connection details (a discovery/base URL, username
//! and password, usually an app-specific one) sit on the account row in their own
//! `carddav_*` columns, because providers routinely serve contacts from a different
//! host than calendars (`contacts.icloud.com` vs `caldav.icloud.com`).
//!
//! Contacts are exchanged as vCards, reusing the address book's own
//! parser/serializer so a CardDAV round-trip behaves exactly like Import/Export.
//! Change detection is ETag-based (CardDAV has no per-item `updated` timestamp),
//! matching the contacts path in [`crate::google_sync`]: a local edit wins over a
//! changed remote, a clean local row takes the remote's copy.
//!
//! Each contact remembers its address book. Reconciliation only writes to
//! discovered collections and keeps identical UIDs in different books separate.


use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    Json,
};
use reqwest::Url;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

use crate::{
    calendar_routes as cal, contacts_routes as con, db, db::AppState,
    dav::{self, DavConfig, DavKind},
    error::AppError,
    sync_reconcile::{reconcile_orphan, LocalState, OrphanAction},
};

// ─────────────────────────── config storage ───────────────────────────

/// Load a complete CardDAV config for the account, or `None` if not fully set up.
async fn load_config(
    state: &Arc<AppState>,
    account_id: i64,
) -> Result<Option<DavConfig>, AppError> {
    let general = state.ensure_ready(false).await?.general_pool.clone();
    let row = sqlx::query(
        "SELECT carddav_url, carddav_username, carddav_password FROM accounts WHERE account_id = ?",
    )
    .bind(account_id)
    .fetch_optional(&general)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let url: String = row.try_get::<Option<String>, _>("carddav_url").ok().flatten().unwrap_or_default();
    let username: String = row.try_get::<Option<String>, _>("carddav_username").ok().flatten().unwrap_or_default();
    let password: String = row.try_get::<Option<String>, _>("carddav_password").ok().flatten().unwrap_or_default();
    if url.trim().is_empty() || username.trim().is_empty() || password.is_empty() {
        return Ok(None);
    }
    Ok(Some(DavConfig {
        kind: DavKind::CardDav,
        url: url.trim().to_string(),
        username,
        password,
    }))
}

pub async fn carddav_status(
    State(state): State<Arc<AppState>>,
    Path(account_id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let general = state.ensure_ready(false).await?.general_pool.clone();
    let row = sqlx::query("SELECT carddav_url, carddav_username FROM accounts WHERE account_id = ?")
        .bind(account_id)
        .fetch_optional(&general)
        .await?;
    let cfg = load_config(&state, account_id).await?;
    let (url, username) = row
        .map(|r| {
            (
                r.try_get::<Option<String>, _>("carddav_url").ok().flatten().unwrap_or_default(),
                r.try_get::<Option<String>, _>("carddav_username").ok().flatten().unwrap_or_default(),
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

pub async fn carddav_get_config(
    State(state): State<Arc<AppState>>,
    Path(account_id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let general = state.ensure_ready(false).await?.general_pool.clone();
    let row = sqlx::query(
        "SELECT carddav_url, carddav_username, carddav_password, caldav_username, email_address FROM accounts WHERE account_id = ?",
    )
    .bind(account_id)
    .fetch_optional(&general)
    .await?;
    let Some(row) = row else {
        return Ok(Json(json!({ "url": "", "username": "", "hasPassword": false, "suggestedUsername": "" })));
    };
    let get = |col: &str| -> String {
        row.try_get::<Option<String>, _>(col).ok().flatten().unwrap_or_default()
    };
    let password: Option<String> = row.try_get("carddav_password").ok().flatten();
    // An empty form pre-fills from whatever the user already typed for CalDAV,
    // falling back to the mail address — the same login on both, in practice.
    let caldav_username = get("caldav_username");
    let suggested = if caldav_username.trim().is_empty() { get("email_address") } else { caldav_username };
    Ok(Json(json!({
        "url": get("carddav_url"),
        "username": get("carddav_username"),
        "hasPassword": password.map(|p| !p.is_empty()).unwrap_or(false),
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

/// Save (or clear) the CardDAV config. A non-empty URL is validated by running
/// discovery with the resolved credentials before it is persisted, so the form gets
/// immediate feedback; an empty URL disconnects and clears all three fields.
pub async fn carddav_put_config(
    State(state): State<Arc<AppState>>,
    Path(account_id): Path<i64>,
    Json(body): Json<ConfigBody>,
) -> Result<Json<Value>, AppError> {
    let general = state.ensure_ready(false).await?.general_pool.clone();
    let url = body.url.unwrap_or_default().trim().to_string();
    let username = body.username.unwrap_or_default().trim().to_string();

    if url.is_empty() {
        sqlx::query(
            "UPDATE accounts SET carddav_url = NULL, carddav_username = NULL, carddav_password = NULL WHERE account_id = ?",
        )
        .bind(account_id)
        .execute(&general)
        .await?;
        return Ok(Json(json!({ "configured": false })));
    }

    let password = match body.password {
        Some(p) if !p.is_empty() => p,
        _ => sqlx::query_scalar::<_, Option<String>>(
            "SELECT carddav_password FROM accounts WHERE account_id = ?",
        )
        .bind(account_id)
        .fetch_optional(&general)
        .await?
        .flatten()
        .unwrap_or_default(),
    };
    if username.is_empty() || password.is_empty() {
        return Err(AppError::BadRequest(
            "CardDAV needs a server URL, a username and a password.".into(),
        ));
    }

    let cfg = DavConfig {
        kind: DavKind::CardDav,
        url: url.clone(),
        username: username.clone(),
        password: password.clone(),
    };
    let http = dav::dav_client();
    let books = dav::discover(&http, &cfg, &dav::ADDRESS_BOOKS).await?;
    if books.is_empty() {
        return Err(AppError::BadRequest(
            "Connected, but no address books were found at that URL.".into(),
        ));
    }

    sqlx::query(
        "UPDATE accounts SET carddav_url = ?, carddav_username = ?, carddav_password = ? WHERE account_id = ?",
    )
    .bind(&url)
    .bind(&username)
    .bind(&password)
    .bind(account_id)
    .execute(&general)
    .await?;

    Ok(Json(json!({ "configured": true, "addressBooks": books.len() as i64 })))
}

// ─────────────────────────── remote fetch ───────────────────────────

struct RemoteCard {
    href: String,
    address_book: String,
    etag: Option<String>,
    vcf: String,
}

/// Fetch every vCard in an address book via addressbook-query.
async fn fetch_cards(
    http: &reqwest::Client,
    cfg: &DavConfig,
    collection: &Url,
) -> Result<Vec<RemoteCard>, AppError> {
    const BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?><c:addressbook-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:carddav"><d:prop><d:getetag/><c:address-data/></d:prop></c:addressbook-query>"#;
    let resp = dav::dav_request(http, cfg, dav::report(), collection.as_str(), Some("1"), Some(dav::XML_CT), &[], Some(BODY.to_string())).await?;
    if dav::is_dav_success(resp.status) && !dav::has_elem(&resp.body, "number-of-matches-within-limits") {
        return collect_full_snapshot(http, cfg, collection, &resp.body).await;
    }
    // A Depth:1 PROPFIND can discover vCard hrefs on servers that reject REPORT.
    const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:carddav"><d:prop><d:getetag/><c:address-data/></d:prop></d:propfind>"#;
    let fallback = dav::dav_request(http, cfg, dav::propfind(), collection.as_str(), Some("1"), Some(dav::XML_CT), &[], Some(PROPFIND_BODY.to_string())).await?;
    if !dav::is_dav_success(fallback.status) {
        return Err(AppError::BadRequest(format!("Could not read CardDAV address book (HTTP {})", fallback.status.as_u16())));
    }
    collect_full_snapshot(http, cfg, collection, &fallback.body).await
}

fn scoped_card_href(response: &str, collection: &Url) -> Option<Url> {
    let href = dav::elem_text(response, "href")?;
    let absolute = collection.join(&href).ok()?;
    belongs_to(collection, &absolute).then_some(absolute)
}

async fn collect_full_snapshot(
    http: &reqwest::Client, cfg: &DavConfig, collection: &Url, body: &str,
) -> Result<Vec<RemoteCard>, AppError> {
    if !dav::has_elem(body, "multistatus") || dav::has_elem(body, "number-of-matches-within-limits") {
        return Err(AppError::BadRequest("CardDAV did not return a complete address book".into()));
    }
    let mut out = Vec::new();
    collect_cards(body, collection, &mut out);
    let found: HashSet<_> = out.iter().map(|card| card.href.clone()).collect();
    for response in dav::responses(body) {
        let Some(href) = scoped_card_href(response, collection) else { continue };
        if found.contains(href.as_str()) {
            continue;
        }
        let resource = dav::dav_request(http, cfg, reqwest::Method::GET, href.as_str(),
            None, None, &[], None).await?;
        if !dav::is_dav_success(resource.status) || !resource.body.to_ascii_uppercase().contains("BEGIN:VCARD") {
            return Err(AppError::BadRequest("A CardDAV contact could not be loaded".into()));
        }
        out.push(RemoteCard { href: href.to_string(), address_book: collection.to_string(),
            etag: resource.etag.or_else(|| dav::elem_text(response, "getetag")), vcf: resource.body });
    }
    Ok(out)
}

fn collect_cards(body: &str, collection: &Url, out: &mut Vec<RemoteCard>) {
    for r in dav::responses(body) {
        let Some(absolute) = scoped_card_href(r, collection) else { continue };
        let vcf = dav::elem_text(r, "address-data").unwrap_or_default();
        if vcf.to_uppercase().contains("BEGIN:VCARD") {
            out.push(RemoteCard { href: absolute.to_string(), address_book: collection.to_string(),
                etag: dav::elem_text(r, "getetag"), vcf });
        }
    }
}

fn belongs_to(collection: &Url, item: &Url) -> bool {
    item.origin() == collection.origin()
        && item.path().strip_prefix(collection.path().trim_end_matches('/'))
            .and_then(|rest| rest.strip_prefix('/')).is_some_and(|rest| !rest.is_empty() && !rest.contains('/'))
}

pub(crate) async fn init_books(pool: &sqlx::SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query("CREATE TABLE IF NOT EXISTS carddav_books (href TEXT PRIMARY KEY, name TEXT NOT NULL)")
        .execute(pool).await?;
    Ok(())
}

pub(crate) async fn require_book(pool: &sqlx::SqlitePool, book: Option<&str>) -> Result<(), AppError> {
    if let Some(book) = book {
        let available: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM carddav_books WHERE href = ?)")
            .bind(book).fetch_one(pool).await?;
        if !available {
            return Err(AppError::BadRequest("CardDAV address book is unavailable.".into()));
        }
    }
    Ok(())
}

pub async fn address_books(
    State(state): State<Arc<AppState>>,
    Path(account_id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let pool = db::get_user_db_pool(&state, account_id).await?;
    let books: Vec<(String, String)> = sqlx::query_as("SELECT href, name FROM carddav_books ORDER BY name, href")
        .fetch_all(&pool).await?;
    Ok(Json(json!(books.into_iter().map(|(href, name)| json!({ "href": href, "name": name })).collect::<Vec<_>>())))
}

// ─────────────────────────── local rows ───────────────────────────

struct LocalRow {
    id: i64,
    remote_id: Option<String>,
    uid: String,
    etag: Option<String>,
    state: LocalState,
    card_json: String,
    address_book: Option<String>,
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

/// Whether a `remote_id` was written by this module — a DAV href, as opposed to a
/// Google People resource name. Guards every destructive branch so two backends
/// configured on one account can't delete each other's rows.
fn is_dav_href(remote_id: Option<&str>) -> bool {
    remote_id
        .map(|s| s.starts_with("http://") || s.starts_with("https://"))
        .unwrap_or(false)
}

// ─────────────────────────── sync ───────────────────────────

pub async fn sync_carddav(
    State(state): State<Arc<AppState>>,
    Path(account_id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let lock = state.workspace_sync_lock(account_id, "contacts").await;
    let _guard = lock.lock().await;
    let Some(cfg) = load_config(&state, account_id).await? else {
        return Err(AppError::BadRequest(
            "CardDAV is not configured for this account.".into(),
        ));
    };
    let pool = db::get_user_db_pool(&state, account_id).await?;
    let http = dav::dav_client();
    Ok(Json(run_sync(&http, &cfg, &pool).await?))
}

/// One full reconcile pass. Split from the handler so it can be driven against a
/// stand-in server with a real (in-memory) contacts table — see the tests below.
async fn run_sync(
    http: &reqwest::Client,
    cfg: &DavConfig,
    pool: &sqlx::SqlitePool,
) -> Result<Value, AppError> {
    // 1. Discover address books and fetch everything in them.
    let books = dav::discover(http, cfg, &dav::ADDRESS_BOOKS).await?;
    if books.is_empty() {
        return Err(AppError::BadRequest(
            "No address books were found on that CardDAV server.".into(),
        ));
    }
    let mut remote: Vec<RemoteCard> = Vec::new();
    for book in &books {
        remote.extend(fetch_cards(http, cfg, &book.url).await?);
    }

    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM carddav_books").execute(&mut *tx).await?;
    for book in &books {
        let name = if book.display_name.is_empty() { book.url.path() } else { &book.display_name };
        sqlx::query("INSERT INTO carddav_books (href, name) VALUES (?, ?)")
            .bind(book.url.as_str()).bind(name).execute(&mut *tx).await?;
        sqlx::query(
            "UPDATE contacts SET card_json = json_set(card_json, '$.addressBook', ?)
             WHERE json_valid(card_json) AND json_extract(card_json, '$.addressBook') IS NULL
               AND substr(remote_id, 1, length(?)) = ?",
        ).bind(book.url.as_str()).bind(book.url.as_str()).bind(book.url.as_str()).execute(&mut *tx).await?;
    }
    tx.commit().await?;

    // 2. Load local contacts.
    let rows = sqlx::query(
        "SELECT contact_id, card_json, uid, remote_id, etag, dirty, deleted, remote_updated_ms, local_updated_ms,
         CASE WHEN json_valid(card_json) THEN json_extract(card_json, '$.addressBook') END AS address_book FROM contacts",
    )
    .fetch_all(pool)
    .await?;
    let locals: Vec<LocalRow> = rows
        .iter()
        .map(|r| LocalRow {
            id: r.try_get("contact_id").unwrap_or_default(),
            remote_id: r.try_get("remote_id").ok().flatten(),
            uid: r.try_get::<Option<String>, _>("uid").ok().flatten().unwrap_or_default(),
            etag: r.try_get("etag").ok().flatten(),
            address_book: r.try_get("address_book").ok().flatten(),
            state: parse_state(r),
            card_json: r.try_get::<Option<String>, _>("card_json").ok().flatten().unwrap_or_default(),
        })
        .collect();

    let primary = &books[0].url;
    let mut by_remote: HashMap<&str, usize> = HashMap::new();
    let mut by_uid: HashMap<(&str, &str), usize> = HashMap::new();
    for (i, l) in locals.iter().enumerate() {
        if let Some(rid) = l.remote_id.as_deref().filter(|s| !s.is_empty()) {
            by_remote.insert(rid, i);
        } else if !l.uid.is_empty() {
            let book = l.address_book.as_deref().unwrap_or_else(|| primary.as_str());
            by_uid.entry((book, l.uid.as_str())).or_insert(i);
        }
    }

    let mut seen: HashSet<usize> = HashSet::new();
    let (mut pulled, mut pushed, mut deleted_remote) = (0i64, 0i64, 0i64);

    // 3. Reconcile each remote vCard against local (ETag drives change detection).
    for rc in &remote {
        let Some(mut card) = con::parse_vcards(&rc.vcf).into_iter().next() else { continue };
        card.address_book = Some(rc.address_book.clone());
        let remote_etag = rc.etag.as_deref();
        let idx = by_remote
            .get(rc.href.as_str()).copied()
            .or_else(|| by_uid.get(&(rc.address_book.as_str(), card.uid.trim()))
                .copied().filter(|i| !seen.contains(i)));
        match idx {
            None => {
                con::sync_write_contact(pool, None, &card, &rc.href, remote_etag, 0).await?;
                pulled += 1;
            }
            Some(i) => {
                seen.insert(i);
                let local = &locals[i];
                if local.state.deleted {
                    dav::delete_resource(http, cfg, &rc.href, local.etag.as_deref()).await?;
                    cal::sync_hard_delete(pool, "contacts", "contact_id", local.id).await?;
                    deleted_remote += 1;
                } else if local.state.dirty {
                    // Local wins; overwrite with the freshly-fetched ETag so a
                    // genuine conflict can't 412 and wedge every later sync.
                    if let Ok(lcard) = serde_json::from_str::<con::ContactCard>(&local.card_json) {
                        let new_etag = dav::put_resource(http, cfg, &rc.href, dav::VCARD_CT, &con::card_to_vcard(&lcard), remote_etag).await?;
                        cal::sync_mark_pushed(pool, "contacts", "contact_id", local.id, &rc.href, new_etag.as_deref(), 0).await?;
                        sqlx::query("UPDATE contacts SET card_json = json_set(card_json, '$.addressBook', ?) WHERE contact_id = ?")
                            .bind(&rc.address_book).bind(local.id).execute(pool).await?;
                        pushed += 1;
                    }
                } else if local.etag.as_deref() != remote_etag {
                    con::sync_write_contact(pool, Some(local.id), &card, &rc.href, remote_etag, 0).await?;
                    pulled += 1;
                }
            }
        }
    }

    // 4. Orphans: local contacts with no matching remote card this round. Rows that
    //    belong to another backend (a Google resource name in `remote_id`) are left
    //    strictly alone.
    for (i, local) in locals.iter().enumerate() {
        if seen.contains(&i) {
            continue;
        }
        let ours = is_dav_href(local.remote_id.as_deref());
        if local.state.has_remote_id && !ours {
            continue;
        }
        let remote_url = local.remote_id.as_deref().and_then(|href| Url::parse(href).ok());
        let target = if let Some(href) = remote_url {
            books.iter().find(|book| belongs_to(&book.url, &href)).map(|book| &book.url)
        } else if let Some(owner) = local.address_book.as_deref() {
            books.iter().find(|book| book.url.as_str() == owner).map(|book| &book.url)
        } else {
            Some(primary)
        };
        let Some(target) = target else { continue };
        match reconcile_orphan(local.state, true) {
            OrphanAction::Noop => {}
            OrphanAction::DropLocal | OrphanAction::DeleteLocal => {
                cal::sync_hard_delete(pool, "contacts", "contact_id", local.id).await?;
            }
            OrphanAction::CreateRemote | OrphanAction::RecreateRemote => {
                if let Ok(mut lcard) = serde_json::from_str::<con::ContactCard>(&local.card_json) {
                    if lcard.uid.trim().is_empty() {
                        lcard.uid = con::new_uid();
                    }
                    let name = dav::sanitize_name(&lcard.uid, "contact");
                    let Ok(href) = target.join(&format!("{name}.vcf")) else { continue };
                    let new_etag = dav::put_resource(http, cfg, href.as_str(), dav::VCARD_CT, &con::card_to_vcard(&lcard), None).await?;
                    cal::sync_mark_pushed(pool, "contacts", "contact_id", local.id, href.as_str(), new_etag.as_deref(), 0).await?;
                    sqlx::query("UPDATE contacts SET card_json = json_set(card_json, '$.addressBook', ?) WHERE contact_id = ?")
                        .bind(target.as_str()).bind(local.id).execute(pool).await?;
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

    #[test]
    fn only_dav_hrefs_count_as_ours() {
        assert!(is_dav_href(Some("https://dav.example.com/addressbooks/u/default/x.vcf")));
        assert!(is_dav_href(Some("http://localhost:5232/u/book/x.vcf")));
        assert!(!is_dav_href(Some("people/c12345")));
        assert!(!is_dav_href(Some("")));
        assert!(!is_dav_href(None));
    }

    // ── Full reconcile against a stand-in CardDAV server ──
    //
    // The reconcile is where the risk lives: a wrong branch either loses a local
    // edit or deletes someone's contact. This drives `run_sync` end to end over a
    // real in-memory contacts table and a server that actually stores what we PUT,
    // covering all four directions plus the "leave other backends alone" guard.

    use axum::{extract::Request, response::Response, routing::any, Router};
    use sqlx::sqlite::SqlitePoolOptions;
    use sqlx::SqlitePool;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Server {
        /// href suffix ("a.vcf") → (vCard, ETag)
        cards: std::collections::BTreeMap<String, (String, String)>,
        log: Vec<String>,
    }

    type Shared = Arc<Mutex<Server>>;

    const BOOK: &str = "/books/default/";

    fn xml(body: String) -> Response {
        Response::builder()
            .status(207)
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(axum::body::Body::from(body))
            .unwrap()
    }

    fn empty(status: u16, etag: Option<&str>) -> Response {
        let mut b = Response::builder().status(status);
        if let Some(e) = etag {
            b = b.header("ETag", e);
        }
        b.body(axum::body::Body::empty()).unwrap()
    }

    async fn mock_carddav(
        axum::extract::State(state): axum::extract::State<Shared>,
        req: Request,
    ) -> Response {
        let path = req.uri().path().to_string();
        let method = req.method().as_str().to_string();
        let body = axum::body::to_bytes(req.into_body(), 256 * 1024).await.unwrap();
        let body = String::from_utf8_lossy(&body).to_string();
        let mut srv = state.lock().unwrap();
        srv.log.push(format!("{method} {path}"));

        match (method.as_str(), path.as_str()) {
            ("PROPFIND", "/") => xml(r#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/</d:href><d:propstat><d:prop>
                <d:current-user-principal><d:href>/p/</d:href></d:current-user-principal>
                </d:prop></d:propstat></d:response></d:multistatus>"#.into()),
            ("PROPFIND", "/p/") => xml(r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:carddav"><d:response><d:href>/p/</d:href>
                <d:propstat><d:prop><c:addressbook-home-set><d:href>/books/</d:href></c:addressbook-home-set>
                </d:prop></d:propstat></d:response></d:multistatus>"#.into()),
            ("PROPFIND", "/books/") => xml(format!(
                r#"<d:multistatus xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
                   <d:response><d:href>/books/</d:href><d:propstat><d:prop>
                     <d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>
                   <d:response><d:href>{BOOK}</d:href><d:propstat><d:prop>
                     <d:resourcetype><d:collection/><card:addressbook/></d:resourcetype>
                     <d:displayname>Personal</d:displayname></d:prop></d:propstat></d:response>
                   </d:multistatus>"#
            )),
            ("REPORT", p) if p == BOOK => {
                let mut out = String::from(r#"<d:multistatus xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">"#);
                for (name, (vcf, etag)) in &srv.cards {
                    out.push_str(&format!(
                        r#"<d:response><d:href>{BOOK}{name}</d:href><d:propstat><d:prop>
                           <d:getetag>{etag}</d:getetag>
                           <card:address-data>{}</card:address-data></d:prop></d:propstat></d:response>"#,
                        vcf.replace('&', "&amp;").replace('<', "&lt;")
                    ));
                }
                out.push_str("</d:multistatus>");
                xml(out)
            }
            ("PUT", p) if p.starts_with(BOOK) => {
                let name = p.trim_start_matches(BOOK).to_string();
                let etag = format!("\"v{}\"", srv.log.len());
                srv.cards.insert(name, (body, etag.clone()));
                empty(201, Some(&etag))
            }
            ("DELETE", p) if p.starts_with(BOOK) => {
                srv.cards.remove(p.trim_start_matches(BOOK));
                empty(204, None)
            }
            _ => empty(404, None),
        }
    }

    async fn spawn(state: Shared) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().fallback(any(mock_carddav)).with_state(state);
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://127.0.0.1:{port}/")
    }

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        // The columns this module touches, mirroring db::init_user_db.
        sqlx::query(
            r#"CREATE TABLE contacts (
                contact_id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT, display_name TEXT, mail_address TEXT,
                phone_number_country_code TEXT, website TEXT, uid TEXT,
                first_name TEXT, last_name TEXT, company TEXT, job_title TEXT,
                is_favorite INTEGER NOT NULL DEFAULT 0, categories TEXT, card_json TEXT,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                updated_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                remote_id TEXT, etag TEXT,
                dirty INTEGER NOT NULL DEFAULT 0, deleted INTEGER NOT NULL DEFAULT 0,
                remote_updated_ms INTEGER, local_updated_ms INTEGER
            )"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        init_books(&pool).await.unwrap();
        pool
    }

    fn vcard(uid: &str, fullname: &str) -> String {
        format!("BEGIN:VCARD\r\nVERSION:3.0\r\nUID:{uid}\r\nFN:{fullname}\r\nN:;{fullname};;;\r\nEND:VCARD\r\n")
    }

    fn card_json(uid: &str, display: &str) -> String {
        let card = con::ContactCard {
            uid: uid.to_string(),
            display_name: display.to_string(),
            ..Default::default()
        };
        serde_json::to_string(&card).unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_local(
        pool: &SqlitePool,
        uid: &str,
        display: &str,
        remote_id: Option<&str>,
        etag: Option<&str>,
        dirty: bool,
        deleted: bool,
    ) -> i64 {
        let res = sqlx::query(
            "INSERT INTO contacts (uid, display_name, card_json, remote_id, etag, dirty, deleted) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(uid)
        .bind(display)
        .bind(card_json(uid, display))
        .bind(remote_id)
        .bind(etag)
        .bind(dirty as i64)
        .bind(deleted as i64)
        .execute(pool)
        .await
        .unwrap();
        res.last_insert_rowid()
    }

    #[tokio::test]
    async fn reconcile_moves_every_direction_and_spares_other_backends() {
        let srv: Shared = Arc::new(Mutex::new(Server::default()));
        {
            let mut s = srv.lock().unwrap();
            // Only on the server: must be pulled.
            s.cards.insert("remote-only.vcf".into(), (vcard("remote-only", "Remote Only"), "\"r1\"".into()));
            // Locally edited: our copy must overwrite this.
            s.cards.insert("edited.vcf".into(), (vcard("edited", "Stale Server Name"), "\"r2\"".into()));
            // Locally deleted: must disappear from the server.
            s.cards.insert("gone.vcf".into(), (vcard("gone", "Doomed"), "\"r3\"".into()));
        }
        let base = spawn(srv.clone()).await;
        let pool = test_pool().await;
        let book = format!("{}books/default/", base);

        let edited_id = insert_local(&pool, "edited", "Fresh Local Name", Some(&format!("{book}edited.vcf")), Some("\"r2\""), true, false).await;
        insert_local(&pool, "gone", "Doomed", Some(&format!("{book}gone.vcf")), Some("\"r3\""), false, true).await;
        let created_id = insert_local(&pool, "brand-new", "Brand New", None, None, true, false).await;
        let google_id = insert_local(&pool, "people/c1", "Google Person", Some("people/c1"), Some("\"g1\""), false, false).await;

        let cfg = DavConfig {
            kind: DavKind::CardDav,
            url: base.clone(),
            username: "user".into(),
            password: "app-specific".into(),
        };
        let stats = run_sync(&dav::dav_client(), &cfg, &pool).await.unwrap();
        assert_eq!(stats["pulled"], 1, "the server-only contact should be pulled: {stats}");
        assert_eq!(stats["pushed"], 2, "the edit and the new contact should be pushed: {stats}");
        assert_eq!(stats["deletedRemote"], 1, "the tombstone should delete remotely: {stats}");

        // Scoped so the guard is dropped before the awaits below.
        {
            let s = srv.lock().unwrap();
            assert!(!s.cards.contains_key("gone.vcf"), "tombstoned contact still on the server");
            assert!(s.cards["edited.vcf"].0.contains("Fresh Local Name"), "local edit did not win");
            assert!(
                s.cards.keys().any(|k| k.starts_with("brand-new")),
                "new local contact was not created: {:?}",
                s.cards.keys().collect::<Vec<_>>()
            );
        }

        // Pulled row exists and is clean; the pushed rows are no longer dirty.
        let pulled: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM contacts WHERE uid = 'remote-only' AND dirty = 0 AND deleted = 0")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(pulled, 1);
        for id in [edited_id, created_id] {
            let dirty: i64 = sqlx::query_scalar("SELECT dirty FROM contacts WHERE contact_id = ?").bind(id).fetch_one(&pool).await.unwrap();
            assert_eq!(dirty, 0, "row {id} should be marked pushed");
        }
        let remote_id: Option<String> = sqlx::query_scalar("SELECT remote_id FROM contacts WHERE contact_id = ?")
            .bind(created_id).fetch_one(&pool).await.unwrap();
        assert!(remote_id.unwrap().starts_with(&book), "created row should remember its href");

        // The Google-owned row is untouched — not deleted, not re-homed, not pushed.
        let (rid, dirty): (Option<String>, i64) =
            sqlx::query_as("SELECT remote_id, dirty FROM contacts WHERE contact_id = ?")
                .bind(google_id).fetch_one(&pool).await.unwrap();
        assert_eq!(rid.as_deref(), Some("people/c1"));
        assert_eq!(dirty, 0);
        assert!(
            !srv.lock().unwrap().cards.keys().any(|k| k.starts_with("people")),
            "a Google-synced contact must never be pushed to CardDAV"
        );
    }

    #[tokio::test]
    async fn a_clean_local_row_takes_the_remote_copy_when_the_etag_moves() {
        let srv: Shared = Arc::new(Mutex::new(Server::default()));
        {
            let mut s = srv.lock().unwrap();
            s.cards.insert("x.vcf".into(), (vcard("x", "New Server Name"), "\"v2\"".into()));
        }
        let base = spawn(srv.clone()).await;
        let pool = test_pool().await;
        let href = format!("{base}books/default/x.vcf");
        // Same contact, stale ETag, no local edits.
        insert_local(&pool, "x", "Old Local Name", Some(&href), Some("\"v1\""), false, false).await;

        let cfg = DavConfig { kind: DavKind::CardDav, url: base, username: "u".into(), password: "p".into() };
        let stats = run_sync(&dav::dav_client(), &cfg, &pool).await.unwrap();
        assert_eq!(stats["pulled"], 1);
        assert_eq!(stats["pushed"], 0);
        let (name, etag): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT display_name, etag FROM contacts WHERE uid = 'x'")
                .fetch_one(&pool).await.unwrap();
        assert_eq!(name.as_deref(), Some("New Server Name"));
        assert_eq!(etag.as_deref(), Some("\"v2\""));
        // Unchanged on a second pass: nothing is dirty, the ETag now matches.
        let again = run_sync(&dav::dav_client(), &cfg, &pool).await.unwrap();
        assert_eq!(again["pulled"], 0);
        assert_eq!(again["pushed"], 0);
        assert_eq!(again["deletedRemote"], 0);
    }

    #[test]
    fn cards_are_collected_with_absolute_hrefs() {
        let collection = Url::parse("https://dav.example.com/addressbooks/user/default/").unwrap();
        let body = r#"<d:multistatus xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
            <d:response><d:href>/addressbooks/user/default/a.vcf</d:href>
              <d:propstat><d:prop><d:getetag>"e1"</d:getetag>
              <card:address-data>BEGIN:VCARD&#13;
FN:Ada &amp; Co&#13;
END:VCARD</card:address-data></d:prop></d:propstat></d:response>
            <d:response><d:href>/addressbooks/user/default/</d:href>
              <d:propstat><d:prop><d:getetag>"c"</d:getetag></d:prop></d:propstat></d:response>
        </d:multistatus>"#;
        let mut out = Vec::new();
        collect_cards(body, &collection, &mut out);
        assert_eq!(out.len(), 1, "the collection itself must be skipped");
        assert_eq!(out[0].href, "https://dav.example.com/addressbooks/user/default/a.vcf");
        assert_eq!(out[0].etag.as_deref(), Some("\"e1\""));
        assert!(out[0].vcf.contains("FN:Ada & Co"));
    }
}
