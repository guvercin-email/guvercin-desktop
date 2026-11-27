use axum::http::HeaderValue;
use axum::{
    body::{Body, Bytes},
    extract::{Json, Path, Query, State},
    http::{
        header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE},
        StatusCode,
    },
    response::{IntoResponse, Response},
};
use base64::prelude::BASE64_STANDARD;
use base64::Engine;
use chrono::{DateTime, Utc};
use mailparse::MailHeaderMap;
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;

use crate::{
    error::AppError,
    imap_session::{self, ImapState},
    mail_models::{
        merge_mailbox_label_into_preview, AdvancedSearchRequest, AdvancedSearchResponse,
        AttachmentInfo, ConnectImapBody, MailContent, MailListResponse, MailboxListResponse,
    },
};

pub struct MailAppState {
    pub _db: Arc<crate::db::AppState>,
    pub imap: Arc<ImapState>,
    pub sync_locks: Mutex<HashMap<i64, Arc<Mutex<()>>>>,
}

impl MailAppState {
    pub(crate) async fn account_sync_lock(&self, account_id: i64) -> Arc<Mutex<()>> {
        self.sync_locks.lock().await.entry(account_id).or_default().clone()
    }
}

#[derive(Debug, Deserialize)]
pub struct ImportPreviewQuery {
    pub kind: String,
}

pub async fn post_import_preview(
    Path(_account_id): Path<i64>,
    Query(q): Query<ImportPreviewQuery>,
    bytes: Bytes,
) -> impl IntoResponse {
    let kind = q.kind.trim().to_ascii_lowercase();
    if kind != "eml" && kind != "msg" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "kind must be eml or msg" })),
        )
            .into_response();
    }

    let import_id = format!("import-{}", Utc::now().timestamp_millis());
    let raw = bytes.to_vec();

    let mut content: MailContent = if kind == "eml" {
        imap_session::parse_mail_content_with_attachment_data(import_id.clone(), &raw)
    } else {
        fn looks_like_cfb(raw: &[u8]) -> bool {
            const MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
            raw.len() >= MAGIC.len() && raw[..MAGIC.len()] == MAGIC
        }

        // guvercin currently exports "MSG" as RFC822 raw bytes (IMAP raw), not a real CFB .msg file.
        // Only try the .msg parser if the file looks like a real CFB container; otherwise treat as RFC822.
        if !looks_like_cfb(&raw) {
            imap_session::parse_mail_content_with_attachment_data(import_id.clone(), &raw)
        } else {
            let parsed = std::panic::catch_unwind(|| tiny_msg::Email::from_bytes(&raw));
            let email = match parsed {
                Ok(email) => email,
                Err(_) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": "Invalid .msg file" })),
                    )
                        .into_response();
                }
            };

            let (from_name, from_address) = email
                .from
                .clone()
                .unwrap_or_else(|| (String::new(), String::new()));
            let subject = email.subject.clone().unwrap_or_default();
            let date = email
                .sent_date
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_default();

            let body = email.body.clone().unwrap_or_default();
            let body_trim = body.trim_start().to_ascii_lowercase();
            let looks_like_html = body_trim.starts_with("<!doctype")
                || body_trim.starts_with("<html")
                || body_trim.starts_with("<body")
                || body.contains("</div>")
                || body.contains("</p>");

            let (html_body, plain_body) = if looks_like_html {
                (body, String::new())
            } else {
                (String::new(), body)
            };

            let cc_value = email
                .cc
                .iter()
                .map(|(name, addr)| {
                    let name = name.trim();
                    let addr = addr.trim();
                    if !name.is_empty() && !addr.is_empty() {
                        format!("{name} <{addr}>")
                    } else if !addr.is_empty() {
                        addr.to_string()
                    } else {
                        name.to_string()
                    }
                })
                .filter(|v| !v.trim().is_empty())
                .collect::<Vec<_>>()
                .join(", ");

            let bcc_value = email
                .bcc
                .iter()
                .map(|(name, addr)| {
                    let name = name.trim();
                    let addr = addr.trim();
                    if !name.is_empty() && !addr.is_empty() {
                        format!("{name} <{addr}>")
                    } else if !addr.is_empty() {
                        addr.to_string()
                    } else {
                        name.to_string()
                    }
                })
                .filter(|v| !v.trim().is_empty())
                .collect::<Vec<_>>()
                .join(", ");

            let attachments = email
                .attachments
                .iter()
                .enumerate()
                .map(|(idx, attachment)| AttachmentInfo {
                    id: idx.to_string(),
                    filename: attachment.name.clone(),
                    content_type: "application/octet-stream".to_string(),
                    size: attachment.data.len(),
                    is_inline: false,
                    data_base64: Some(BASE64_STANDARD.encode(&attachment.data)),
                    content_id: None,
                })
                .collect::<Vec<_>>();

            MailContent {
                id: import_id.clone(),
                subject,
                from_name,
                from_address,
                cc: cc_value,
                bcc: bcc_value,
                date,
                html_body,
                plain_body,
                attachments,
            }
        }
    };

    if content.subject.trim().is_empty() {
        content.subject = "(No Subject)".to_string();
    }
    if content.date.trim().is_empty() {
        content.date = Utc::now().to_rfc3339();
    }
    if content.from_address.trim().is_empty() {
        content.from_address = "unknown".to_string();
    }

    let mail = json!({
        "id": import_id,
        "name": content.from_name.clone(),
        "address": content.from_address.clone(),
        "subject": content.subject.clone(),
        "date": content.date.clone(),
        "seen": true,
        "flagged": false,
        "recipient_to": "",
        "size": raw.len(),
        "importance": 0,
        "content_type": "text/plain",
        "category": "",
        "labels": [],
        "isImported": true,
    });

    (
        StatusCode::OK,
        Json(json!({ "mail": mail, "content": content })),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub struct ProxyImageQuery {
    pub url: String,
}

fn is_public_image_ip(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;

    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !matches!((a, b, c),
                (0 | 10 | 127 | 224..=255, _, _)
                | (100, 64..=127, _) | (169, 254, _) | (172, 16..=31, _)
                | (192, 0, 0 | 2) | (192, 88, 99) | (192, 168, _)
                | (198, 18 | 19, _) | (198, 51, 100) | (203, 0, 113))
        }
        IpAddr::V6(ip) => {
            let segments = ip.segments();
            // Global unicast only, excluding transition and documentation ranges.
            segments[0] & 0xe000 == 0x2000
                && !(segments[0] == 0x2001 && (segments[1] < 0x0200 || segments[1] == 0x0db8))
                && segments[0] != 0x2002
                && segments[0] != 0x3fff
        }
    }
}

fn is_public_image_url(url: &reqwest::Url) -> bool {
    if !matches!(url.scheme(), "http" | "https") || !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    match url.host() {
        Some(url::Host::Ipv4(ip)) => is_public_image_ip(ip.into()),
        Some(url::Host::Ipv6(ip)) => is_public_image_ip(ip.into()),
        Some(url::Host::Domain(_)) => true,
        None => false,
    }
}

struct PublicImageDns;

impl reqwest::dns::Resolve for PublicImageDns {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let host = name.as_str();
            let addresses: Vec<_> = tokio::net::lookup_host((host, 0))
                .await?
                .filter(|address| is_public_image_ip(address.ip()))
                .collect();
            if addresses.is_empty() {
                return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "Image host has no public address").into());
            }
            Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

#[cfg(test)]
mod image_proxy_tests {
    use super::*;

    #[test]
    fn only_public_image_destinations_are_allowed() {
        for address in [
            "127.0.0.1", "169.254.3.1", "100.64.0.1",
            "10.0.0.1", "192.168.1.1", "172.16.0.1", "::1", "::ffff:127.0.0.1", "fe80::1",
        ] {
            assert!(!is_public_image_ip(address.parse().unwrap()), "{address}");
        }
        for address in ["8.8.8.8", "2606:4700:4700::1111"] {
            assert!(is_public_image_ip(address.parse().unwrap()), "{address}");
        }
        assert!(!is_public_image_url(&reqwest::Url::parse("http://127.0.0.1:5500/health").unwrap()));
        assert!(!is_public_image_url(&reqwest::Url::parse("https://user:pass@example.test/image").unwrap()));
    }
}

pub async fn get_proxy_image(
    Path(_account_id): Path<i64>,
    Query(q): Query<ProxyImageQuery>,
) -> impl IntoResponse {
    let url = q.url.trim();
    if url.is_empty() || url.len() > 4096 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "url is required" })),
        )
            .into_response();
    }
    let url = match reqwest::Url::parse(url) {
        Ok(url) if is_public_image_url(&url) => url,
        _ => return (StatusCode::BAD_REQUEST, "Image URL must have a public HTTP or HTTPS destination").into_response(),
    };

    let client = match reqwest::Client::builder()
        .no_proxy()
        .dns_resolver(Arc::new(PublicImageDns))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() > 5 || !is_public_image_url(attempt.url()) {
                attempt.error("Invalid image redirect")
            } else {
                attempt.follow()
            }
        }))
        .timeout(std::time::Duration::from_secs(12))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    let mut response = match client
        .get(url)
        .header(reqwest::header::USER_AGENT, "guvercin/1.0")
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    if !response.status().is_success() {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("upstream status {}", response.status()) })),
        )
            .into_response();
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    let mime = content_type.split(';').next().unwrap_or_default().trim();
    if !matches!(mime.to_ascii_lowercase().as_str(),
        "image/jpeg" | "image/png" | "image/gif" | "image/webp" | "image/avif"
        | "image/bmp" | "image/x-icon" | "image/vnd.microsoft.icon") {
        return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "Unsupported image format").into_response();
    }

    const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
    if response.content_length().is_some_and(|size| size > MAX_IMAGE_BYTES as u64) {
        return (StatusCode::PAYLOAD_TOO_LARGE, "Image is too large").into_response();
    }
    let mut bytes = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if bytes.len() + chunk.len() <= MAX_IMAGE_BYTES => bytes.extend_from_slice(&chunk),
            Ok(Some(_)) => return (StatusCode::PAYLOAD_TOO_LARGE, "Image is too large").into_response(),
            Ok(None) => break,
            Err(_) => return (StatusCode::BAD_GATEWAY, "Could not read image").into_response(),
        }
    }

    let builder = Response::builder()
        .status(StatusCode::OK)
        .header(
            CONTENT_TYPE,
            HeaderValue::from_str(&content_type)
                .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
        )
        .header("Cache-Control", "private, max-age=86400");

    builder
        .body(Body::from(bytes))
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::empty())
                .unwrap()
        })
        .into_response()
}

pub async fn connect_imap(
    State(state): State<Arc<MailAppState>>,
    Json(body): Json<ConnectImapBody>,
) -> impl IntoResponse {
    let result = tokio::task::spawn_blocking({
        let imap_state = state.imap.clone();
        let email = body.email.clone();
        let password = body.password.clone();
        let host = body.imap_host.clone();
        let port = body.imap_port.unwrap_or(143) as u16;
        let ssl = body.ssl_mode.clone();
        let id = body.account_id;
        move || {
            imap_session::connect_and_login(&imap_state, id, &email, &password, &host, port, &ssl)
        }
    })
    .await
    .unwrap_or_else(|e| Err(format!("Task panic: {e}")));

    if result.is_ok() {
        let inner = match state._db.ensure_ready(false).await {
            Ok(inner) => inner,
            Err(e) => return e.into_response(),
        };
        let _ =
            sqlx::query("UPDATE accounts SET auth_token = ?, ssl_mode = ? WHERE account_id = ?")
                .bind(&body.password)
                .bind(&body.ssl_mode)
                .bind(body.account_id)
                .execute(&inner.general_pool)
                .await;
    }

    match result {
        Ok(()) => (StatusCode::OK, Json(json!({"status": "connected"}))).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

pub async fn connect_imap_stored(
    State(state): State<Arc<MailAppState>>,
    Path(account_id): Path<i64>,
) -> impl IntoResponse {
    let pool = match state._db.ensure_ready(false).await {
        Ok(inner) => inner.general_pool.clone(),
        Err(e) => return e.into_response(),
    };

    let account = match sqlx::query(
        "SELECT email_address, imap_host, imap_port, auth_token, ssl_mode, provider_type FROM accounts WHERE account_id = ?",
    )
    .bind(account_id)
    .fetch_one(&pool)
    .await {
        Ok(a) => a,
        Err(_) => return (StatusCode::NOT_FOUND, Json(json!({"error": "Account not found"}))).into_response(),
    };

    let email: String = account
        .try_get::<Option<String>, _>("email_address")
        .unwrap_or_default()
        .unwrap_or_default();
    let host: String = account
        .try_get::<Option<String>, _>("imap_host")
        .unwrap_or_default()
        .unwrap_or_default();
    let port: i64 = account
        .try_get::<Option<i64>, _>("imap_port")
        .unwrap_or_default()
        .unwrap_or(143);
    let password: String = account
        .try_get::<Option<String>, _>("auth_token")
        .unwrap_or_default()
        .unwrap_or_default();
    let ssl: String = account
        .try_get::<Option<String>, _>("ssl_mode")
        .unwrap_or_default()
        .unwrap_or_else(|| "STARTTLS".to_string());
    let provider: String = account
        .try_get::<Option<String>, _>("provider_type")
        .unwrap_or_default()
        .unwrap_or_default();

    let is_gmail = provider == crate::oauth::PROVIDER_GMAIL;

    // For Gmail we authenticate with a fresh OAuth2 access token; for IMAP we
    // use the stored password. Resolve the effective secret before entering the
    // blocking connect closure.
    let secret: String = if is_gmail {
        match crate::oauth::access_token_for_account(&pool, account_id).await {
            Ok(tok) => tok,
            Err(e) => return (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
        }
    } else {
        password
    };

    if secret.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "No credentials stored"})),
        )
            .into_response();
    }

    let result = tokio::task::spawn_blocking({
        let imap_state = state.imap.clone();
        move || {
            if is_gmail {
                imap_session::connect_and_login_xoauth2(
                    &imap_state,
                    account_id,
                    &email,
                    &secret,
                    &host,
                    port as u16,
                    &ssl,
                )
            } else {
                imap_session::connect_and_login(
                    &imap_state,
                    account_id,
                    &email,
                    &secret,
                    &host,
                    port as u16,
                    &ssl,
                )
            }
        }
    })
    .await
    .unwrap_or_else(|e| Err(format!("Task panic: {e}")));

    match result {
        Ok(()) => (StatusCode::OK, Json(json!({"status": "connected"}))).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

pub async fn get_mailboxes(
    State(state): State<Arc<MailAppState>>,
    Path(account_id): Path<i64>,
) -> impl IntoResponse {
    let entries = tokio::task::spawn_blocking({
        let imap_state = state.imap.clone();
        move || imap_session::list_mailboxes_detailed(&imap_state, account_id)
    })
    .await
    .unwrap_or_default();

    Json(MailboxListResponse::from_entries(entries))
}

#[derive(Deserialize)]
pub struct MailListQuery {
    #[serde(default = "default_inbox")]
    pub mailbox: String,
    #[serde(default = "default_page")]
    pub page: usize,
    #[serde(default = "default_per_page")]
    pub per_page: usize,
}
fn default_inbox() -> String {
    "INBOX".into()
}
fn default_page() -> usize {
    1
}
fn default_per_page() -> usize {
    50
}

#[derive(Deserialize)]
pub struct MailContentQuery {
    #[serde(default = "default_inbox")]
    pub mailbox: String,
    pub uid_validity: Option<u32>,
}

#[derive(Deserialize)]
pub struct MailboxCreateBody {
    pub name: String,
}

pub async fn get_mail_list(
    State(state): State<Arc<MailAppState>>,
    Path(account_id): Path<i64>,
    Query(q): Query<MailListQuery>,
) -> Result<Json<MailListResponse>, AppError> {
    let sync_lock = state.account_sync_lock(account_id).await;
    let _guard = sync_lock.lock().await;
    let mailbox = crate::mail_models::mailbox_identity(&q.mailbox).to_string();
    let imap_session::MailboxPage { generation, total, mut previews } =
        tokio::task::spawn_blocking({
            let imap_state = state.imap.clone();
            let mailbox = mailbox.clone();
            move || imap_session::fetch_mail_list(&imap_state, account_id, &mailbox, q.page, q.per_page, None)
        })
        .await
        .map_err(|error| AppError::BadRequest(error.to_string()))?
        .map_err(AppError::BadRequest)?;

    previews.iter_mut().for_each(|mail| merge_mailbox_label_into_preview(mail, &mailbox));

    if let Ok(pool) = crate::db::get_user_db_pool(&state._db, account_id).await {
        crate::offline_routes::record_mailbox_generation(&pool, &mailbox, generation).await?;
        for mail in &previews {
            let _ = crate::offline_routes::cache_mail_preview(&pool, &mailbox, mail).await;
        }
    }

    Ok(Json(MailListResponse { total_count: total, mails: previews }))
}

pub async fn search_advanced(
    State(state): State<Arc<MailAppState>>,
    Path(account_id): Path<i64>,
    Json(body): Json<AdvancedSearchRequest>,
) -> impl IntoResponse {
    if matches!(body.scope, crate::mail_models::SearchScope::Mailboxes) && body.mailboxes.is_empty()
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "mailboxes must be provided when scope=mailboxes" })),
        )
            .into_response();
    }

    let result = tokio::task::spawn_blocking({
        let imap_state = state.imap.clone();
        let req = body.clone();
        move || imap_session::advanced_search(&imap_state, account_id, &req)
    })
    .await
    .unwrap_or_else(|e| Err(format!("Task panic: {e}")));

    match result {
        Ok(mails) => Json(AdvancedSearchResponse {
            total_count: mails.len(),
            mails,
        })
        .into_response(),
        Err(error) => (StatusCode::BAD_GATEWAY, Json(json!({ "error": error }))).into_response(),
    }
}

pub async fn create_mailbox(
    State(state): State<Arc<MailAppState>>,
    Path(account_id): Path<i64>,
    Json(body): Json<MailboxCreateBody>,
) -> impl IntoResponse {
    let mailbox = body.name.trim().to_string();
    if mailbox.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Mailbox name is required" })),
        )
            .into_response();
    }

    let result = tokio::task::spawn_blocking({
        let imap_state = state.imap.clone();
        move || imap_session::create_mailbox(&imap_state, account_id, &mailbox)
    })
    .await
    .unwrap_or_else(|e| Err(format!("Task panic: {e}")));

    match result {
        Ok(()) => (StatusCode::OK, Json(json!({ "status": "created" }))).into_response(),
        Err(error) => (StatusCode::BAD_GATEWAY, Json(json!({ "error": error }))).into_response(),
    }
}

pub async fn get_mail_content(
    State(state): State<Arc<MailAppState>>,
    Path((account_id, uid)): Path<(i64, String)>,
    Query(q): Query<MailContentQuery>,
) -> impl IntoResponse {
    let uid_for_response = uid.clone();
    let raw = tokio::task::spawn_blocking({
        let imap_state = state.imap.clone();
        let uid = uid.clone();
        let mailbox = q.mailbox.clone();
        move || imap_session::fetch_mail_raw_with_validity(&imap_state, account_id, &mailbox, &uid, q.uid_validity)
    })
    .await
    .ok()
    .flatten();

    if let Some((raw_bytes, uid_validity)) = raw {
        let is_drafts_mailbox = {
            let lower = q.mailbox.trim().to_ascii_lowercase();
            lower == "drafts"
                || lower.ends_with("/drafts")
                || lower.ends_with(".drafts")
                || lower.contains("drafts")
        };
        let mut content = if is_drafts_mailbox {
            imap_session::parse_mail_content_with_attachment_data(uid_for_response, &raw_bytes)
        } else {
            imap_session::parse_mail_content(uid_for_response, &raw_bytes)
        };
        let local_html = if is_drafts_mailbox {
            content.html_body.clone()
        } else {
            crate::offline_routes::rewrite_inline_attachment_urls(
                &content, &format!("/api/offline/{account_id}/local-content/{uid}"),
                &q.mailbox, uid_validity,
            )
        };
        let cached = if let Ok(pool) = crate::db::get_user_db_pool(&state._db, account_id).await {
            let date_ms: i64 = DateTime::parse_from_rfc2822(&content.date)
                .map(|dt| dt.timestamp_millis())
                .unwrap_or(0);
            sqlx::query(
                r#"
                UPDATE local_mail_cache
                SET plain_body = ?, html_body = ?, date_value = ?, date_ms = ?, cc_value = ?, bcc_value = ?, raw_rfc822 = ?, updated_at = CURRENT_TIMESTAMP
                WHERE uid = ? AND folder = ? AND uid_validity = ?
                "#,
            )
            .bind(&content.plain_body)
            .bind(&local_html)
            .bind(&content.date)
            .bind(date_ms)
            .bind(&content.cc)
            .bind(&content.bcc)
            .bind(raw_bytes)
            .bind(&content.id)
            .bind(&q.mailbox)
            .bind(uid_validity)
            .execute(&pool)
            .await
            .is_ok_and(|result| result.rows_affected() > 0)
        } else {
            false
        };
        if cached {
            content.html_body = local_html;
        } else if !is_drafts_mailbox {
            content.html_body = crate::offline_routes::rewrite_inline_attachment_urls(
                &content, &format!("/api/mail/{account_id}/content/{uid}"),
                &q.mailbox, uid_validity,
            );
        }
        Json(content).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Mail not found"})),
        )
            .into_response()
    }
}

pub async fn get_mail_raw(
    State(state): State<Arc<MailAppState>>,
    Path((account_id, uid)): Path<(i64, String)>,
    Query(q): Query<MailContentQuery>,
) -> impl IntoResponse {
    let file_name = if uid.trim().is_empty() {
        "message.msg".to_string()
    } else {
        format!("{uid}.msg")
    };

    let raw = tokio::task::spawn_blocking({
        let imap_state = state.imap.clone();
        let uid = uid.clone();
        let mailbox = q.mailbox.clone();
        move || imap_session::fetch_mail_raw_in_mailbox(&imap_state, account_id, &mailbox, &uid, q.uid_validity)
    })
    .await
    .ok()
    .flatten();

    if let Some(raw_bytes) = raw {
        let escaped = file_name.replace('"', "\\\"");
        let encoded = percent_encode_filename(&file_name);
        let disposition = format!("attachment; filename=\"{escaped}\"; filename*=UTF-8''{encoded}");
        return Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "application/vnd.ms-outlook")
            .header(CONTENT_LENGTH, raw_bytes.len().to_string())
            .header(CONTENT_DISPOSITION, disposition)
            .body(Body::from(raw_bytes))
            .unwrap_or_else(|_| {
                Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::empty())
                    .unwrap()
            })
            .into_response();
    }

    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": "Mail not found"})),
    )
        .into_response()
}

fn parse_reply_seed_from_raw(raw: &[u8]) -> serde_json::Value {
    let mut message_id = String::new();
    let mut references = String::new();
    let mut in_reply_to = String::new();
    let mut reply_to = String::new();

    if let Ok(parsed) = mailparse::parse_mail(raw) {
        let headers = parsed.get_headers();
        message_id = headers
            .get_first_value("Message-ID")
            .unwrap_or_default()
            .trim()
            .to_string();
        references = headers
            .get_first_value("References")
            .unwrap_or_default()
            .trim()
            .to_string();
        in_reply_to = headers
            .get_first_value("In-Reply-To")
            .unwrap_or_default()
            .trim()
            .to_string();
        reply_to = headers
            .get_first_value("Reply-To")
            .unwrap_or_default()
            .trim()
            .to_string();
    }

    json!({
        "message_id": message_id,
        "references": references,
        "in_reply_to": in_reply_to,
        "reply_to": reply_to,
    })
}

pub async fn get_reply_seed(
    State(state): State<Arc<MailAppState>>,
    Path((account_id, uid)): Path<(i64, String)>,
    Query(q): Query<MailContentQuery>,
) -> impl IntoResponse {
    let raw = tokio::task::spawn_blocking({
        let imap_state = state.imap.clone();
        let uid = uid.clone();
        let mailbox = q.mailbox.clone();
        move || imap_session::fetch_mail_raw_in_mailbox(&imap_state, account_id, &mailbox, &uid, q.uid_validity)
    })
    .await
    .ok()
    .flatten();

    if let Some(raw_bytes) = raw {
        Json(parse_reply_seed_from_raw(&raw_bytes)).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Mail not found"})),
        )
            .into_response()
    }
}

pub async fn download_attachment(
    State(state): State<Arc<MailAppState>>,
    Path((account_id, uid, attachment_index)): Path<(i64, String, usize)>,
    Query(q): Query<MailContentQuery>,
) -> impl IntoResponse {
    let raw = tokio::task::spawn_blocking({
        let imap_state = state.imap.clone();
        let uid = uid.clone();
        let mailbox = q.mailbox.clone();
        move || imap_session::fetch_mail_raw_in_mailbox(&imap_state, account_id, &mailbox, &uid, q.uid_validity)
    })
    .await
    .ok()
    .flatten();

    if let Some(raw_bytes) = raw {
        if let Some((info, data)) =
            imap_session::find_attachment_bytes(&raw_bytes, attachment_index)
        {
            let escaped = info.filename.replace('"', "\\\"");
            let encoded = percent_encode_filename(&info.filename);
            let disposition =
                format!("attachment; filename=\"{escaped}\"; filename*=UTF-8''{encoded}");
            let response = Response::builder()
                .status(StatusCode::OK)
                .header(CONTENT_TYPE, info.content_type)
                .header(CONTENT_LENGTH, data.len().to_string())
                .header(CONTENT_DISPOSITION, disposition)
                .body(Body::from(data))
                .unwrap_or_else(|_| {
                    Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(Body::empty())
                        .unwrap()
                });
            return response;
        }
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Attachment not found"})),
        )
            .into_response();
    }

    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": "Mail not found"})),
    )
        .into_response()
}

pub async fn disconnect_imap(
    State(state): State<Arc<MailAppState>>,
    Path(account_id): Path<i64>,
) -> impl IntoResponse {
    tokio::task::spawn_blocking({
        let imap_state = state.imap.clone();
        move || imap_session::disconnect(&imap_state, account_id)
    })
    .await
    .ok();

    (StatusCode::OK, Json(json!({"status": "disconnected"}))).into_response()
}

fn percent_encode_filename(input: &str) -> String {
    input
        .as_bytes()
        .iter()
        .map(|&b| match b {
            0x21..=0x7e if b != b'%' && b != b'"' && b != b'\\' => (b as char).to_string(),
            _ => format!("%{:02X}", b),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reply_seed_from_raw_includes_reply_to() {
        let raw = concat!(
            "From: sender@example.com\r\n",
            "To: me@example.com\r\n",
            "Reply-To: reply@example.com\r\n",
            "Message-ID: <m1>\r\n",
            "\r\n",
            "Hello\r\n",
        );
        let seed = parse_reply_seed_from_raw(raw.as_bytes());
        assert_eq!(
            seed.get("reply_to").and_then(|v| v.as_str()).unwrap_or(""),
            "reply@example.com"
        );
        assert_eq!(
            seed.get("message_id")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            "<m1>"
        );
    }
}
