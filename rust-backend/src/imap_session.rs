use std::{
    collections::HashMap,
    io::{Read, Write},
    net::TcpStream,
    sync::{Arc, Mutex},
};

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use chrono::{DateTime, Datelike, Duration, NaiveDate};
use native_tls::TlsConnector;
use tracing::{error, info, warn};

use crate::mail_models::{
    merge_mailbox_label_into_preview, AdvancedSearchRequest, AttachmentInfo, MailboxEntry,
    MailContent, MailPreview, MailSearchPreview, ReadStatus, SearchScope,
};
use mailparse::{
    addrparse_header, parse_headers, parse_mail, DispositionType, MailAddr, MailHeaderMap,
    ParsedMail,
};

fn parse_threading_headers(headers: &[mailparse::MailHeader<'_>]) -> (String, String, String) {
    let message_id = headers
        .get_first_value("Message-ID")
        .unwrap_or_default()
        .trim()
        .to_string();
    let in_reply_to = headers
        .get_first_value("In-Reply-To")
        .unwrap_or_default()
        .trim()
        .to_string();
    let references = headers
        .get_first_value("References")
        .unwrap_or_default()
        .trim()
        .to_string();
    (message_id, in_reply_to, references)
}

pub struct ImapState {
    sessions: Mutex<HashMap<i64, ImapSession>>,
}

impl ImapState {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }
}

pub enum ImapSession {
    Plain(imap::Session<TcpStream>),
    Tls(imap::Session<native_tls::TlsStream<TcpStream>>),
}

pub struct MoveError {
    pub message: String,
    pub outcome_uncertain: bool,
}

impl From<String> for MoveError {
    fn from(message: String) -> Self {
        Self {
            message,
            outcome_uncertain: false,
        }
    }
}

impl MoveError {
    fn after_mutation(error: imap::Error) -> Self {
        let outcome_uncertain = !matches!(
            error,
            imap::Error::No(_) | imap::Error::Bad(_) | imap::Error::Validate(_)
        );
        Self {
            message: error.to_string(),
            outcome_uncertain,
        }
    }
}

fn move_uid<T: Read + Write>(
    session: &mut imap::Session<T>,
    uid: &str,
    destination: &str,
) -> Result<(), MoveError> {
    let capabilities = session.capabilities().map_err(|error| error.to_string())?;
    if capabilities.has_str("MOVE") || capabilities.has_str("IMAP4rev2") {
        return session.uid_mv(uid, destination).map_err(MoveError::after_mutation);
    }
    if !capabilities.has_str("UIDPLUS") {
        return Err("Server does not support UID MOVE or UIDPLUS for a targeted move"
            .to_string()
            .into());
    }

    session
        .uid_copy(uid, quote_imap_string(destination))
        .map_err(MoveError::after_mutation)?;
    let finish = session
        .uid_store(uid, "+FLAGS.SILENT (\\Deleted)")
        .and_then(|_| session.uid_expunge(uid));
    finish.map_err(|error| MoveError {
        message: format!("IMAP copied the message, but source removal could not be confirmed: {error}"),
        outcome_uncertain: true,
    })?;
    Ok(())
}

fn delete_uid<T: Read + Write>(session: &mut imap::Session<T>, uid: &str) -> Result<(), String> {
    let capabilities = session.capabilities().map_err(|error| error.to_string())?;
    if !capabilities.has_str("UIDPLUS") && !capabilities.has_str("IMAP4rev2") {
        return Err("Server does not support targeted UID EXPUNGE".to_string());
    }

    session
        .uid_store(uid, "+FLAGS.SILENT (\\Deleted)")
        .map_err(|error| error.to_string())?;
    session.uid_expunge(uid).map_err(|error| error.to_string())?;
    Ok(())
}

impl ImapSession {
    fn noop(&mut self) -> Result<(), String> {
        let result = match self {
            ImapSession::Plain(s) => s.noop(),
            ImapSession::Tls(s) => s.noop(),
        };
        result.map(|_| ()).map_err(|e| format!("{e}"))
    }

    fn list_mailboxes(&mut self) -> Vec<String> {
        self.list_mailboxes_detailed()
            .into_iter()
            .map(|entry| entry.name)
            .collect()
    }

    /// List mailboxes together with their SPECIAL-USE role and selectability,
    /// so the UI can present Gmail's special folders like a native client does.
    fn list_mailboxes_detailed(&mut self) -> Vec<MailboxEntry> {
        let result = match self {
            ImapSession::Plain(s) => s.list(Some(""), Some("*")),
            ImapSession::Tls(s) => s.list(Some(""), Some("*")),
        };
        match result {
            Ok(names) => names
                .iter()
                .map(|n| {
                    let name = crate::imap_utf7::decode(n.name());
                    let mut role = None;
                    let mut selectable = true;
                    for attr in n.attributes() {
                        match attr {
                            imap::types::NameAttribute::NoSelect => selectable = false,
                            imap::types::NameAttribute::Custom(value)
                                if role.is_none() => {
                                    role = special_use_role(value);
                                }
                            _ => {}
                        }
                    }
                    // INBOX has no SPECIAL-USE flag but is always the inbox.
                    if role.is_none() && name.eq_ignore_ascii_case("INBOX") {
                        role = Some("inbox".to_string());
                    }
                    MailboxEntry {
                        name,
                        role,
                        selectable,
                    }
                })
                .collect(),
            Err(e) => {
                warn!("list_mailboxes error: {e}");
                vec![]
            }
        }
    }

    fn select_mailbox(&mut self, name: &str) -> Result<u32, String> {
        self.select_mailbox_info(name).map(|mailbox| mailbox.exists)
    }

    fn select_mailbox_info(&mut self, name: &str) -> Result<imap::types::Mailbox, String> {
        let name = crate::imap_utf7::encode(name);
        let mb = match self {
            ImapSession::Plain(s) => s.select(&name),
            ImapSession::Tls(s) => s.select(&name),
        };
        match mb {
            Ok(mb) => Ok(mb),
            Err(e) => Err(format!("{e}")),
        }
    }

    fn fetch_headers(&mut self, sequence: &str, uid_validity: Option<u32>) -> Vec<MailPreview> {
        let data = match self {
            ImapSession::Plain(s) => s.fetch(
                sequence,
                "(BODY.PEEK[HEADER.FIELDS (FROM TO SUBJECT DATE MESSAGE-ID IN-REPLY-TO REFERENCES CONTENT-TYPE IMPORTANCE X-PRIORITY KEYWORDS X-CATEGORY)] FLAGS UID RFC822.SIZE)",
            ),
            ImapSession::Tls(s) => s.fetch(
                sequence,
                "(BODY.PEEK[HEADER.FIELDS (FROM TO SUBJECT DATE MESSAGE-ID IN-REPLY-TO REFERENCES CONTENT-TYPE IMPORTANCE X-PRIORITY KEYWORDS X-CATEGORY)] FLAGS UID RFC822.SIZE)",
            ),
        };

        let fetches = match data {
            Ok(f) => f,
            Err(e) => {
                warn!("fetch_headers error: {e}");
                return vec![];
            }
        };

        let mut previews = Vec::new();
        for msg in fetches.iter() {
            let Some(uid) = msg.uid else {
                warn!("fetch_headers returned a message without UID");
                continue;
            };
            let uid = uid.to_string();
            let seen = msg
                .flags()
                .iter()
                .any(|f| matches!(f, imap::types::Flag::Seen));
            let flagged = msg
                .flags()
                .iter()
                .any(|f| matches!(f, imap::types::Flag::Flagged));

            let mut labels = Vec::new();
            for f in msg.flags().iter() {
                if let imap::types::Flag::Custom(s) = f {
                    let keyword = s.to_string();
                    if !labels.contains(&keyword) {
                        labels.push(keyword);
                    }
                }
            }

            if let Some(header_bytes) = msg.header() {
                let parsed_headers = match parse_headers(header_bytes) {
                    Ok((headers, _)) => headers,
                    Err(_) => Vec::new(),
                };

                let subject = parsed_headers
                    .get_first_value("Subject")
                    .unwrap_or_default();
                let from_raw = parsed_headers.get_first_value("From").unwrap_or_default();
                let recipient_to = parsed_headers.get_first_value("To").unwrap_or_default();
                let date = parsed_headers.get_first_value("Date").unwrap_or_default();
                let (message_id, in_reply_to, references) =
                    parse_threading_headers(&parsed_headers);
                let content_type = parse_content_type(
                    &parsed_headers
                        .get_first_value("Content-Type")
                        .unwrap_or_default(),
                );
                let importance = parse_importance_from_headers(&parsed_headers);
                let category = parse_category_from_headers(&parsed_headers);

                for h_label in parse_labels_from_headers(&parsed_headers) {
                    if !labels.contains(&h_label) {
                        labels.push(h_label);
                    }
                }
                let (name, address) = split_from(&from_raw);

                previews.push(MailPreview {
                    uid_validity,
                    id: uid,
                    message_id,
                    in_reply_to,
                    references,
                    name,
                    address,
                    subject,
                    date,
                    seen,
                    flagged,
                    recipient_to,
                    size: msg.size.unwrap_or(0) as usize,
                    importance,
                    content_type,
                    category,
                    labels,
                });
            }
        }
        previews
    }

    fn uid_fetch_raw_body(&mut self, uid: &str, query: &str) -> Option<Vec<u8>> {
        let data = match self {
            ImapSession::Plain(s) => s.uid_fetch(uid, query),
            ImapSession::Tls(s) => s.uid_fetch(uid, query),
        };
        let fetches = match data {
            Ok(fetches) => fetches,
            Err(e) => {
                warn!("uid_fetch raw body failed (uid={uid}, query={query}): {e}");
                return None;
            }
        };
        let expected_uid = uid.parse::<u32>().ok()?;
        let raw = fetches.iter().find_map(|msg| {
            if msg.uid == Some(expected_uid) {
                msg.body()
            } else {
                None
            }
        })?;
        Some(raw.to_vec())
    }

    fn fetch_mail_raw(&mut self, uid: &str) -> Option<Vec<u8>> {
        for query in ["(UID BODY.PEEK[] RFC822.SIZE)", "BODY.PEEK[]"] {
            if let Some(raw) = self.uid_fetch_raw_body(uid, query) {
                if !raw.is_empty() {
                    return Some(raw);
                }
            }
        }
        None
    }

    fn uid_store_flag(&mut self, uid: &str, flag: &str, add: bool) -> Result<(), String> {
        let op = if add {
            format!("+FLAGS ({flag})")
        } else {
            format!("-FLAGS ({flag})")
        };
        let res = match self {
            ImapSession::Plain(s) => s.uid_store(uid, &op),
            ImapSession::Tls(s) => s.uid_store(uid, &op),
        };
        res.map(|_| ()).map_err(|e| format!("{e}"))
    }

    fn uid_store_keyword(&mut self, uid: &str, keyword: &str, add: bool) -> Result<(), String> {
        let keyword = keyword.trim();
        if keyword.is_empty() {
            return Err("Label keyword cannot be empty".to_string());
        }

        if keyword.chars().any(|ch| {
            ch.is_whitespace() || matches!(ch, '(' | ')' | '{' | '%' | '*' | '"' | '\\' | ']')
        }) {
            return Err(format!(
                "Label \"{keyword}\" contains characters not supported by IMAP keywords"
            ));
        }

        let op = if add {
            format!("+FLAGS ({keyword})")
        } else {
            format!("-FLAGS ({keyword})")
        };
        let res = match self {
            ImapSession::Plain(s) => s.uid_store(uid, &op),
            ImapSession::Tls(s) => s.uid_store(uid, &op),
        };
        res.map(|_| ()).map_err(|e| format!("{e}"))
    }

    fn uid_move_to(&mut self, uid: &str, folder: &str) -> Result<(), MoveError> {
        let folder = crate::imap_utf7::encode(folder);
        match self {
            ImapSession::Plain(session) => move_uid(session, uid, &folder),
            ImapSession::Tls(session) => move_uid(session, uid, &folder),
        }
    }

    fn uid_delete(&mut self, uid: &str) -> Result<(), String> {
        match self {
            ImapSession::Plain(session) => delete_uid(session, uid),
            ImapSession::Tls(session) => delete_uid(session, uid),
        }
    }

    fn search_new_uids(&mut self, since_uid: u32) -> Vec<u32> {
        let Some(first_uid) = since_uid.checked_add(1) else {
            return vec![];
        };
        let query = format!("UID {first_uid}:*");
        match self.uid_search_query(&query) {
            Ok(mut uids) => {
                // IMAP sequence ranges are bidirectional. When first_uid is
                // beyond the highest UID, '*' can still match the last message.
                uids.retain(|uid| *uid >= first_uid);
                uids.reverse();
                uids
            }
            Err(error) => {
                warn!("UID SEARCH error: {error}");
                vec![]
            }
        }
    }

    fn uid_search_query(&mut self, query: &str) -> Result<Vec<u32>, String> {
        let result = match self {
            ImapSession::Plain(s) => s.uid_search(query),
            ImapSession::Tls(s) => s.uid_search(query),
        };
        match result {
            Ok(set) => {
                let mut v: Vec<u32> = set.into_iter().collect();
                v.sort_unstable();
                Ok(v)
            }
            Err(e) => Err(format!("{e}")),
        }
    }

    fn fetch_headers_by_uid_set(
        &mut self,
        uid_set: &str,
        uid_validity: Option<u32>,
        account_id: i64,
        mailbox: &str,
    ) -> Vec<MailPreview> {
        let data = match self {
            ImapSession::Plain(s) => s.uid_fetch(
                uid_set,
                "(BODY.PEEK[HEADER.FIELDS (FROM TO SUBJECT DATE MESSAGE-ID IN-REPLY-TO REFERENCES CONTENT-TYPE IMPORTANCE X-PRIORITY KEYWORDS X-CATEGORY)] FLAGS UID RFC822.SIZE)",
            ),
            ImapSession::Tls(s) => s.uid_fetch(
                uid_set,
                "(BODY.PEEK[HEADER.FIELDS (FROM TO SUBJECT DATE MESSAGE-ID IN-REPLY-TO REFERENCES CONTENT-TYPE IMPORTANCE X-PRIORITY KEYWORDS X-CATEGORY)] FLAGS UID RFC822.SIZE)",
            ),
        };

        let fetches = match data {
            Ok(f) => f,
            Err(e) => {
                warn!("uid_fetch error: {e}");
                return vec![];
            }
        };

        let requested: std::collections::HashSet<_> = uid_set.split(',').collect();
        let mut previews = Vec::new();
        for msg in fetches.iter() {
            let Some(uid) = msg.uid else {
                warn!(
                    "uid_fetch returned message without UID (account_id={}, mailbox={}, uid_set={}, seq={})",
                    account_id,
                    mailbox,
                    uid_set,
                    msg.message
                );
                continue;
            };
            let uid = uid.to_string();
            if !requested.contains(uid.as_str()) {
                continue;
            }
            let seen = msg
                .flags()
                .iter()
                .any(|f| matches!(f, imap::types::Flag::Seen));
            let flagged = msg
                .flags()
                .iter()
                .any(|f| matches!(f, imap::types::Flag::Flagged));

            let mut labels = Vec::new();
            for f in msg.flags().iter() {
                if let imap::types::Flag::Custom(s) = f {
                    let keyword = s.to_string();
                    if !labels.contains(&keyword) {
                        labels.push(keyword);
                    }
                }
            }

            if let Some(header_bytes) = msg.header() {
                let parsed_headers = match parse_headers(header_bytes) {
                    Ok((headers, _)) => headers,
                    Err(_) => Vec::new(),
                };

                let subject = parsed_headers
                    .get_first_value("Subject")
                    .unwrap_or_default();
                let from_raw = parsed_headers.get_first_value("From").unwrap_or_default();
                let recipient_to = parsed_headers.get_first_value("To").unwrap_or_default();
                let date = parsed_headers.get_first_value("Date").unwrap_or_default();
                let (message_id, in_reply_to, references) =
                    parse_threading_headers(&parsed_headers);
                let content_type = parse_content_type(
                    &parsed_headers
                        .get_first_value("Content-Type")
                        .unwrap_or_default(),
                );
                let importance = parse_importance_from_headers(&parsed_headers);
                let category = parse_category_from_headers(&parsed_headers);

                for h_label in parse_labels_from_headers(&parsed_headers) {
                    if !labels.contains(&h_label) {
                        labels.push(h_label);
                    }
                }
                let (name, address) = split_from(&from_raw);

                previews.push(MailPreview {
                    uid_validity,
                    id: uid,
                    message_id,
                    in_reply_to,
                    references,
                    name,
                    address,
                    subject,
                    date,
                    seen,
                    flagged,
                    recipient_to,
                    size: msg.size.unwrap_or(0) as usize,
                    importance,
                    content_type,
                    category,
                    labels,
                });
            }
        }
        previews
    }
}

#[cfg(test)]
mod threading_header_tests {
    use super::parse_threading_headers;
    use mailparse::parse_headers;

    #[test]
    fn extracts_message_id_reply_and_references() {
        let raw = b"Message-ID: <m1>\r\nIn-Reply-To: <m0>\r\nReferences: <m0> <m1>\r\n\r\n";
        let (headers, _) = parse_headers(raw).expect("parse_headers");
        let (message_id, in_reply_to, references) = parse_threading_headers(&headers);
        assert_eq!(message_id, "<m1>");
        assert_eq!(in_reply_to, "<m0>");
        assert_eq!(references, "<m0> <m1>");
    }
}

pub fn connect_and_login(
    state: &Arc<ImapState>,
    account_id: i64,
    email: &str,
    password: &str,
    imap_host: &str,
    imap_port: u16,
    ssl_mode: &str,
) -> Result<(), String> {
    let mut builder = TlsConnector::builder();
    builder.danger_accept_invalid_certs(true);
    builder.danger_accept_invalid_hostnames(true);
    let tls = builder.build().map_err(|e| format!("{e}"))?;

    let session = match ssl_mode.to_uppercase().as_str() {
        "SSL" => {
            let client = imap::connect((imap_host, imap_port), imap_host, &tls)
                .map_err(|e| format!("{e}"))?;
            let s = client
                .login(email, password)
                .map_err(|(e, _)| format!("{e}"))?;
            ImapSession::Tls(s)
        }
        "STARTTLS" => {
            let client = imap::connect_starttls((imap_host, imap_port), imap_host, &tls)
                .map_err(|e| format!("{e}"))?;
            let s = client
                .login(email, password)
                .map_err(|(e, _)| format!("{e}"))?;
            ImapSession::Tls(s)
        }
        _ => {
            let tcp = TcpStream::connect((imap_host, imap_port)).map_err(|e| format!("{e}"))?;

            tcp.set_read_timeout(Some(std::time::Duration::from_secs(30)))
                .ok();
            tcp.set_write_timeout(Some(std::time::Duration::from_secs(30)))
                .ok();

            let mut client = imap::Client::new(tcp);
            client.read_greeting().map_err(|e| format!("{e}"))?;
            let s = client
                .login(email, password)
                .map_err(|(e, _)| format!("{e}"))?;
            ImapSession::Plain(s)
        }
    };

    state.sessions.lock().unwrap().insert(account_id, session);
    info!("IMAP session opened for account {account_id}");
    Ok(())
}

/// Open an IMAP session authenticating with SASL `XOAUTH2` (Gmail / OAuth2).
/// `access_token` must be a currently-valid OAuth2 access token.
pub fn connect_and_login_xoauth2(
    state: &Arc<ImapState>,
    account_id: i64,
    email: &str,
    access_token: &str,
    imap_host: &str,
    imap_port: u16,
    ssl_mode: &str,
) -> Result<(), String> {
    let mut builder = TlsConnector::builder();
    builder.danger_accept_invalid_certs(true);
    builder.danger_accept_invalid_hostnames(true);
    let tls = builder.build().map_err(|e| format!("{e}"))?;

    let auth = crate::imap_client::XOAuth2 {
        user: email.to_string(),
        access_token: access_token.to_string(),
    };

    let session = match ssl_mode.to_uppercase().as_str() {
        "STARTTLS" => {
            let client = imap::connect_starttls((imap_host, imap_port), imap_host, &tls)
                .map_err(|e| format!("{e}"))?;
            let s = client
                .authenticate("XOAUTH2", &auth)
                .map_err(|(e, _)| format!("{e}"))?;
            ImapSession::Tls(s)
        }
        _ => {
            let client = imap::connect((imap_host, imap_port), imap_host, &tls)
                .map_err(|e| format!("{e}"))?;
            let s = client
                .authenticate("XOAUTH2", &auth)
                .map_err(|(e, _)| format!("{e}"))?;
            ImapSession::Tls(s)
        }
    };

    state.sessions.lock().unwrap().insert(account_id, session);
    info!("IMAP XOAUTH2 session opened for account {account_id}");
    Ok(())
}

#[cfg(test)]
mod special_use_tests {
    use super::special_use_role;

    #[test]
    fn maps_known_attributes() {
        assert_eq!(special_use_role("\\Sent").as_deref(), Some("sent"));
        assert_eq!(special_use_role("\\Trash").as_deref(), Some("trash"));
        assert_eq!(special_use_role("\\All").as_deref(), Some("all"));
        assert_eq!(special_use_role("\\Important").as_deref(), Some("important"));
        assert_eq!(special_use_role("\\Junk").as_deref(), Some("junk"));
        assert_eq!(special_use_role("\\Flagged").as_deref(), Some("flagged"));
    }

    #[test]
    fn ignores_non_special_use_attributes() {
        assert_eq!(special_use_role("\\HasNoChildren"), None);
        assert_eq!(special_use_role("\\Marked"), None);
    }
}

/// Map an IMAP SPECIAL-USE attribute (e.g. `\Sent`) to a short role name.
fn special_use_role(attr: &str) -> Option<String> {
    let role = match attr.trim_start_matches('\\').to_ascii_lowercase().as_str() {
        "sent" => "sent",
        "drafts" => "drafts",
        "trash" => "trash",
        "junk" => "junk",
        "all" => "all",
        "important" => "important",
        "flagged" => "flagged",
        "archive" => "archive",
        _ => return None,
    };
    Some(role.to_string())
}

pub fn list_mailboxes(state: &Arc<ImapState>, account_id: i64) -> Vec<String> {
    let mut sessions = state.sessions.lock().unwrap();
    match sessions.get_mut(&account_id) {
        Some(s) => s.list_mailboxes(),
        None => {
            warn!("No IMAP session for account {account_id}");
            vec![]
        }
    }
}

pub fn list_mailboxes_detailed(state: &Arc<ImapState>, account_id: i64) -> Vec<MailboxEntry> {
    let mut sessions = state.sessions.lock().unwrap();
    match sessions.get_mut(&account_id) {
        Some(s) => s.list_mailboxes_detailed(),
        None => {
            warn!("No IMAP session for account {account_id}");
            vec![]
        }
    }
}

pub fn fetch_mail_list(
    state: &Arc<ImapState>,
    account_id: i64,
    mailbox: &str,
    page: usize,
    per_page: usize,
) -> (usize, Vec<MailPreview>) {
    let mut sessions = state.sessions.lock().unwrap();
    let session = match sessions.get_mut(&account_id) {
        Some(s) => s,
        None => {
            warn!("No IMAP session for account {account_id}");
            return (0, vec![]);
        }
    };

    let selected = match session.select_mailbox_info(mailbox) {
        Ok(mailbox) => mailbox,
        Err(e) => {
            error!("select_mailbox {mailbox}: {e}");
            return (0, vec![]);
        }
    };

    let total = selected.exists as usize;
    if total == 0 {
        return (0, vec![]);
    }

    let start_from_end = (page.saturating_sub(1)) * per_page;
    if start_from_end >= total {
        return (total, vec![]);
    }

    let hi = total - start_from_end;
    let lo = if hi > per_page { hi - per_page + 1 } else { 1 };
    let sequence = format!("{lo}:{hi}");

    let mut previews = session.fetch_headers(&sequence, selected.uid_validity);

    fn date_ms(date_value: &str) -> i64 {
        DateTime::parse_from_rfc2822(date_value)
            .map(|dt| dt.timestamp_millis())
            .unwrap_or(0)
    }
    fn uid_num(id: &str) -> u32 {
        id.parse::<u32>().unwrap_or(0)
    }

    previews.sort_by(|a, b| {
        date_ms(&b.date)
            .cmp(&date_ms(&a.date))
            .then_with(|| uid_num(&b.id).cmp(&uid_num(&a.id)))
            .then_with(|| b.id.cmp(&a.id))
    });
    (total, previews)
}

fn quote_imap_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\r' | '\n' => out.push(' '),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn parse_ymd(value: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d").ok()
}

fn format_imap_date(date: NaiveDate) -> String {
    let month = match date.month() {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        12 => "Dec",
        _ => "Jan",
    };
    format!("{:02}-{month}-{}", date.day(), date.year())
}

pub fn build_imap_advanced_query(req: &AdvancedSearchRequest) -> String {
    let mut parts: Vec<String> = Vec::new();

    let from = req.from.as_deref().map(str::trim).filter(|v| !v.is_empty());
    let to = req.to.as_deref().map(str::trim).filter(|v| !v.is_empty());
    let cc = req.cc.as_deref().map(str::trim).filter(|v| !v.is_empty());
    let subject = req
        .subject
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let keywords = req
        .keywords
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());

    if let Some(value) = from {
        parts.push(format!("FROM {}", quote_imap_string(value)));
    }
    if let Some(value) = to {
        parts.push(format!("TO {}", quote_imap_string(value)));
    }
    if let Some(value) = cc {
        parts.push(format!("CC {}", quote_imap_string(value)));
    }
    if let Some(value) = subject {
        parts.push(format!("SUBJECT {}", quote_imap_string(value)));
    }
    if let Some(value) = keywords {
        parts.push(format!("TEXT {}", quote_imap_string(value)));
    }

    if let Some(start) = req
        .date_start
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .and_then(parse_ymd)
    {
        parts.push(format!("SENTSINCE {}", format_imap_date(start)));
    }

    if let Some(end) = req
        .date_end
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .and_then(parse_ymd)
        .and_then(|d| d.checked_add_signed(Duration::days(1)))
    {
        parts.push(format!("SENTBEFORE {}", format_imap_date(end)));
    }

    match req.read_status.unwrap_or(ReadStatus::All) {
        ReadStatus::Read => parts.push("SEEN".to_string()),
        ReadStatus::Unread => parts.push("UNSEEN".to_string()),
        ReadStatus::All => {}
    }

    if parts.is_empty() {
        "ALL".to_string()
    } else {
        parts.join(" ")
    }
}

fn matches_attachment_hint(content_type: &str) -> bool {
    let value = content_type.trim().to_ascii_lowercase();
    value.starts_with("multipart/mixed")
        || value.starts_with("multipart/related")
        || value.starts_with("multipart/report")
}

pub fn advanced_search(
    state: &Arc<ImapState>,
    account_id: i64,
    req: &AdvancedSearchRequest,
) -> Result<Vec<MailSearchPreview>, String> {
    let mut sessions = state.sessions.lock().unwrap();
    let session = sessions
        .get_mut(&account_id)
        .ok_or_else(|| format!("No IMAP session for account {account_id}"))?;

    let query = build_imap_advanced_query(req);

    let mut mailboxes: Vec<String> = match req.scope {
        SearchScope::All => session.list_mailboxes(),
        SearchScope::Mailboxes => req.mailboxes.clone(),
    };

    let mut seen = std::collections::HashSet::new();
    mailboxes.retain(|value| {
        if value.is_empty()
            || matches!(value.to_ascii_lowercase().as_str(), "folders" | "labels" | "[labels]")
        {
            return false;
        }
        seen.insert(crate::mail_models::mailbox_identity(value).to_owned())
    });

    let want_attachments = req.has_attachments;
    let mut results: Vec<MailSearchPreview> = Vec::new();
    let chunk_size = 200usize;

    for mailbox in mailboxes {
        let Ok(selected) = session.select_mailbox_info(&mailbox) else {
            continue;
        };

        let uids = match session.uid_search_query(&query) {
            Ok(v) => v,
            Err(e) => {
                warn!("advanced_search UID SEARCH error (mailbox={mailbox}): {e}");
                continue;
            }
        };

        if uids.is_empty() {
            continue;
        }

        for chunk in uids.chunks(chunk_size) {
            let uid_set = chunk
                .iter()
                .map(|u| u.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let mut previews = session.fetch_headers_by_uid_set(&uid_set, selected.uid_validity, account_id, &mailbox);

            for preview in previews.iter_mut() {
                merge_mailbox_label_into_preview(preview, &mailbox);
            }

            for preview in previews {
                if want_attachments && !matches_attachment_hint(&preview.content_type) {
                    continue;
                }
                results.push(MailSearchPreview {
                    mailbox: mailbox.clone(),
                    mail: preview,
                });
            }
        }
    }

    fn date_ms(date_value: &str) -> i64 {
        DateTime::parse_from_rfc2822(date_value)
            .map(|dt| dt.timestamp_millis())
            .unwrap_or(0)
    }
    fn uid_num(id: &str) -> u32 {
        id.parse::<u32>().unwrap_or(0)
    }

    results.sort_by(|a, b| {
        date_ms(&b.mail.date)
            .cmp(&date_ms(&a.mail.date))
            .then_with(|| uid_num(&b.mail.id).cmp(&uid_num(&a.mail.id)))
            .then_with(|| b.mail.id.cmp(&a.mail.id))
    });

    Ok(results)
}

pub fn fetch_mail_raw_in_mailbox(
    state: &Arc<ImapState>,
    account_id: i64,
    mailbox: &str,
    uid: &str,
) -> Option<Vec<u8>> {
    fetch_mail_raw_with_validity(state, account_id, mailbox, uid).map(|(raw, _)| raw)
}

pub fn fetch_mail_raw_with_validity(
    state: &Arc<ImapState>,
    account_id: i64,
    mailbox: &str,
    uid: &str,
) -> Option<(Vec<u8>, Option<u32>)> {
    let mut sessions = state.sessions.lock().unwrap();
    let session = sessions.get_mut(&account_id)?;
    let selected = session.select_mailbox_info(mailbox).ok()?;
    session.fetch_mail_raw(uid).map(|raw| (raw, selected.uid_validity))
}

pub fn fetch_new_uids_since(
    state: &Arc<ImapState>,
    account_id: i64,
    mailbox: &str,
    since_uid: u32,
) -> (usize, Vec<u32>) {
    let mut sessions = state.sessions.lock().unwrap();
    let session = match sessions.get_mut(&account_id) {
        Some(s) => s,
        None => {
            warn!("No IMAP session for account {account_id}");
            return (0, vec![]);
        }
    };

    let total = match session.select_mailbox(mailbox) {
        Ok(n) => n as usize,
        Err(e) => {
            error!("select_mailbox {mailbox}: {e}");
            return (0, vec![]);
        }
    };

    if total == 0 {
        return (0, vec![]);
    }

    let new_uids = session.search_new_uids(since_uid);
    (total, new_uids)
}

pub fn fetch_headers_for_uids(
    state: &Arc<ImapState>,
    account_id: i64,
    mailbox: &str,
    uids: &[u32],
) -> Vec<MailPreview> {
    if uids.is_empty() {
        return vec![];
    }
    let mut sessions = state.sessions.lock().unwrap();
    let session = match sessions.get_mut(&account_id) {
        Some(s) => s,
        None => return vec![],
    };

    let selected = match session.select_mailbox_info(mailbox) {
        Ok(selected) => selected,
        Err(error) => {
            warn!("Cannot fetch headers for the requested mailbox: {error}");
            return vec![];
        }
    };

    let uid_set: String = uids
        .iter()
        .map(|u| u.to_string())
        .collect::<Vec<_>>()
        .join(",");
    session.fetch_headers_by_uid_set(&uid_set, selected.uid_validity, account_id, mailbox)
}

pub fn parse_mail_content(uid: String, raw: &[u8]) -> MailContent {
    match parse_mail(raw) {
        Ok(parsed) => build_mail_content(uid, &parsed),
        Err(err) => {
            warn!(error = %err, "mailparse failed, falling back to basic parser");
            fallback_parse_rfc822(uid, raw)
        }
    }
}

pub fn parse_mail_content_with_attachment_data(uid: String, raw: &[u8]) -> MailContent {
    match parse_mail(raw) {
        Ok(parsed) => build_mail_content_with_options(uid, &parsed, true),
        Err(err) => {
            warn!(error = %err, "mailparse failed, falling back to basic parser");
            fallback_parse_rfc822(uid, raw)
        }
    }
}

pub fn find_attachment_bytes(
    raw: &[u8],
    attachment_index: usize,
) -> Option<(AttachmentInfo, Vec<u8>)> {
    let parsed = parse_mail(raw).ok()?;
    let descriptor = find_attachment_descriptor(&parsed, attachment_index)?;
    let bytes = descriptor.bytes?;
    Some((descriptor.info, bytes))
}

pub fn disconnect(state: &Arc<ImapState>, account_id: i64) {
    let mut sessions = state.sessions.lock().unwrap();
    if let Some(mut s) = sessions.remove(&account_id) {
        match &mut s {
            ImapSession::Plain(sess) => {
                let _ = sess.logout();
            }
            ImapSession::Tls(sess) => {
                let _ = sess.logout();
            }
        }
        info!("IMAP session closed for account {account_id}");
    }
}

pub fn is_connected(state: &Arc<ImapState>, account_id: i64) -> bool {
    let mut sessions = state.sessions.lock().unwrap();
    let health = match sessions.get_mut(&account_id) {
        Some(session) => session.noop(),
        None => return false,
    };

    if let Err(err) = health {
        warn!("IMAP session unhealthy for account {account_id}: {err}");
        sessions.remove(&account_id);
        return false;
    }

    true
}

pub enum QueuedMailAction<'a> {
    Seen(bool),
    Answered,
    Forwarded,
    Label(&'a str, bool),
    Move(&'a str),
    Delete,
}

pub struct QueuedMailError {
    pub message: String,
    pub retryable: bool,
}

impl From<String> for QueuedMailError {
    fn from(message: String) -> Self {
        Self { message, retryable: true }
    }
}

fn with_queued_mailbox<T>(
    state: &Arc<ImapState>,
    account_id: i64,
    mailbox: &str,
    expected_validity: Option<u32>,
    action: impl FnOnce(&mut ImapSession) -> Result<T, QueuedMailError>,
) -> Result<T, QueuedMailError> {
    let expected = expected_validity.filter(|value| *value > 0).ok_or_else(|| QueuedMailError {
        message: "Queued message has no UIDVALIDITY. Refresh the folder and repeat the action.".into(),
        retryable: false,
    })?;
    let mut sessions = state.sessions.lock().unwrap();
    let session = sessions.get_mut(&account_id)
        .ok_or_else(|| format!("No IMAP session for account {account_id}"))?;
    let selected = session.select_mailbox_info(mailbox)?;
    if selected.uid_validity != Some(expected) {
        return Err(QueuedMailError {
            message: "Mailbox UIDVALIDITY changed or is unavailable. Refresh the folder and repeat the action.".into(),
            retryable: false,
        });
    }
    action(session)
}

pub fn apply_queued_mail_action(
    state: &Arc<ImapState>,
    account_id: i64,
    mailbox: &str,
    uid: &str,
    expected_validity: Option<u32>,
    action: QueuedMailAction<'_>,
) -> Result<(), QueuedMailError> {
    with_queued_mailbox(state, account_id, mailbox, expected_validity, |session| {
        match action {
            QueuedMailAction::Seen(seen) => session.uid_store_flag(uid, "\\Seen", seen)?,
            QueuedMailAction::Answered => session.uid_store_flag(uid, "\\Answered", true)?,
            QueuedMailAction::Forwarded => session.uid_store_keyword(uid, "$Forwarded", true)?,
            QueuedMailAction::Label(label, add) => set_selected_mailbox_label(session, uid, label, add)?,
            QueuedMailAction::Move(destination) => session.uid_move_to(uid, destination).map_err(|error| {
                QueuedMailError { message: error.message, retryable: !error.outcome_uncertain }
            })?,
            QueuedMailAction::Delete => session.uid_delete(uid)?,
        }
        Ok(())
    })
}

pub fn fetch_queued_mail_raw(
    state: &Arc<ImapState>,
    account_id: i64,
    mailbox: &str,
    uid: &str,
    expected_validity: Option<u32>,
    cached: Option<Vec<u8>>,
) -> Result<Option<Vec<u8>>, QueuedMailError> {
    with_queued_mailbox(state, account_id, mailbox, expected_validity, |session| {
        Ok(cached.or_else(|| session.fetch_mail_raw(uid)))
    })
}

fn set_selected_mailbox_label(
    session: &mut ImapSession,
    uid: &str,
    label: &str,
    add: bool,
) -> Result<(), String> {
    session.uid_store_keyword(uid, label, add)?;

    if add {
        let label_folders = ["Labels", "Labels", "[Labels]"];
        let mut target_folder = None;
        let mailboxes = session.list_mailboxes();

        for root in label_folders {
            let candidate = format!("{}/{}", root, label);
            if mailboxes.iter().any(|m| m.eq_ignore_ascii_case(&candidate)) {
                target_folder = Some(candidate);
                break;
            }
        }

        if let Some(folder) = target_folder {
            let folder = crate::imap_utf7::encode(&folder);
            let _ = match session {
                ImapSession::Plain(s) => s.uid_copy(uid, &folder),
                ImapSession::Tls(s) => s.uid_copy(uid, &folder),
            };
        }
    } 

    Ok(())
}

pub fn create_mailbox(
    state: &Arc<ImapState>,
    account_id: i64,
    mailbox: &str,
) -> Result<(), String> {
    let mut sessions = state.sessions.lock().unwrap();
    let session = sessions
        .get_mut(&account_id)
        .ok_or_else(|| format!("No IMAP session for account {account_id}"))?;
    let mailbox = crate::imap_utf7::encode(mailbox);
    let result = match session {
        ImapSession::Plain(s) => s.create(&mailbox),
        ImapSession::Tls(s) => s.create(&mailbox),
    };
    result.map(|_| ()).map_err(|e| format!("{e}"))
}

pub fn delete_mail(
    state: &Arc<ImapState>,
    account_id: i64,
    mailbox: &str,
    uid: &str,
) -> Result<(), String> {
    let mut sessions = state.sessions.lock().unwrap();
    let session = sessions
        .get_mut(&account_id)
        .ok_or_else(|| format!("No IMAP session for account {account_id}"))?;
    session.select_mailbox(mailbox)?;
    session.uid_delete(uid)
}

pub fn append_draft(
    state: &Arc<ImapState>,
    account_id: i64,
    mailbox: &str,
    raw: &[u8],
    tracking_key: &str,
) -> Result<String, String> {
    let mut sessions = state.sessions.lock().unwrap();
    let session = sessions
        .get_mut(&account_id)
        .ok_or_else(|| format!("No IMAP session for account {account_id}"))?;

    let flags = [imap::types::Flag::Draft, imap::types::Flag::Seen];
    let wire_mailbox = crate::imap_utf7::encode(mailbox);
    let append_result = match session {
        ImapSession::Plain(s) => s.append_with_flags(&wire_mailbox, raw, &flags),
        ImapSession::Tls(s) => s.append_with_flags(&wire_mailbox, raw, &flags),
    };
    append_result.map_err(|e| format!("{e}"))?;

    session.select_mailbox(mailbox)?;
    let query = format!(
        "HEADER X-GUVERCIN-DRAFT-KEY {}",
        quote_imap_string(tracking_key),
    );
    let tracked_uids = session.uid_search_query(&query).unwrap_or_default();
    if let Some(uid) = tracked_uids.into_iter().max() {
        return Ok(uid.to_string());
    }

    let all_uids = session.uid_search_query("ALL")?;
    all_uids
        .into_iter()
        .max()
        .map(|uid| uid.to_string())
        .ok_or_else(|| "Draft append succeeded but appended UID could not be resolved".to_string())
}

fn build_mail_content(uid: String, parsed: &ParsedMail) -> MailContent {
    build_mail_content_with_options(uid, parsed, false)
}

fn build_mail_content_with_options(
    uid: String,
    parsed: &ParsedMail,
    include_attachment_data: bool,
) -> MailContent {
    let headers = parsed.get_headers();
    let subject = headers.get_first_value("Subject").unwrap_or_default();
    let date = headers.get_first_value("Date").unwrap_or_default();
    let (from_name, from_address) = extract_sender(parsed);
    let cc = headers.get_first_value("Cc").unwrap_or_default();
    let bcc = headers.get_first_value("Bcc").unwrap_or_default();

    let mut plain = None;
    let mut html = None;
    collect_text_parts(parsed, &mut plain, &mut html);

    MailContent {
        id: uid,
        subject,
        from_name,
        from_address,
        cc,
        bcc,
        date,
        html_body: html.unwrap_or_default(),
        plain_body: plain.unwrap_or_default(),
        attachments: collect_attachment_infos(parsed, include_attachment_data),
    }
}

fn collect_text_parts(part: &ParsedMail, plain: &mut Option<String>, html: &mut Option<String>) {
    if part.subparts.is_empty() {
        let mimetype = part.ctype.mimetype.to_lowercase();
        if mimetype == "text/plain" && plain.is_none() {
            if let Ok(body) = part.get_body() {
                *plain = Some(body);
            }
        } else if mimetype == "text/html" && html.is_none() {
            if let Ok(body) = part.get_body() {
                *html = Some(body);
            }
        }
    } else {
        for child in &part.subparts {
            collect_text_parts(child, plain, html);
        }
    }
}

fn extract_sender(parsed: &ParsedMail) -> (String, String) {
    if let Some(header) = parsed.get_headers().get_first_header("From") {
        if let Ok(entries) = addrparse_header(header) {
            for addr in entries.iter() {
                if let MailAddr::Single(info) = addr {
                    return (
                        info.display_name.clone().unwrap_or_default(),
                        info.addr.clone(),
                    );
                }
            }
        }
        return (String::new(), header.get_value());
    }
    (String::new(), String::new())
}

fn collect_attachment_infos(parsed: &ParsedMail, include_bytes: bool) -> Vec<AttachmentInfo> {
    let mut attachments = Vec::new();
    iterate_attachment_parts(parsed, &mut |part, idx| {
        if let Some(mut desc) = build_attachment_descriptor(part, idx, include_bytes) {
            if include_bytes {
                desc.info.data_base64 = desc.bytes.map(|bytes| BASE64_STANDARD.encode(bytes));
            }
            attachments.push(desc.info);
        }
        false
    });
    attachments
}

fn find_attachment_descriptor(
    parsed: &ParsedMail,
    target_index: usize,
) -> Option<AttachmentDescriptor> {
    let mut descriptor = None;
    iterate_attachment_parts(parsed, &mut |part, idx| {
        if idx == target_index {
            if let Some(desc) = build_attachment_descriptor(part, idx, true) {
                descriptor = Some(desc);
                return true;
            }
        }
        false
    });
    descriptor
}

fn iterate_attachment_parts<F>(part: &ParsedMail, visitor: &mut F) -> bool
where
    F: FnMut(&ParsedMail, usize) -> bool,
{
    fn recurse<F>(part: &ParsedMail, counter: &mut usize, visitor: &mut F) -> bool
    where
        F: FnMut(&ParsedMail, usize) -> bool,
    {
        if part.subparts.is_empty() {
            if should_treat_as_attachment(part) {
                let idx = *counter;
                *counter += 1;
                if visitor(part, idx) {
                    return true;
                }
            }
        } else {
            for child in &part.subparts {
                if recurse(child, counter, visitor) {
                    return true;
                }
            }
        }
        false
    }

    let mut counter = 0;
    recurse(part, &mut counter, visitor)
}

fn should_treat_as_attachment(part: &ParsedMail) -> bool {
    if !part.subparts.is_empty() {
        return false;
    }
    let disposition = part.get_content_disposition();
    let mimetype = part.ctype.mimetype.to_lowercase();
    let has_filename =
        disposition.params.contains_key("filename") || part.ctype.params.contains_key("name");

    if matches!(disposition.disposition, DispositionType::Attachment) {
        return true;
    }
    if has_filename {
        return true;
    }
    if mimetype.starts_with("text/") {
        return false;
    }
    if mimetype == "message/rfc822" {
        return false;
    }
    true
}

fn build_attachment_descriptor(
    part: &ParsedMail,
    index: usize,
    include_bytes: bool,
) -> Option<AttachmentDescriptor> {
    let disposition = part.get_content_disposition();
    let filename = disposition
        .params
        .get("filename")
        .cloned()
        .or_else(|| part.ctype.params.get("name").cloned())
        .unwrap_or_else(|| format!("attachment-{}", index + 1));

    let body = part.get_body_raw().ok()?;
    let size = body.len();
    let content_type = part.ctype.mimetype.clone();
    let is_inline = matches!(disposition.disposition, DispositionType::Inline);
    let content_id = part
        .get_headers()
        .get_first_value("Content-ID")
        .map(|value| value.trim().trim_matches('<').trim_matches('>').to_string())
        .filter(|value| !value.is_empty());
    let bytes = if include_bytes { Some(body) } else { None };

    Some(AttachmentDescriptor {
        info: AttachmentInfo {
            id: index.to_string(),
            filename,
            content_type,
            size,
            is_inline,
            data_base64: None,
            content_id,
        },
        bytes,
    })
}

fn fallback_parse_rfc822(uid: String, raw: &[u8]) -> MailContent {
    let text = String::from_utf8_lossy(raw);
    let (header_part, body_part) = if let Some(idx) = text.find("\r\n\r\n") {
        (&text[..idx], &text[idx + 4..])
    } else if let Some(idx) = text.find("\n\n") {
        (&text[..idx], &text[idx + 2..])
    } else {
        (text.as_ref(), "")
    };

    let subject = parse_header(header_part, "Subject");
    let from_raw = parse_header(header_part, "From");
    let (from_name, from_address) = split_from(&from_raw);
    let cc = parse_header(header_part, "Cc");
    let bcc = parse_header(header_part, "Bcc");
    let date = parse_header(header_part, "Date");

    MailContent {
        id: uid,
        subject,
        from_name,
        from_address,
        cc,
        bcc,
        date,
        html_body: String::new(),
        plain_body: body_part.chars().take(10000).collect(),
        attachments: vec![],
    }
}

struct AttachmentDescriptor {
    info: AttachmentInfo,
    bytes: Option<Vec<u8>>,
}

fn parse_header(headers: &str, name: &str) -> String {
    match parse_headers(headers.as_bytes()) {
        Ok((parsed, _)) => parsed.get_first_value(name).unwrap_or_default(),
        Err(_) => String::new(),
    }
}

fn parse_content_type(value: &str) -> String {
    value
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

fn parse_importance_from_headers(headers: &[mailparse::MailHeader]) -> i32 {
    let importance = headers
        .get_first_value("Importance")
        .unwrap_or_default()
        .to_ascii_lowercase();
    match importance.as_str() {
        "high" => 2,
        "normal" => 1,
        "low" => 0,
        _ => {
            let x_priority = headers.get_first_value("X-Priority").unwrap_or_default();
            let digit = x_priority
                .chars()
                .find(|ch| ch.is_ascii_digit())
                .and_then(|ch| ch.to_digit(10))
                .unwrap_or(3);
            match digit {
                1 | 2 => 2,
                3 => 1,
                4 | 5 => 0,
                _ => 1,
            }
        }
    }
}

fn parse_category_from_headers(headers: &[mailparse::MailHeader]) -> String {
    let x_category = headers.get_first_value("X-Category").unwrap_or_default();
    if !x_category.is_empty() {
        return x_category;
    }
    headers.get_first_value("Keywords").unwrap_or_default()
}

fn parse_labels_from_headers(headers: &[mailparse::MailHeader]) -> Vec<String> {
    let keywords = headers.get_first_value("Keywords").unwrap_or_default();
    if !keywords.is_empty() {
        let mut labels = Vec::new();
        for part in keywords.split(',') {
            let label = part.trim();
            if label.is_empty() {
                continue;
            }
            if !labels.iter().any(|existing| existing == label) {
                labels.push(label.to_string());
            }
        }
        if !labels.is_empty() {
            return labels;
        }
    }

    let category = headers.get_first_value("X-Category").unwrap_or_default();
    if category.is_empty() {
        return Vec::new();
    }

    vec![category]
}



#[cfg(test)]
fn parse_labels(headers: &str) -> Vec<String> {
    match parse_headers(headers.as_bytes()) {
        Ok((parsed, _)) => parse_labels_from_headers(&parsed),
        Err(_) => Vec::new(),
    }
}

fn split_from(from: &str) -> (String, String) {
    if let (Some(lt), Some(gt)) = (from.find('<'), from.find('>')) {
        let name = from[..lt].trim().trim_matches('"').to_string();
        let address = from[lt + 1..gt].trim().to_string();
        (name, address)
    } else {
        (String::new(), from.trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{build_imap_advanced_query, parse_labels};
    use crate::mail_models::{AdvancedSearchRequest, ReadStatus};

    #[test]
    fn imap_move_only_expunges_the_target_uid() {
        for (capability, expected_commands, succeeds) in [
            ("MOVE", vec!["UID MOVE 42 \"Project Archive\""], true),
            (
                "UIDPLUS",
                vec![
                    "UID COPY 42 \"Project Archive\"",
                    "UID STORE 42 +FLAGS.SILENT (\\Deleted)",
                    "UID EXPUNGE 42",
                ],
                true,
            ),
            ("", vec![], false),
        ] {
            let commands = record_imap_commands(capability, None, |mut session| {
                assert_eq!(super::move_uid(&mut session, "42", "Project Archive").is_ok(), succeeds);
            });
            assert_eq!(commands, expected_commands);
        }
    }

    #[test]
    fn imap_delete_only_expunges_the_target_uid() {
        let commands = record_imap_commands("UIDPLUS", None, |mut session| {
            super::delete_uid(&mut session, "42").unwrap();
        });
        assert_eq!(commands, ["UID STORE 42 +FLAGS.SILENT (\\Deleted)", "UID EXPUNGE 42"]);
        let commands = record_imap_commands("", None, |mut session| {
            assert!(super::delete_uid(&mut session, "42").is_err());
        });
        assert!(commands.is_empty());
    }

    #[test]
    fn uidplus_move_reports_a_partial_copy_when_source_removal_fails() {
        let commands = record_imap_commands("UIDPLUS", Some("UID STORE"), |mut session| {
            let error = super::move_uid(&mut session, "42", "Archive").err().unwrap();
            assert!(error.outcome_uncertain);
            assert!(error.message.contains("copied the message"));
        });
        assert_eq!(commands, [
            "UID COPY 42 \"Archive\"",
            "UID STORE 42 +FLAGS.SILENT (\\Deleted)",
        ]);
    }

    #[test]
    fn rejected_mailbox_selection_stops_the_header_fetch() {
        let commands = record_imap_commands("UIDPLUS", Some("SELECT"), |session| {
            let state = std::sync::Arc::new(super::ImapState::new());
            state.sessions.lock().unwrap().insert(1, super::ImapSession::Plain(session));
            assert!(super::fetch_headers_for_uids(&state, 1, "Missing folder", &[42]).is_empty());
        });
        assert_eq!(commands, ["SELECT \"Missing folder\""]);
    }

    #[test]
    fn incremental_uid_search_ignores_the_old_last_uid_and_handles_exhaustion() {
        let commands = record_imap_commands("UIDPLUS", None, |session| {
            let mut session = super::ImapSession::Plain(session);
            assert!(session.search_new_uids(42).is_empty());
            assert_eq!(session.search_new_uids(40), vec![42, 41]);
            assert!(session.search_new_uids(u32::MAX).is_empty());
        });
        assert_eq!(commands, ["UID SEARCH UID 43:*", "UID SEARCH UID 41:*"]);
    }

    #[test]
    fn queued_actions_refuse_reused_uids_and_accept_the_original_generation() {
        use super::{apply_queued_mail_action, ImapSession, ImapState, QueuedMailAction};
        let commands = record_imap_commands("UIDPLUS", None, |session| {
            let state = std::sync::Arc::new(ImapState::new());
            state.sessions.lock().unwrap().insert(1, ImapSession::Plain(session));
            for stale in [None, Some(6)] {
                let error = apply_queued_mail_action(
                    &state, 1, "INBOX", "42", stale, QueuedMailAction::Delete,
                ).err().unwrap();
                assert!(!error.retryable);
                assert!(error.message.contains("UIDVALIDITY"));
            }
            apply_queued_mail_action(
                &state, 1, "INBOX", "42", Some(7), QueuedMailAction::Seen(true),
            ).ok().unwrap();
        });
        assert_eq!(commands, [
            "SELECT \"INBOX\"",
            "SELECT \"INBOX\"",
            "UID STORE 42 +FLAGS (\\Seen)",
        ]);
    }

    fn record_imap_commands(
        capability: &'static str,
        reject_prefix: Option<&'static str>,
        action: impl FnOnce(imap::Session<std::net::TcpStream>),
    ) -> Vec<String> {
        use std::io::{BufRead, BufReader, Write};
        use std::net::{TcpListener, TcpStream};
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            write!(stream, "* OK test server\r\n").unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut commands = Vec::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let (tag, command) = line.trim_end().split_once(' ').unwrap();
                match command {
                    "CAPABILITY" => {
                        let capabilities = format!("IMAP4rev1 {capability}");
                        write!(stream, "* CAPABILITY {}\r\n", capabilities.trim_end()).unwrap()
                    }
                    "LOGOUT" => {
                        write!(stream, "* BYE goodbye\r\n{tag} OK logout\r\n").unwrap();
                        break;
                    }
                    cmd if cmd.starts_with("LOGIN ") => {}
                    cmd => {
                        commands.push(cmd.to_string());
                        if cmd.starts_with("UID SEARCH ") {
                            write!(stream, "* SEARCH 41 42\r\n").unwrap();
                        }
                        if reject_prefix.is_some_and(|prefix| cmd.starts_with(prefix)) {
                            write!(stream, "{tag} NO fixture rejection\r\n").unwrap();
                            continue;
                        }
                        if cmd.starts_with("SELECT ") {
                            write!(stream, "* 1 EXISTS\r\n* OK [UIDVALIDITY 7] fixture\r\n").unwrap();
                        }
                    }
                }
                write!(stream, "{tag} OK success\r\n").unwrap();
            }
            commands
        });
        let stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut client = imap::Client::new(stream);
        client.read_greeting().unwrap();
        let session = client.login("test", "test").unwrap();
        action(session);
        server.join().unwrap()
    }

    #[test]
    fn parse_labels_splits_and_dedupes_keywords() {
        let labels = parse_labels("Keywords: Work, Urgent, Work,  , Clients\r\n");
        assert_eq!(labels, vec!["Work", "Urgent", "Clients"]);
    }

    #[test]
    fn parse_labels_falls_back_to_x_category() {
        let labels = parse_labels("X-Category: FollowUp\r\n");
        assert_eq!(labels, vec!["FollowUp"]);
    }

    #[test]
    fn parse_labels_returns_empty_without_keywords_or_category() {
        let labels = parse_labels("Subject: Hello\r\n");
        assert!(labels.is_empty());
    }

    #[test]
    fn build_imap_advanced_query_defaults_to_all() {
        let req = AdvancedSearchRequest::default();
        assert_eq!(build_imap_advanced_query(&req), "ALL");
    }

    #[test]
    fn build_imap_advanced_query_adds_unseen() {
        let req = AdvancedSearchRequest {
            read_status: Some(ReadStatus::Unread),
            ..Default::default()
        };
        assert_eq!(build_imap_advanced_query(&req), "UNSEEN");
    }

    #[test]
    fn build_imap_advanced_query_formats_date_range_inclusive_end() {
        let req = AdvancedSearchRequest {
            date_start: Some("2026-03-01".to_string()),
            date_end: Some("2026-03-09".to_string()),
            ..Default::default()
        };
        assert_eq!(
            build_imap_advanced_query(&req),
            "SENTSINCE 01-Mar-2026 SENTBEFORE 10-Mar-2026"
        );
    }

    #[test]
    fn build_imap_advanced_query_escapes_quotes_and_backslashes() {
        let req = AdvancedSearchRequest {
            subject: Some(r#"Hello "world" \\ test"#.to_string()),
            ..Default::default()
        };
        assert_eq!(
            build_imap_advanced_query(&req),
            r#"SUBJECT "Hello \"world\" \\\\ test""#
        );
    }
}
