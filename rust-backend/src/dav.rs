//! Shared WebDAV plumbing behind the CalDAV (RFC 4791) and CardDAV (RFC 6352)
//! integrations — the transport, the tiny XML reader and the discovery chain that
//! [`crate::caldav_sync`] (events), [`crate::caldav_tasks`] (to-dos) and
//! [`crate::carddav_sync`] (contacts) all share.
//!
//! Three things are worth knowing:
//!
//! * **Redirects are followed by hand.** `.well-known/caldav` and friends answer
//!   with a 301, and a generic HTTP client turns a redirected PROPFIND into a GET
//!   (or drops its body), which silently breaks discovery. [`dav_request`] keeps
//!   the method and the body across hops.
//! * **Discovery accepts whatever the user typed.** A bare domain, an email
//!   address, a `.well-known` URL, a principal URL or a URL pointing straight at
//!   one collection all resolve, because each candidate base runs the standard
//!   `current-user-principal` → home-set → collection chain and the first base
//!   that yields collections wins.
//! * **The XML reader is deliberately small.** DAV bodies are namespaced
//!   (`D:`, `d:`, `cal:`, default, …) and we only ever pull a handful of leaf
//!   values out of well-formed server responses, so a namespace-agnostic scanner
//!   is enough and avoids a new dependency.

use reqwest::{header, Method, StatusCode, Url};

use crate::error::AppError;

/// Which DAV flavour a config talks — drives the well-known path and the wording
/// of user-facing errors.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DavKind {
    CalDav,
    CardDav,
}

impl DavKind {
    pub fn label(self) -> &'static str {
        match self {
            DavKind::CalDav => "CalDAV",
            DavKind::CardDav => "CardDAV",
        }
    }

    pub fn well_known(self) -> &'static str {
        match self {
            DavKind::CalDav => "/.well-known/caldav",
            DavKind::CardDav => "/.well-known/carddav",
        }
    }

    pub fn auth_error(self) -> AppError {
        AppError::BadRequest(format!(
            "{} rejected the credentials — check the server URL, username and (app-specific) password.",
            self.label()
        ))
    }
}

#[derive(Clone)]
pub struct DavConfig {
    pub kind: DavKind,
    pub url: String,
    pub username: String,
    pub password: String,
}

pub const XML_CT: &str = "application/xml; charset=utf-8";
pub const ICAL_CT: &str = "text/calendar; charset=utf-8";
pub const VCARD_CT: &str = "text/vcard; charset=utf-8";

const MAX_REDIRECTS: usize = 5;

pub fn propfind() -> Method {
    Method::from_bytes(b"PROPFIND").expect("valid method")
}

pub fn report() -> Method {
    Method::from_bytes(b"REPORT").expect("valid method")
}

/// A client for DAV traffic: redirects are disabled so [`dav_request`] can follow
/// them itself without losing the method or the request body.
pub fn dav_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

pub struct DavResp {
    pub status: StatusCode,
    pub etag: Option<String>,
    pub body: String,
}

/// Perform an authenticated DAV request and read the body, following redirects
/// with the method and body intact. `depth` sets the Depth header
/// (PROPFIND/REPORT); `body` is an XML/iCalendar/vCard payload.
///
/// A 401/403 is returned as a normal [`DavResp`] — callers decide whether that is
/// an authentication failure (see [`DavKind::auth_error`]) or an expected "no
/// access to this collection".
#[allow(clippy::too_many_arguments)]
pub async fn dav_request(
    http: &reqwest::Client,
    cfg: &DavConfig,
    method: Method,
    url: &str,
    depth: Option<&str>,
    content_type: Option<&str>,
    extra: &[(&str, &str)],
    body: Option<String>,
) -> Result<DavResp, AppError> {
    let mut target = Url::parse(url).map_err(|_| {
        AppError::BadRequest(format!("That {} URL is not valid.", cfg.kind.label()))
    })?;

    for _ in 0..=MAX_REDIRECTS {
        let mut req = http
            .request(method.clone(), target.clone())
            .basic_auth(&cfg.username, Some(&cfg.password));
        if let Some(d) = depth {
            req = req.header("Depth", d);
        }
        if let Some(ct) = content_type {
            req = req.header(header::CONTENT_TYPE, ct);
        }
        for (k, v) in extra {
            req = req.header(*k, *v);
        }
        if let Some(b) = &body {
            req = req.body(b.clone());
        }
        let resp = req.send().await.map_err(|e| {
            AppError::BadRequest(format!("{} request failed: {e}", cfg.kind.label()))
        })?;
        let status = resp.status();

        if status.is_redirection() {
            let location = resp
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            if let Some(next) = location.and_then(|loc| target.join(&loc).ok()) {
                if next != target {
                    target = next;
                    continue;
                }
            }
        }

        let etag = resp
            .headers()
            .get(header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let text = resp.text().await.unwrap_or_default();
        return Ok(DavResp { status, etag, body: text });
    }

    Err(AppError::BadRequest(format!(
        "{} kept redirecting — check the server URL.",
        cfg.kind.label()
    )))
}

pub fn is_dav_success(status: StatusCode) -> bool {
    status.is_success() || status == StatusCode::MULTI_STATUS
}

pub fn is_auth_status(status: StatusCode) -> bool {
    status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN
}

// ─────────────────────────── tiny XML reader ───────────────────────────
//
// Every element we extract is a leaf, or a container that never nests another
// element of the same local name — which is what makes "first matching close
// tag" correct here.

pub fn local_name(tag: &str) -> &str {
    let tag = tag.trim();
    let end = tag.find(|c: char| c.is_whitespace() || c == '/').unwrap_or(tag.len());
    let name = &tag[..end];
    match name.rfind(':') {
        Some(i) => &name[i + 1..],
        None => name,
    }
}

/// Inner content of the first element with local name `local` at/after `from`, plus
/// the byte index just past its close tag. Self-closing elements yield `""`.
pub fn elem_inner_from<'a>(xml: &'a str, local: &str, from: usize) -> Option<(&'a str, usize)> {
    let mut i = from;
    while let Some(rel) = xml[i..].find('<') {
        let lt = i + rel;
        let rel_gt = xml[lt..].find('>')?;
        let gt = lt + rel_gt;
        let tag = &xml[lt + 1..gt];
        if tag.starts_with('/') || tag.starts_with('?') || tag.starts_with('!') {
            i = gt + 1;
            continue;
        }
        if local_name(tag).eq_ignore_ascii_case(local) {
            if tag.ends_with('/') {
                return Some(("", gt + 1));
            }
            let inner_start = gt + 1;
            let mut j = inner_start;
            while let Some(crel) = xml[j..].find("</") {
                let clt = j + crel;
                let crel_gt = xml[clt..].find('>')?;
                let cgt = clt + crel_gt;
                if local_name(&xml[clt + 2..cgt]).eq_ignore_ascii_case(local) {
                    return Some((&xml[inner_start..clt], cgt + 1));
                }
                j = cgt + 1;
            }
            return None;
        }
        i = gt + 1;
    }
    None
}

pub fn elem_inner<'a>(xml: &'a str, local: &str) -> Option<&'a str> {
    elem_inner_from(xml, local, 0).map(|(s, _)| s)
}

pub fn elem_text(xml: &str, local: &str) -> Option<String> {
    elem_inner(xml, local).map(|s| xml_unescape(s.trim()))
}

/// Whether any element with local name `local` appears in `xml` (open or self-closing).
pub fn has_elem(xml: &str, local: &str) -> bool {
    let mut i = 0;
    while let Some(rel) = xml[i..].find('<') {
        let lt = i + rel;
        let Some(rel_gt) = xml[lt..].find('>') else { break };
        let gt = lt + rel_gt;
        let tag = &xml[lt + 1..gt];
        if !tag.starts_with('/')
            && !tag.starts_with('?')
            && !tag.starts_with('!')
            && local_name(tag).eq_ignore_ascii_case(local)
        {
            return true;
        }
        i = gt + 1;
    }
    false
}

/// Iterate the `<response>` blocks of a multistatus body (inner content of each).
pub fn responses(xml: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some((inner, end)) = elem_inner_from(xml, "response", i) {
        out.push(inner);
        i = end;
    }
    out
}

pub fn xml_unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        if let Some(semi) = after.find(';') {
            let ent = &after[1..semi];
            let repl = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                    u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32)
                }
                _ if ent.starts_with('#') => {
                    ent[1..].parse::<u32>().ok().and_then(char::from_u32)
                }
                _ => None,
            };
            if let Some(c) = repl {
                out.push(c);
                rest = &after[semi + 1..];
                continue;
            }
        }
        out.push('&');
        rest = &after[1..];
    }
    out.push_str(rest);
    out
}

// ─────────────────────────── discovery ───────────────────────────

/// A discovered collection: one calendar or one address book.
#[derive(Debug)]
pub struct Collection {
    pub url: Url,
    pub display_name: String,
    pub color: String,
}

/// What a discovery run is looking for. `component` restricts calendar
/// collections to those advertising an iCalendar component (VEVENT for the
/// calendar, VTODO for the task list); address books ignore it.
pub struct CollectionSpec {
    /// `resourcetype` child that marks a usable collection.
    pub resource_type: &'static str,
    /// Home-set container element (local name) inside the principal response.
    pub home_container: &'static str,
    /// PROPFIND body requesting the home set.
    pub home_body: &'static str,
    /// Depth:1 PROPFIND body listing the collections under the home set.
    pub list_body: &'static str,
    /// Depth:0 PROPFIND body probing a single collection.
    pub self_body: &'static str,
    pub component: Option<&'static str>,
}

const PRINCIPAL_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:current-user-principal/></d:prop></d:propfind>"#;

const CAL_HOME_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><c:calendar-home-set/></d:prop></d:propfind>"#;
const CAL_LIST_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/" xmlns:ic="http://apple.com/ns/ical/"><d:prop><d:resourcetype/><d:displayname/><cs:getctag/><c:supported-calendar-component-set/><ic:calendar-color/></d:prop></d:propfind>"#;
const CAL_SELF_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:ic="http://apple.com/ns/ical/"><d:prop><d:resourcetype/><d:displayname/><c:supported-calendar-component-set/><ic:calendar-color/></d:prop></d:propfind>"#;

const CARD_HOME_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:carddav"><d:prop><c:addressbook-home-set/></d:prop></d:propfind>"#;
const CARD_LIST_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/"><d:prop><d:resourcetype/><d:displayname/><cs:getctag/></d:prop></d:propfind>"#;
const CARD_SELF_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:displayname/></d:prop></d:propfind>"#;

/// Calendars that hold events.
pub const EVENT_CALENDARS: CollectionSpec = CollectionSpec {
    resource_type: "calendar",
    home_container: "calendar-home-set",
    home_body: CAL_HOME_BODY,
    list_body: CAL_LIST_BODY,
    self_body: CAL_SELF_BODY,
    component: Some("VEVENT"),
};

/// Calendars that hold to-dos (VTODO) — the same collections on most servers.
pub const TASK_CALENDARS: CollectionSpec = CollectionSpec {
    resource_type: "calendar",
    home_container: "calendar-home-set",
    home_body: CAL_HOME_BODY,
    list_body: CAL_LIST_BODY,
    self_body: CAL_SELF_BODY,
    component: Some("VTODO"),
};

/// Address books.
pub const ADDRESS_BOOKS: CollectionSpec = CollectionSpec {
    resource_type: "addressbook",
    home_container: "addressbook-home-set",
    home_body: CARD_HOME_BODY,
    list_body: CARD_LIST_BODY,
    self_body: CARD_SELF_BODY,
    component: None,
};

/// Candidate base URLs for whatever the user typed, most likely first.
///
/// A bare domain or an email address probes `.well-known` first (the standard
/// entry point) and then the site root; an explicit URL is tried verbatim first,
/// falling back to `.well-known` when it carries no path of its own.
pub fn base_candidates(raw: &str, kind: DavKind) -> Result<Vec<Url>, AppError> {
    let input = raw.trim();
    if input.is_empty() {
        return Err(AppError::BadRequest(format!(
            "Enter the {} server address.",
            kind.label()
        )));
    }
    let invalid = || AppError::BadRequest(format!("That {} address is not valid.", kind.label()));

    let mut out: Vec<Url> = Vec::new();
    let push = |u: Url, out: &mut Vec<Url>| {
        if !out.iter().any(|x| x == &u) {
            out.push(u);
        }
    };

    let lower = input.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        let url = Url::parse(input).map_err(|_| invalid())?;
        let bare_root = url.path() == "/" || url.path().is_empty();
        if !bare_root {
            push(url.clone(), &mut out);
        }
        if let Ok(wk) = url.join(kind.well_known()) {
            push(wk, &mut out);
        }
        if bare_root {
            push(url, &mut out);
        }
    } else {
        // "you@example.com", "example.com" or "example.com/dav/".
        let host = input.rsplit('@').next().unwrap_or(input).trim_matches('/');
        if host.is_empty() {
            return Err(invalid());
        }
        let url = Url::parse(&format!("https://{host}/")).map_err(|_| invalid())?;
        if let Ok(wk) = url.join(kind.well_known()) {
            push(wk, &mut out);
        }
        push(url, &mut out);
    }
    Ok(out)
}

/// Follow one `<href>` inside a single-property PROPFIND response. `Err` means the
/// server refused the credentials; `Ok(None)` means the property was absent.
async fn propfind_href(
    http: &reqwest::Client,
    cfg: &DavConfig,
    at: &Url,
    body: &str,
    container: &str,
) -> Result<Option<String>, AppError> {
    let resp = dav_request(http, cfg, propfind(), at.as_str(), Some("0"), Some(XML_CT), &[], Some(body.to_string())).await?;
    if is_auth_status(resp.status) {
        return Err(cfg.kind.auth_error());
    }
    if !is_dav_success(resp.status) {
        return Ok(None);
    }
    let Some(inner) = elem_inner(&resp.body, container) else { return Ok(None) };
    Ok(elem_text(inner, "href").filter(|s| !s.trim().is_empty()))
}

fn normalize_color(raw: Option<String>) -> String {
    let fallback = "#246bce".to_string();
    let Some(c) = raw else { return fallback };
    let c = c.trim();
    // Apple stores "#RRGGBBAA"; trim the alpha and validate.
    let hex = c.strip_prefix('#').unwrap_or(c);
    let hex: String = hex.chars().take(6).collect();
    if hex.len() == 6 && hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
        format!("#{hex}")
    } else {
        fallback
    }
}

fn name_from_href(url: &Url, fallback: &str) -> String {
    url.path_segments()
        .and_then(|mut segs| segs.rfind(|s| !s.is_empty()))
        .map(xml_unescape)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

/// Whether a listed collection matches the spec (right resourcetype, and — for
/// calendars — advertises the component we want). An absent component set means
/// "everything allowed", which is what RFC 4791 says.
fn matches_spec(response_xml: &str, spec: &CollectionSpec) -> bool {
    let rtype = elem_inner(response_xml, "resourcetype").unwrap_or("");
    if !has_elem(rtype, spec.resource_type) {
        return false;
    }
    match spec.component {
        None => true,
        Some(component) => elem_inner(response_xml, "supported-calendar-component-set")
            .map(|s| s.to_uppercase().contains(component))
            .unwrap_or(true),
    }
}

fn fallback_name(spec: &CollectionSpec) -> &'static str {
    match (spec.resource_type, spec.component) {
        ("addressbook", _) => "Contacts",
        (_, Some("VTODO")) => "Tasks",
        _ => "Calendar",
    }
}

/// List the collections under `home` (Depth 1) that match `spec`.
async fn list_collections(
    http: &reqwest::Client,
    cfg: &DavConfig,
    home: &Url,
    spec: &CollectionSpec,
) -> Result<Vec<Collection>, AppError> {
    let resp = dav_request(http, cfg, propfind(), home.as_str(), Some("1"), Some(XML_CT), &[], Some(spec.list_body.to_string())).await?;
    if is_auth_status(resp.status) {
        return Err(cfg.kind.auth_error());
    }
    if !is_dav_success(resp.status) {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for r in responses(&resp.body) {
        let Some(href) = elem_text(r, "href").filter(|s| !s.trim().is_empty()) else { continue };
        if !matches_spec(r, spec) {
            continue; // the home collection itself, the wrong component, …
        }
        let Ok(abs) = home.join(&href) else { continue };
        let display_name = elem_text(r, "displayname")
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| name_from_href(&abs, fallback_name(spec)));
        out.push(Collection {
            display_name,
            color: normalize_color(elem_text(r, "calendar-color")),
            url: abs,
        });
    }
    Ok(out)
}

/// Treat `at` itself as a collection if it is one (handles a URL that points
/// straight at a single calendar or address book).
async fn self_collection(
    http: &reqwest::Client,
    cfg: &DavConfig,
    at: &Url,
    spec: &CollectionSpec,
) -> Option<Collection> {
    let resp = dav_request(http, cfg, propfind(), at.as_str(), Some("0"), Some(XML_CT), &[], Some(spec.self_body.to_string()))
        .await
        .ok()?;
    if !is_dav_success(resp.status) {
        return None;
    }
    let r = responses(&resp.body).into_iter().next().unwrap_or(&resp.body);
    if !matches_spec(r, spec) {
        return None;
    }
    let display_name = elem_text(r, "displayname")
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| name_from_href(at, fallback_name(spec)));
    Some(Collection {
        display_name,
        color: normalize_color(elem_text(r, "calendar-color")),
        url: at.clone(),
    })
}

/// One discovery attempt from a single base URL: principal → home set →
/// collections, with the "URL points at one collection" fallbacks.
async fn discover_at(
    http: &reqwest::Client,
    cfg: &DavConfig,
    spec: &CollectionSpec,
    base: &Url,
) -> Result<Vec<Collection>, AppError> {
    let principal_url = propfind_href(http, cfg, base, PRINCIPAL_BODY, "current-user-principal")
        .await?
        .and_then(|h| base.join(&h).ok())
        .unwrap_or_else(|| base.clone());

    let home_url = propfind_href(http, cfg, &principal_url, spec.home_body, spec.home_container)
        .await?
        .and_then(|h| principal_url.join(&h).ok())
        .unwrap_or_else(|| principal_url.clone());

    let mut cols = list_collections(http, cfg, &home_url, spec).await?;
    if cols.is_empty() {
        if let Some(c) = self_collection(http, cfg, &home_url, spec).await {
            cols.push(c);
        } else if let Some(c) = self_collection(http, cfg, base, spec).await {
            cols.push(c);
        }
    }
    Ok(cols)
}

/// Full discovery: try each candidate base until one yields collections. Bad
/// credentials surface immediately instead of looking like "nothing found".
pub async fn discover(
    http: &reqwest::Client,
    cfg: &DavConfig,
    spec: &CollectionSpec,
) -> Result<Vec<Collection>, AppError> {
    let bases = base_candidates(&cfg.url, cfg.kind)?;
    let mut first_err: Option<AppError> = None;
    for base in bases {
        match discover_at(http, cfg, spec, &base).await {
            Ok(cols) if !cols.is_empty() => return Ok(cols),
            Ok(_) => {}
            Err(e) => {
                // Credentials are wrong everywhere — no point probing on.
                if matches!(&e, AppError::BadRequest(m) if m.contains("rejected the credentials")) {
                    return Err(e);
                }
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(Vec::new()),
    }
}

// ─────────────────────────── resource writes ───────────────────────────

/// PUT a resource (iCalendar or vCard) to `href`. `if_match` uses conditional
/// update semantics; `None` creates with `If-None-Match: *`. Returns the new ETag
/// when the server sends one.
pub async fn put_resource(
    http: &reqwest::Client,
    cfg: &DavConfig,
    href: &str,
    content_type: &str,
    payload: &str,
    if_match: Option<&str>,
) -> Result<Option<String>, AppError> {
    let extra: [(&str, &str); 1] = match if_match {
        Some(etag) => [("If-Match", etag)],
        None => [("If-None-Match", "*")],
    };
    let resp = dav_request(http, cfg, Method::PUT, href, None, Some(content_type), &extra, Some(payload.to_string())).await?;
    if is_auth_status(resp.status) {
        return Err(cfg.kind.auth_error());
    }
    if is_dav_success(resp.status) {
        return Ok(resp.etag);
    }
    // The ETag moved on (someone else wrote first) — take the remote's word for
    // its current state and overwrite unconditionally, honouring the local edit.
    if resp.status == StatusCode::PRECONDITION_FAILED && if_match.is_some() {
        let retry = dav_request(http, cfg, Method::PUT, href, None, Some(content_type), &[], Some(payload.to_string())).await?;
        if is_dav_success(retry.status) {
            return Ok(retry.etag);
        }
        return Err(AppError::BadRequest(format!(
            "{} rejected an item ({})",
            cfg.kind.label(),
            retry.status.as_u16()
        )));
    }
    Err(AppError::BadRequest(format!(
        "{} rejected an item ({})",
        cfg.kind.label(),
        resp.status.as_u16()
    )))
}

/// DELETE a resource, tolerating already-gone. On a precondition failure (the
/// remote changed since we fetched its ETag) retry unconditionally, honouring the
/// local tombstone.
pub async fn delete_resource(
    http: &reqwest::Client,
    cfg: &DavConfig,
    href: &str,
    if_match: Option<&str>,
) -> Result<(), AppError> {
    let extra: Vec<(&str, &str)> = if_match.map(|e| vec![("If-Match", e)]).unwrap_or_default();
    let resp = dav_request(http, cfg, Method::DELETE, href, None, None, &extra, None).await?;
    let gone = |s: StatusCode| s == StatusCode::NOT_FOUND || s == StatusCode::GONE;
    if is_dav_success(resp.status) || gone(resp.status) {
        return Ok(());
    }
    if is_auth_status(resp.status) {
        return Err(cfg.kind.auth_error());
    }
    if resp.status == StatusCode::PRECONDITION_FAILED && if_match.is_some() {
        let retry = dav_request(http, cfg, Method::DELETE, href, None, None, &[], None).await?;
        if is_dav_success(retry.status) || gone(retry.status) {
            return Ok(());
        }
        return Err(AppError::BadRequest(format!(
            "{} delete failed ({})",
            cfg.kind.label(),
            retry.status.as_u16()
        )));
    }
    Err(AppError::BadRequest(format!(
        "{} delete failed ({})",
        cfg.kind.label(),
        resp.status.as_u16()
    )))
}

/// Make a UID safe to use as the last path segment of a resource href.
pub fn sanitize_name(uid: &str, fallback: &str) -> String {
    let cleaned: String = uid
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_name_strips_prefix_and_attrs() {
        assert_eq!(local_name("D:href"), "href");
        assert_eq!(local_name("href"), "href");
        assert_eq!(local_name("cal:calendar-data xmlns=\"x\""), "calendar-data");
        assert_eq!(local_name("collection/"), "collection");
    }

    #[test]
    fn elem_text_reads_first_leaf() {
        let xml = r#"<d:response><d:href>/cal/1.ics</d:href><d:getetag>"abc"</d:getetag></d:response>"#;
        assert_eq!(elem_text(xml, "href").as_deref(), Some("/cal/1.ics"));
        assert_eq!(elem_text(xml, "getetag").as_deref(), Some("\"abc\""));
    }

    #[test]
    fn calendar_data_is_unescaped() {
        let xml = r#"<c:calendar-data>BEGIN:VEVENT&#13;
SUMMARY:A &amp; B&#13;
END:VEVENT</c:calendar-data>"#;
        let got = elem_text(xml, "calendar-data").unwrap();
        assert!(got.contains("SUMMARY:A & B"));
    }

    #[test]
    fn resourcetype_detects_collection_kinds() {
        let cal = r#"<d:response><d:resourcetype><d:collection/><c:calendar/></d:resourcetype></d:response>"#;
        assert!(matches_spec(cal, &EVENT_CALENDARS));
        assert!(!matches_spec(cal, &ADDRESS_BOOKS));
        let book = r#"<d:response><d:resourcetype><d:collection/><card:addressbook/></d:resourcetype></d:response>"#;
        assert!(matches_spec(book, &ADDRESS_BOOKS));
        assert!(!matches_spec(book, &EVENT_CALENDARS));
        let plain = r#"<d:response><d:resourcetype><d:collection/></d:resourcetype></d:response>"#;
        assert!(!matches_spec(plain, &EVENT_CALENDARS));
    }

    #[test]
    fn component_set_filters_calendars() {
        let events_only = r#"<d:response><d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
            <c:supported-calendar-component-set><c:comp name="VEVENT"/></c:supported-calendar-component-set></d:response>"#;
        assert!(matches_spec(events_only, &EVENT_CALENDARS));
        assert!(!matches_spec(events_only, &TASK_CALENDARS));
        let both = r#"<d:response><d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
            <c:supported-calendar-component-set><c:comp name="VEVENT"/><c:comp name="VTODO"/></c:supported-calendar-component-set></d:response>"#;
        assert!(matches_spec(both, &EVENT_CALENDARS));
        assert!(matches_spec(both, &TASK_CALENDARS));
        // Absent set ⇒ everything allowed.
        let unstated = r#"<d:response><d:resourcetype><d:collection/><c:calendar/></d:resourcetype></d:response>"#;
        assert!(matches_spec(unstated, &TASK_CALENDARS));
    }

    #[test]
    fn responses_are_split() {
        let xml = r#"<d:multistatus><d:response><d:href>/a/</d:href></d:response><d:response><d:href>/b/</d:href></d:response></d:multistatus>"#;
        let rs = responses(xml);
        assert_eq!(rs.len(), 2);
        assert_eq!(elem_text(rs[0], "href").as_deref(), Some("/a/"));
        assert_eq!(elem_text(rs[1], "href").as_deref(), Some("/b/"));
    }

    #[test]
    fn principal_href_is_nested() {
        let body = r#"<d:multistatus><d:response><d:href>/</d:href><d:propstat><d:prop><d:current-user-principal><d:href>/principals/u/</d:href></d:current-user-principal></d:prop></d:propstat></d:response></d:multistatus>"#;
        let inner = elem_inner(body, "current-user-principal").unwrap();
        assert_eq!(elem_text(inner, "href").as_deref(), Some("/principals/u/"));
    }

    #[test]
    fn color_normalizes_apple_argb() {
        assert_eq!(normalize_color(Some("#FF5733FF".into())), "#FF5733");
        assert_eq!(normalize_color(Some("nope".into())), "#246bce");
        assert_eq!(normalize_color(None), "#246bce");
    }

    #[test]
    fn sanitize_name_is_path_safe() {
        assert_eq!(sanitize_name("abc-123_def.ics", "x"), "abc-123_def.ics");
        assert_eq!(sanitize_name("a/b c:d", "x"), "a-b-c-d");
        assert_eq!(sanitize_name("///", "fallback"), "fallback");
    }

    #[test]
    fn bare_domain_probes_well_known_first() {
        let got = base_candidates("example.com", DavKind::CalDav).unwrap();
        assert_eq!(got[0].as_str(), "https://example.com/.well-known/caldav");
        assert_eq!(got[1].as_str(), "https://example.com/");
    }

    #[test]
    fn email_address_uses_its_domain() {
        let got = base_candidates("you@example.com", DavKind::CardDav).unwrap();
        assert_eq!(got[0].as_str(), "https://example.com/.well-known/carddav");
    }

    #[test]
    fn explicit_path_url_wins_over_well_known() {
        let got = base_candidates("https://cloud.example.com/remote.php/dav", DavKind::CalDav).unwrap();
        assert_eq!(got[0].as_str(), "https://cloud.example.com/remote.php/dav");
        assert!(got.iter().any(|u| u.path() == "/.well-known/caldav"));
    }

    #[test]
    fn root_url_tries_well_known_before_root() {
        let got = base_candidates("https://example.com/", DavKind::CalDav).unwrap();
        assert_eq!(got[0].as_str(), "https://example.com/.well-known/caldav");
        assert_eq!(got[1].as_str(), "https://example.com/");
    }

    #[test]
    fn empty_address_is_rejected() {
        assert!(base_candidates("   ", DavKind::CalDav).is_err());
    }

    // ── Transport + discovery against a stand-in server ──
    //
    // Covers the two things unit tests on strings can't: that a redirected
    // PROPFIND keeps its method and body (the `.well-known` hop every provider
    // starts with), and that principal → home-set → collection chaining resolves
    // relative hrefs the way real servers write them.

    use axum::{extract::Request, response::Response, routing::any, Router};

    const MULTISTATUS: &str = "application/xml; charset=utf-8";

    fn xml_response(body: &str) -> Response {
        Response::builder()
            .status(207)
            .header(header::CONTENT_TYPE, MULTISTATUS)
            .body(axum::body::Body::from(body.to_string()))
            .unwrap()
    }

    async fn mock_dav(req: Request) -> Response {
        let path = req.uri().path().to_string();
        let method = req.method().clone();
        let depth = req
            .headers()
            .get("Depth")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        // Every request must be authenticated and, for PROPFIND, carry its body.
        assert!(
            req.headers().contains_key(header::AUTHORIZATION),
            "missing basic auth on {method} {path}"
        );
        let body = axum::body::to_bytes(req.into_body(), 64 * 1024).await.unwrap();
        let body = String::from_utf8_lossy(&body).to_string();

        match path.as_str() {
            // The well-known entry point redirects, as providers really do.
            "/.well-known/caldav" => Response::builder()
                .status(301)
                .header(header::LOCATION, "/dav/")
                .body(axum::body::Body::empty())
                .unwrap(),
            "/dav/" => {
                assert_eq!(method.as_str(), "PROPFIND");
                assert!(body.contains("current-user-principal"), "body was dropped across the redirect: {body}");
                xml_response(
                    r#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/dav/</d:href><d:propstat><d:prop>
                       <d:current-user-principal><d:href>/dav/principals/me/</d:href></d:current-user-principal>
                       </d:prop></d:propstat></d:response></d:multistatus>"#,
                )
            }
            "/dav/principals/me/" => xml_response(
                r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:response><d:href>/dav/principals/me/</d:href>
                   <d:propstat><d:prop><c:calendar-home-set><d:href>/dav/calendars/me/</d:href></c:calendar-home-set>
                   </d:prop></d:propstat></d:response></d:multistatus>"#,
            ),
            "/dav/calendars/me/" => {
                assert_eq!(depth, "1", "collection listing must be Depth: 1");
                xml_response(
                    r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:ic="http://apple.com/ns/ical/">
                       <d:response><d:href>/dav/calendars/me/</d:href><d:propstat><d:prop>
                         <d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>
                       <d:response><d:href>/dav/calendars/me/work/</d:href><d:propstat><d:prop>
                         <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
                         <d:displayname>Work &amp; life</d:displayname>
                         <ic:calendar-color>#FF5733FF</ic:calendar-color>
                         <c:supported-calendar-component-set><c:comp name="VEVENT"/></c:supported-calendar-component-set>
                       </d:prop></d:propstat></d:response>
                       <d:response><d:href>/dav/calendars/me/chores/</d:href><d:propstat><d:prop>
                         <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
                         <d:displayname>Chores</d:displayname>
                         <c:supported-calendar-component-set><c:comp name="VTODO"/></c:supported-calendar-component-set>
                       </d:prop></d:propstat></d:response>
                       </d:multistatus>"#,
                )
            }
            _ => Response::builder()
                .status(404)
                .body(axum::body::Body::empty())
                .unwrap(),
        }
    }

    async fn spawn_mock() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().fallback(any(mock_dav));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://127.0.0.1:{port}/")
    }

    fn cfg_for(url: &str) -> DavConfig {
        DavConfig {
            kind: DavKind::CalDav,
            url: url.to_string(),
            username: "user".into(),
            password: "app-specific".into(),
        }
    }

    #[tokio::test]
    async fn discovery_follows_well_known_redirect_and_home_set() {
        let base = spawn_mock().await;
        let http = dav_client();
        let events = discover(&http, &cfg_for(&base), &EVENT_CALENDARS).await.unwrap();
        assert_eq!(events.len(), 1, "only the VEVENT calendar is an event collection");
        assert_eq!(events[0].display_name, "Work & life");
        assert_eq!(events[0].color, "#FF5733");
        assert!(events[0].url.as_str().ends_with("/dav/calendars/me/work/"));
    }

    #[tokio::test]
    async fn task_discovery_picks_the_vtodo_collection() {
        let base = spawn_mock().await;
        let http = dav_client();
        let tasks = discover(&http, &cfg_for(&base), &TASK_CALENDARS).await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].display_name, "Chores");
        // No calendar-color on this one ⇒ the shared default.
        assert_eq!(tasks[0].color, "#246bce");
    }

    #[tokio::test]
    async fn no_address_books_on_a_caldav_only_server() {
        let base = spawn_mock().await;
        let http = dav_client();
        let mut cfg = cfg_for(&base);
        cfg.kind = DavKind::CardDav;
        assert!(discover(&http, &cfg, &ADDRESS_BOOKS).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn bad_credentials_surface_as_an_auth_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().fallback(any(|| async {
            Response::builder()
                .status(401)
                .body(axum::body::Body::empty())
                .unwrap()
        }));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let http = dav_client();
        let err = discover(&http, &cfg_for(&format!("http://127.0.0.1:{port}/")), &EVENT_CALENDARS)
            .await
            .expect_err("401 must not look like an empty server");
        assert!(
            matches!(&err, AppError::BadRequest(m) if m.contains("rejected the credentials")),
            "unexpected error: {err}"
        );
    }
}
