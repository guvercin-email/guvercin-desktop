use std::{
    collections::{HashMap, VecDeque},
    io::{self, BufRead, BufReader, Read, Write},
    net::TcpStream,
    sync::{Arc, Mutex},
};

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use chrono::{DateTime, Datelike, Duration, NaiveDate};
use native_tls::TlsConnector;
use tracing::{info, warn};

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

type SharedSession = Arc<Mutex<ImapSession>>;

pub struct ImapState {
    sessions: Mutex<HashMap<i64, SharedSession>>,
}

impl ImapState {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn session(&self, account_id: i64) -> Option<SharedSession> {
        self.sessions.lock().unwrap().get(&account_id).cloned()
    }

    fn insert_session(&self, account_id: i64, session: ImapSession) {
        self.sessions.lock().unwrap().insert(account_id, Arc::new(Mutex::new(session)));
    }

}

const UTF8_UID_SEARCH: &str = "UID SEARCH CHARSET UTF-8 ";

/// The IMAP client accepts a complete SEARCH command but provides no API for
/// synchronizing its literals. Hold only that generated command at the write
/// boundary so each literal can wait for the server's continuation.
#[derive(Debug)]
pub(crate) struct ImapTransport<T> {
    io: BufReader<T>,
    search: Vec<u8>,
    responses: VecDeque<u8>,
    search_failed: bool,
}

impl<T: Read + Write> ImapTransport<T> {
    fn new(io: T) -> Self {
        Self {
            io: BufReader::new(io),
            search: Vec::new(),
            responses: VecDeque::new(),
            search_failed: false,
        }
    }

    fn flush_search(&mut self, search: &[u8]) -> io::Result<()> {
        let tag = search.split(|byte| *byte == b' ').next().unwrap_or_default();
        let mut lines = search.split_inclusive(|byte| *byte == b'\n').peekable();
        let mut response = Vec::new();

        while let Some(line) = lines.next() {
            self.io.get_mut().write_all(line)?;
            self.io.get_mut().flush()?;
            if lines.peek().is_none() {
                break;
            }

            loop {
                response.clear();
                self.io.by_ref().take(65_536).read_until(b'\n', &mut response)?;
                if !response.ends_with(b"\r\n") {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Incomplete IMAP literal continuation"));
                }
                if response.starts_with(b"+") {
                    break;
                }

                // Pass unsolicited responses and any tagged rejection to the
                // IMAP parser. No literal follows a tagged rejection.
                if self.responses.len() + response.len() > 1024 * 1024 {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "Too much IMAP data before the literal continuation"));
                }
                self.responses.extend(&response);
                if response.split(|byte| *byte == b' ').next() == Some(tag)
                    || response.starts_with(b"* BYE")
                {
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}

impl<T: Read> Read for ImapTransport<T> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.responses.is_empty() {
            self.io.read(output)
        } else {
            self.responses.read(output)
        }
    }
}

impl<T: Read + Write> Write for ImapTransport<T> {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        if self.search_failed {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "IMAP literal exchange was interrupted"));
        }
        let mut words = input.splitn(2, |byte| *byte == b' ');
        let tagged = words.next().and_then(|tag| tag.strip_prefix(b"a"))
            .is_some_and(|digits| !digits.is_empty() && digits.iter().all(u8::is_ascii_digit));
        let utf8_search = tagged && words.next()
            .is_some_and(|command| command.starts_with(UTF8_UID_SEARCH.as_bytes()));
        if !self.search.is_empty() || utf8_search {
            self.search.extend_from_slice(input);
            Ok(input.len())
        } else {
            self.io.get_mut().write(input)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.search.is_empty() {
            self.io.get_mut().flush()
        } else {
            let search = std::mem::take(&mut self.search);
            let result = self.flush_search(&search);
            self.search_failed = result.is_err();
            result
        }
    }
}

impl imap::extensions::idle::SetReadTimeout for ImapTransport<TcpStream> {
    fn set_read_timeout(&mut self, timeout: Option<std::time::Duration>) -> imap::error::Result<()> {
        TcpStream::set_read_timeout(self.io.get_mut(), timeout.or(Some(std::time::Duration::from_secs(15))))
            .map_err(imap::Error::Io)
    }
}

impl imap::extensions::idle::SetReadTimeout for ImapTransport<native_tls::TlsStream<TcpStream>> {
    fn set_read_timeout(&mut self, timeout: Option<std::time::Duration>) -> imap::error::Result<()> {
        self.io.get_mut().get_ref().set_read_timeout(timeout.or(Some(std::time::Duration::from_secs(15))))
            .map_err(imap::Error::Io)
    }
}

pub(crate) fn wait_for_mailbox_change(state: &Arc<ImapState>, account_id: i64, mailbox: &str) -> Result<Option<bool>, String> {
    use imap::extensions::idle::{SetReadTimeout, WaitOutcome};

    fn wait<T: Read + Write + SetReadTimeout>(session: &mut imap::Session<T>, mailbox: &str) -> Result<Option<bool>, String> {
        let capabilities = session.capabilities().map_err(|error| error.to_string())?;
        if !capabilities.has_str("IDLE") {
            return Ok(None);
        }
        session.examine(&crate::imap_utf7::encode(mailbox)).map_err(|error| error.to_string())?;
        let outcome = session.idle().timeout(std::time::Duration::from_secs(45)).keepalive(false)
            .wait_while(imap::extensions::idle::stop_on_any).map_err(|error| error.to_string())?;
        Ok(Some(outcome == WaitOutcome::MailboxChanged))
    }

    let shared = state.session(account_id).ok_or("No IMAP session")?;
    let mut session = shared.lock().map_err(|error| error.to_string())?;
    match &mut *session {
        ImapSession::Plain(session) => wait(session, mailbox),
        ImapSession::Tls(session) => wait(session, mailbox),
    }
}

enum ImapSession {
    Plain(imap::Session<ImapTransport<TcpStream>>),
    Tls(imap::Session<ImapTransport<native_tls::TlsStream<TcpStream>>>),
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
                    let name = crate::mail_models::mailbox_identity(&name).to_string();
                    let mut role = None;
                    let mut selectable = true;
                    for attr in n.attributes() {
                        match attr {
                            imap_proto::NameAttribute::NoSelect => selectable = false,
                            imap_proto::NameAttribute::All => role = Some("all".to_owned()),
                            imap_proto::NameAttribute::Archive => role = Some("archive".to_owned()),
                            imap_proto::NameAttribute::Drafts => role = Some("drafts".to_owned()),
                            imap_proto::NameAttribute::Flagged => role = Some("flagged".to_owned()),
                            imap_proto::NameAttribute::Junk => role = Some("junk".to_owned()),
                            imap_proto::NameAttribute::Sent => role = Some("sent".to_owned()),
                            imap_proto::NameAttribute::Trash => role = Some("trash".to_owned()),
                            imap_proto::NameAttribute::Extension(value)
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
        if !is_imap_keyword(keyword) {
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

    fn search_new_uids(&mut self, since_uid: u32) -> Result<Vec<u32>, String> {
        let Some(first_uid) = since_uid.checked_add(1) else {
            return Ok(vec![]);
        };
        let mut uids = self.uid_search_query(&format!("UID {first_uid}:*"))?;
        // IMAP sequence ranges are bidirectional. '*' can match the last
        // message even when first_uid is beyond the highest UID.
        uids.retain(|uid| *uid >= first_uid);
        uids.reverse();
        Ok(uids)
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

pub(crate) fn connect_tcp(host: &str, port: u16) -> Result<TcpStream, String> {
    use std::{net::ToSocketAddrs, time::Duration};

    let timeout = Duration::from_secs(15);
    let addresses = (host, port).to_socket_addrs().map_err(|e| e.to_string())?;
    let mut last_error = "IMAP host has no addresses".to_string();
    for addr in addresses {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => {
                stream.set_read_timeout(Some(timeout)).map_err(|e| e.to_string())?;
                stream.set_write_timeout(Some(timeout)).map_err(|e| e.to_string())?;
                return Ok(stream);
            }
            Err(error) => last_error = error.to_string(),
        }
    }
    Err(last_error)
}

pub(crate) fn connect_tls(
    host: &str,
    port: u16,
    tls: &TlsConnector,
    starttls: bool,
) -> Result<imap::Client<ImapTransport<native_tls::TlsStream<TcpStream>>>, String> {
    let stream = connect_tcp(host, port)?;
    if starttls {
        let mut client = imap::Client::new(stream);
        client.read_greeting().map_err(|e| e.to_string())?;
        let mut stream = client.into_inner().map_err(|e| e.to_string())?;
        stream.write_all(b"guv-starttls STARTTLS\r\n").map_err(|e| e.to_string())?;
        let mut reader = BufReader::new(&mut stream);
        let mut response = Vec::new();
        loop {
            response.clear();
            let length = reader.by_ref().take(65_536).read_until(b'\n', &mut response)
                .map_err(|e| e.to_string())?;
            if length == 0 || !response.ends_with(b"\r\n") {
                return Err("Incomplete IMAP STARTTLS response".into());
            }
            if response.starts_with(b"* ") && !response.starts_with(b"* BYE") {
                continue;
            }
            let mut words = response.split(|byte| byte.is_ascii_whitespace());
            if words.next() == Some(b"guv-starttls")
                && words.next().is_some_and(|status| status.eq_ignore_ascii_case(b"OK"))
            {
                break;
            }
            return Err("IMAP server declined STARTTLS".into());
        }
        let mut client = imap::Client::new(ImapTransport::new(tls.connect(host, stream).map_err(|e| e.to_string())?));
        client.greeting_read = true;
        return Ok(client);
    }
    let mut client = imap::Client::new(ImapTransport::new(tls.connect(host, stream).map_err(|e| e.to_string())?));
    client.read_greeting().map_err(|e| e.to_string())?;
    Ok(client)
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
    let tls = TlsConnector::new().map_err(|e| format!("{e}"))?;

    let session = match ssl_mode.to_uppercase().as_str() {
        "SSL" => {
            let client = connect_tls(imap_host, imap_port, &tls, false)?;
            let s = client
                .login(email, password)
                .map_err(|(e, _)| format!("{e}"))?;
            ImapSession::Tls(s)
        }
        "STARTTLS" => {
            let client = connect_tls(imap_host, imap_port, &tls, true)?;
            let s = client
                .login(email, password)
                .map_err(|(e, _)| format!("{e}"))?;
            ImapSession::Tls(s)
        }
        _ => {
            let tcp = connect_tcp(imap_host, imap_port)?;
            let mut client = imap::Client::new(ImapTransport::new(tcp));
            client.read_greeting().map_err(|e| format!("{e}"))?;
            let s = client
                .login(email, password)
                .map_err(|(e, _)| format!("{e}"))?;
            ImapSession::Plain(s)
        }
    };

    state.insert_session(account_id, session);
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
    let tls = TlsConnector::new().map_err(|e| format!("{e}"))?;

    let auth = crate::imap_client::XOAuth2 {
        user: email.to_string(),
        access_token: access_token.to_string(),
    };

    let session = match ssl_mode.to_uppercase().as_str() {
        "STARTTLS" => {
            let client = connect_tls(imap_host, imap_port, &tls, true)?;
            let s = client
                .authenticate("XOAUTH2", &auth)
                .map_err(|(e, _)| format!("{e}"))?;
            ImapSession::Tls(s)
        }
        _ => {
            let client = connect_tls(imap_host, imap_port, &tls, false)?;
            let s = client
                .authenticate("XOAUTH2", &auth)
                .map_err(|(e, _)| format!("{e}"))?;
            ImapSession::Tls(s)
        }
    };

    state.insert_session(account_id, session);
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
    match state.session(account_id) {
        Some(session) => session.lock().unwrap().list_mailboxes(),
        None => {
            warn!("No IMAP session for account {account_id}");
            vec![]
        }
    }
}

pub fn list_mailboxes_detailed(state: &Arc<ImapState>, account_id: i64) -> Vec<MailboxEntry> {
    match state.session(account_id) {
        Some(session) => session.lock().unwrap().list_mailboxes_detailed(),
        None => {
            warn!("No IMAP session for account {account_id}");
            vec![]
        }
    }
}

pub struct MailboxPage {
    pub generation: u32,
    pub total: usize,
    pub previews: Vec<MailPreview>,
}

pub fn fetch_mail_list(
    state: &Arc<ImapState>,
    account_id: i64,
    mailbox: &str,
    page: usize,
    per_page: usize,
    expected_generation: Option<u32>,
) -> Result<MailboxPage, String> {
    let shared = state.session(account_id).ok_or("No IMAP session")?;
    let mut session = shared.lock().unwrap();
    let selected = session.select_mailbox_info(mailbox)?;
    let generation = selected.uid_validity.filter(|value| *value > 0)
        .ok_or("Mailbox UIDVALIDITY is unavailable")?;
    if expected_generation.is_some_and(|expected| expected != generation) {
        return Err("Mailbox UIDVALIDITY changed during refresh".into());
    }

    let per_page = per_page.max(1);
    let total = selected.exists as usize;
    let start_from_end = page.saturating_sub(1).saturating_mul(per_page);
    if start_from_end >= total {
        return Ok(MailboxPage { generation, total, previews: vec![] });
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
    Ok(MailboxPage { generation, total, previews })
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

fn imap_search_string(value: &str) -> String {
    let value = value.replace(['\r', '\n', '\0'], " ");
    if value.is_ascii() {
        quote_imap_string(&value)
    } else {
        format!("{{{}}}\r\n{value}", value.len())
    }
}

fn imap_expression(expression: crate::search_expression::SearchExpression) -> String {
    use crate::search_expression::{Field, SearchExpression::*};
    match expression {
        Term(field, text) => {
            let key = match field {
                Field::Text | Field::IndexedText => "TEXT",
                Field::From => "FROM",
                Field::To => "TO",
                Field::Cc => "CC",
                Field::Subject => "SUBJECT",
            };
            format!("{key} {}", imap_search_string(&text))
        }
        Not(inner) => format!("NOT ({})", imap_expression(*inner)),
        And(left, right) => format!("({} {})", imap_expression(*left), imap_expression(*right)),
        Or(left, right) => format!("OR ({}) ({})", imap_expression(*left), imap_expression(*right)),
    }
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
        parts.push(format!("FROM {}", imap_search_string(value)));
    }
    if let Some(value) = to {
        parts.push(format!("TO {}", imap_search_string(value)));
    }
    if let Some(value) = cc {
        parts.push(format!("CC {}", imap_search_string(value)));
    }
    if let Some(value) = subject {
        parts.push(format!("SUBJECT {}", imap_search_string(value)));
    }
    if let Some(value) = keywords {
        parts.push(imap_expression(crate::search_expression::keywords(value)));
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
    let shared = state.session(account_id)
        .ok_or_else(|| format!("No IMAP session for account {account_id}"))?;
    let mut session = shared.lock().unwrap();

    let mut query = build_imap_advanced_query(req);
    let unicode = !query.is_ascii();
    if unicode {
        query.insert_str(0, "CHARSET UTF-8 ");
    }

    let mut mailboxes: Vec<String> = match req.scope {
        SearchScope::All => session.list_mailboxes(),
        SearchScope::Mailboxes => req.mailboxes.clone(),
    };

    let mut seen = std::collections::HashSet::new();
    mailboxes.retain(|value| {
        if value.is_empty()
            || matches!(value.to_ascii_lowercase().as_str(), "folders" | "labels" | "etiketler" | "[labels]")
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
                if unicode { return Err(e); }
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
            .then_with(|| a.mailbox.cmp(&b.mailbox))
    });

    Ok(results)
}

pub fn fetch_mail_raw_in_mailbox(
    state: &Arc<ImapState>,
    account_id: i64,
    mailbox: &str,
    uid: &str,
    expected_validity: Option<u32>,
) -> Option<Vec<u8>> {
    fetch_mail_raw_with_validity(state, account_id, mailbox, uid, expected_validity).map(|(raw, _)| raw)
}

pub fn fetch_mail_raw_with_validity(
    state: &Arc<ImapState>,
    account_id: i64,
    mailbox: &str,
    uid: &str,
    expected_validity: Option<u32>,
) -> Option<(Vec<u8>, Option<u32>)> {
    let shared = state.session(account_id)?;
    let mut session = shared.lock().unwrap();
    let selected = session.select_mailbox_info(mailbox).ok()?;
    if expected_validity.is_some_and(|expected| expected == 0 || selected.uid_validity != Some(expected)) {
        return None;
    }
    session.fetch_mail_raw(uid).map(|raw| (raw, selected.uid_validity))
}

pub fn fetch_new_uids_since(
    state: &Arc<ImapState>,
    account_id: i64,
    mailbox: &str,
    checkpoint: (u32, u32),
) -> Result<(u32, u32, Vec<u32>), String> {
    let shared = state.session(account_id).ok_or("No IMAP session")?;
    let mut session = shared.lock().unwrap();
    let selected = session.select_mailbox_info(mailbox)?;
    let generation = selected.uid_validity.filter(|value| *value > 0)
        .ok_or("Mailbox UIDVALIDITY is unavailable")?;
    let since_uid = if generation == checkpoint.1 { checkpoint.0 } else { 0 };
    let uids = if selected.exists == 0 {
        vec![]
    } else {
        session.search_new_uids(since_uid)?
    };
    Ok((generation, since_uid, uids))
}

pub fn fetch_headers_for_uids(
    state: &Arc<ImapState>,
    account_id: i64,
    mailbox: &str,
    uids: &[u32],
    expected_validity: u32,
) -> Vec<MailPreview> {
    if uids.is_empty() {
        return vec![];
    }
    let Some(shared) = state.session(account_id) else {
        return vec![];
    };
    let mut session = shared.lock().unwrap();

    let selected = match session.select_mailbox_info(mailbox) {
        Ok(selected) => selected,
        Err(error) => {
            warn!("Cannot fetch headers for the requested mailbox: {error}");
            return vec![];
        }
    };

    if selected.uid_validity != Some(expected_validity) {
        return vec![];
    }

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
    let removed = state.sessions.lock().unwrap().remove(&account_id);
    if let Some(session) = removed {
        match &mut *session.lock().unwrap() {
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

pub fn has_session(state: &Arc<ImapState>, account_id: i64) -> bool {
    // The map lock only protects membership. Network I/O holds the account's
    // own session lock so polling and operations on other accounts can proceed.
    state.sessions.lock().unwrap().contains_key(&account_id)
}

pub fn is_connected(state: &Arc<ImapState>, account_id: i64) -> bool {
    let Some(shared) = state.session(account_id) else {
        return false;
    };
    let mut session = shared.lock().unwrap();
    if let Err(err) = session.noop() {
        warn!("IMAP session unhealthy for account {account_id}: {err}");
        let mut sessions = state.sessions.lock().unwrap();
        if sessions.get(&account_id).is_some_and(|current| Arc::ptr_eq(current, &shared)) {
            sessions.remove(&account_id);
        }
        return false;
    }
    true
}

pub enum QueuedMailAction {
    Seen(bool),
    Flagged(bool),
    Answered,
    Forwarded,
    Label(String, bool),
    Move(String),
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
    let shared = state.session(account_id)
        .ok_or_else(|| format!("No IMAP session for account {account_id}"))?;
    let mut session = shared.lock().unwrap();
    let selected = session.select_mailbox_info(mailbox)?;
    if selected.uid_validity != Some(expected) {
        return Err(QueuedMailError {
            message: "Mailbox UIDVALIDITY changed or is unavailable. Refresh the folder and repeat the action.".into(),
            retryable: false,
        });
    }
    action(&mut session)
}

pub fn apply_queued_mail_action(
    state: &Arc<ImapState>,
    account_id: i64,
    mailbox: &str,
    uid: &str,
    expected_validity: Option<u32>,
    action: QueuedMailAction,
) -> Result<(), QueuedMailError> {
    with_queued_mailbox(state, account_id, mailbox, expected_validity, |session| {
        match action {
            QueuedMailAction::Seen(seen) => session.uid_store_flag(uid, "\\Seen", seen)?,
            QueuedMailAction::Flagged(flagged) => session.uid_store_flag(uid, "\\Flagged", flagged)?,
            QueuedMailAction::Answered => session.uid_store_flag(uid, "\\Answered", true)?,
            QueuedMailAction::Forwarded => session.uid_store_keyword(uid, "$Forwarded", true)?,
            QueuedMailAction::Label(label, add) => set_selected_mailbox_label(session, mailbox, uid, label.trim(), add)?,
            QueuedMailAction::Move(destination) => session.uid_move_to(uid, &destination).map_err(|error| {
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

fn is_imap_keyword(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| {
        byte.is_ascii_graphic() && !matches!(byte, b'(' | b')' | b'{' | b'%' | b'*' | b'"' | b'\\' | b']')
    })
}

fn uid_message_id(session: &mut ImapSession, uid: &str) -> Result<Option<String>, String> {
    const QUERY: &str = "(UID BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)])";
    let fetches = match session {
        ImapSession::Plain(session) => session.uid_fetch(uid, QUERY),
        ImapSession::Tls(session) => session.uid_fetch(uid, QUERY),
    }.map_err(|error| error.to_string())?;
    let expected_uid = uid.parse::<u32>().ok();
    let header = fetches.iter().find(|fetch| fetch.uid.is_some() && fetch.uid == expected_uid)
        .and_then(|fetch| fetch.header());
    Ok(header.and_then(|header| parse_headers(header).ok())
        .and_then(|(headers, _)| headers.get_first_value("Message-ID"))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty()))
}

fn set_selected_mailbox_label(
    session: &mut ImapSession,
    mailbox: &str,
    uid: &str,
    label: &str,
    add: bool,
) -> Result<(), QueuedMailError> {
    let mailboxes = session.list_mailboxes();
    let folder = ["Labels", "Etiketler", "[Labels]"].into_iter().find_map(|root| {
        let candidate = format!("{root}/{label}");
        mailboxes.iter().find(|mailbox| mailbox.eq_ignore_ascii_case(&candidate))
    });

    if is_imap_keyword(label) || folder.is_none() {
        session.uid_store_keyword(uid, label, add)?;
    }

    if let Some(folder) = folder {
        if add && folder != mailbox {
            let wire_folder = quote_imap_string(&crate::imap_utf7::encode(folder));
            match session {
                ImapSession::Plain(session) => session.uid_copy(uid, &wire_folder),
                ImapSession::Tls(session) => session.uid_copy(uid, &wire_folder),
            }.map_err(|error| {
                let error = MoveError::after_mutation(error);
                QueuedMailError {
                    message: format!("Could not copy message to label folder: {}", error.message),
                    retryable: !error.outcome_uncertain,
                }
            })?;
        } else if !add && folder == mailbox {
            session.uid_delete(uid)?;
        } else if !add {
            let message_id = uid_message_id(session, uid)?
                .ok_or_else(|| "Cannot identify the copy in the label folder without a Message-ID".to_string())?;
            session.select_mailbox_info(folder)?;
            let matches = session.uid_search_query(&format!(
                "HEADER Message-ID {}", quote_imap_string(&message_id),
            ))?;
            let mut exact_uid = None;
            for candidate in matches {
                let candidate = candidate.to_string();
                if uid_message_id(session, &candidate)?.as_deref() == Some(&message_id) {
                    if exact_uid.is_some() {
                        return Err("Multiple copies match the label. Remove the intended copy directly from its folder.".to_string().into());
                    }
                    exact_uid = Some(candidate);
                }
            }
            if let Some(candidate) = exact_uid {
                session.uid_delete(&candidate)?;
            }
        }
    }
    Ok(())
}

pub fn create_mailbox(
    state: &Arc<ImapState>,
    account_id: i64,
    mailbox: &str,
) -> Result<(), String> {
    let shared = state.session(account_id)
        .ok_or_else(|| format!("No IMAP session for account {account_id}"))?;
    let mut session = shared.lock().unwrap();
    let mailbox = crate::imap_utf7::encode(mailbox);
    let result = match &mut *session {
        ImapSession::Plain(s) => s.create(&mailbox),
        ImapSession::Tls(s) => s.create(&mailbox),
    };
    result.map(|_| ()).map_err(|e| format!("{e}"))
}

#[derive(Debug)]
pub struct DraftAppendError {
    pub message: String,
    pub may_exist: bool,
}

pub fn append_draft(
    state: &Arc<ImapState>,
    account_id: i64,
    mailbox: &str,
    raw: &[u8],
    tracking_key: &str,
    append_new: bool,
) -> Result<(String, u32), DraftAppendError> {
    let shared = state.session(account_id).ok_or_else(|| DraftAppendError {
        message: format!("No IMAP session for account {account_id}"),
        may_exist: !append_new,
    })?;
    let mut session = shared.lock().unwrap();

    if append_new {
        let flags = [imap::types::Flag::Draft, imap::types::Flag::Seen];
        let encoded = crate::imap_utf7::encode(mailbox);
        let quoted = quote_imap_string(&encoded);
        let wire_mailbox = &quoted[1..quoted.len() - 1];
        let appended = match &mut *session {
            ImapSession::Plain(s) => s.append(wire_mailbox, raw).flags(flags).finish(),
            ImapSession::Tls(s) => s.append(wire_mailbox, raw).flags(flags).finish(),
        }.map_err(|error| DraftAppendError {
            may_exist: !matches!(error, imap::Error::No(_) | imap::Error::Bad(_) | imap::Error::Append),
            message: error.to_string(),
        })?;
        if let (Some(generation), Some([imap_proto::UidSetMember::Uid(uid)])) =
            (appended.uid_validity, appended.uids.as_deref())
        {
            if generation > 0 && *uid > 0 {
                return Ok((uid.to_string(), generation));
            }
        }
    }

    let mut lookup = || {
        let generation = session.select_mailbox_info(mailbox)?.uid_validity
            .filter(|value| *value > 0)
            .ok_or("Mailbox UIDVALIDITY is unavailable")?;
        let query = format!("HEADER X-GUVERCIN-DRAFT-KEY {}", quote_imap_string(tracking_key));
        match session.uid_search_query(&query)?.as_slice() {
            [uid] => Ok((uid.to_string(), generation)),
            _ => Err("A unique draft UID is not yet available".to_owned()),
        }
    };
    lookup().map_err(|message| DraftAppendError { message, may_exist: true })
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
    fn recurse<F>(part: &ParsedMail, counter: &mut usize, visitor: &mut F, implicit_calendar: bool) -> bool
    where
        F: FnMut(&ParsedMail, usize) -> bool,
    {
        if part.subparts.is_empty() {
            let matches = if implicit_calendar {
                part.ctype.mimetype.eq_ignore_ascii_case("text/calendar") && !should_treat_as_attachment(part)
            } else {
                should_treat_as_attachment(part)
            };
            if matches {
                let idx = *counter;
                *counter += 1;
                if visitor(part, idx) {
                    return true;
                }
            }
        } else {
            for child in &part.subparts {
                if recurse(child, counter, visitor, implicit_calendar) {
                    return true;
                }
            }
        }
        false
    }

    let mut counter = 0;
    // Append implicit invitations so persisted attachment indexes still refer
    // to the original MIME parts.
    recurse(part, &mut counter, visitor, false) || recurse(part, &mut counter, visitor, true)
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
        .unwrap_or_else(|| {
            if part.ctype.mimetype == "text/calendar" {
                format!("invitation-{}.ics", index + 1)
            } else {
                format!("attachment-{}", index + 1)
            }
        });

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
            state.insert_session(1, super::ImapSession::Plain(session));
            assert!(super::fetch_headers_for_uids(&state, 1, "Missing folder", &[42], 7).is_empty());
        });
        assert_eq!(commands, ["SELECT \"Missing folder\""]);
    }

    #[test]
    fn incremental_uid_search_ignores_the_old_last_uid_and_handles_exhaustion() {
        let commands = record_imap_commands("UIDPLUS", None, |session| {
            let mut session = super::ImapSession::Plain(session);
            assert!(session.search_new_uids(42).unwrap().is_empty());
            assert_eq!(session.search_new_uids(40).unwrap(), vec![42, 41]);
            assert!(session.search_new_uids(u32::MAX).unwrap().is_empty());
        });
        assert_eq!(commands, ["UID SEARCH UID 43:*", "UID SEARCH UID 41:*"]);
    }

    #[test]
    fn queued_actions_refuse_reused_uids_and_accept_the_original_generation() {
        use super::{apply_queued_mail_action, ImapSession, ImapState, QueuedMailAction};
        let commands = record_imap_commands("UIDPLUS", None, |session| {
            let state = std::sync::Arc::new(ImapState::new());
            state.insert_session(1, ImapSession::Plain(session));
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

    #[test]
    fn queued_stars_set_and_clear_the_imap_flag() {
        let commands = record_imap_commands("UIDPLUS", None, |session| {
            let state = std::sync::Arc::new(super::ImapState::new());
            state.insert_session(1, super::ImapSession::Plain(session));
            for enabled in [true, false] {
                assert!(super::apply_queued_mail_action(
                    &state, 1, "INBOX", "42", Some(7), super::QueuedMailAction::Flagged(enabled),
                ).is_ok());
            }
        });
        assert!(commands.iter().any(|command| command == "UID STORE 42 +FLAGS (\\Flagged)"));
        assert!(commands.iter().any(|command| command == "UID STORE 42 -FLAGS (\\Flagged)"));
    }

    #[test]
    fn replaced_generation_searches_below_the_old_checkpoint() {
        let commands = record_imap_commands("UIDPLUS", None, |session| {
            let state = std::sync::Arc::new(super::ImapState::new());
            state.insert_session(1, super::ImapSession::Plain(session));
            assert_eq!(
                super::fetch_new_uids_since(&state, 1, "INBOX", (900, 6)).unwrap(),
                (7, 0, vec![42, 41]),
            );
            assert_eq!(
                super::fetch_new_uids_since(&state, 1, "INBOX", (42, 7)).unwrap(),
                (7, 42, vec![]),
            );
            assert!(super::fetch_new_uids_since(&state, 1, "Unknown validity", (42, 7)).is_err());
        });
        assert_eq!(commands, [
            "SELECT \"INBOX\"",
            "UID SEARCH UID 1:*",
            "SELECT \"INBOX\"",
            "UID SEARCH UID 43:*",
            "SELECT \"Unknown validity\"",
        ]);
    }

    #[test]
    fn refresh_replaces_a_generation_without_retargeting_queued_or_open_mail() {
        use axum::{extract::{Path, Query, State}, http::StatusCode, response::IntoResponse, Json};
        use std::sync::Arc;

        use crate::{db, mail_routes::{self, MailAppState}, offline_routes};

        let commands = record_imap_commands("UIDPLUS SINGLE-UID-FIXTURE", None, |session| {
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                let timestamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
                let dir = std::env::temp_dir().join(format!("guvercin-uid-{}-{timestamp}", std::process::id()));
                struct ScratchDirectory(std::path::PathBuf);
                impl Drop for ScratchDirectory {
                    fn drop(&mut self) {
                        let _ = std::fs::remove_dir_all(&self.0);
                    }
                }
                std::fs::create_dir(&dir).unwrap();
                let _scratch = ScratchDirectory(dir.clone());
                std::fs::write(dir.join("security_settings.json"), r#"{"data_encrypted":false}"#).unwrap();

                let db = Arc::new(db::AppState::initialize(Some(dir.clone())).await.unwrap());
                let imap = Arc::new(super::ImapState::new());
                imap.insert_session(1, super::ImapSession::Plain(session));
                let state = Arc::new(MailAppState {
                    _db: db.clone(),
                    imap,
                    sync_locks: Default::default(),
                });
                let pool = db::get_user_db_pool(&db, 1).await.unwrap();
                sqlx::query("INSERT INTO sync_checkpoints (folder_path, last_synced_uid, last_uid_validity) VALUES ('INBOX', 900, 6)")
                    .execute(&pool).await.unwrap();
                sqlx::query(
                    "INSERT INTO local_mail_cache (uid, folder, uid_validity, subject, plain_body) VALUES
                     ('42', 'INBOX', 6, 'Old subject', 'Old body'),
                     ('900', 'INBOX', 6, 'Old cached message', 'Old body'),
                     ('901', 'inbox', 6, 'Old alias message', 'Old body'),
                     ('42', 'Projects', 6, 'Other folder', ''),
                     ('42', 'projects', 6, 'Case distinct folder', '')",
                ).execute(&pool).await.unwrap();
                let other = db::get_user_db_pool(&db, 2).await.unwrap();
                sqlx::query("INSERT INTO local_mail_cache (uid, folder, uid_validity) VALUES ('42', 'INBOX', 6)")
                    .execute(&other).await.unwrap();

                let action = serde_json::from_value(serde_json::json!({
                    "action_type": "delete", "target_uid": "42", "target_folder": "INBOX", "uid_validity": 6,
                })).unwrap();
                let _ = offline_routes::post_offline_action(State(state.clone()), Path(1), Json(action)).await.unwrap();

                let refresh = || serde_json::from_value(serde_json::json!({"mailbox": "inbox"})).unwrap();
                let _ = offline_routes::sync_mailbox(State(state.clone()), Path(1), Query(refresh())).await.unwrap();
                let local_query = |generation| serde_json::from_value(serde_json::json!({
                    "mailbox": "INBOX", "uid_validity": generation,
                })).unwrap();
                let old = offline_routes::get_local_mail_content(State(state.clone()), Path((1, "42".into())), Query(local_query(6)))
                    .await.into_response();
                assert_eq!(old.status(), StatusCode::NOT_FOUND);
                let new = offline_routes::get_local_mail_content(State(state.clone()), Path((1, "42".into())), Query(local_query(7)))
                    .await.into_response();
                assert_eq!(new.status(), StatusCode::NOT_FOUND, "replacement headers must not inherit the old body");
                let old_remote = mail_routes::get_mail_content(State(state.clone()), Path((1, "42".into())), Query(mail_routes::MailContentQuery {
                    mailbox: "INBOX".into(), uid_validity: Some(6),
                })).await.into_response();
                assert_eq!(old_remote.status(), StatusCode::NOT_FOUND);
                let new_remote = mail_routes::get_mail_content(State(state.clone()), Path((1, "42".into())), Query(mail_routes::MailContentQuery {
                    mailbox: "INBOX".into(), uid_validity: Some(7),
                })).await.into_response();
                assert_eq!(new_remote.status(), StatusCode::OK);
                let local = offline_routes::get_local_mail_content(State(state.clone()), Path((1, "42".into())), Query(local_query(7)))
                    .await.into_response();
                let body = axum::body::to_bytes(local.into_body(), 4096).await.unwrap();
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(body["plain_body"].as_str().unwrap().trim(), "Replacement body");

                let rows: Vec<(String, String, u32, String, Option<String>)> = sqlx::query_as(
                    "SELECT folder, uid, uid_validity, subject, plain_body FROM local_mail_cache ORDER BY folder",
                ).fetch_all(&pool).await.unwrap();
                assert_eq!(rows, [
                    ("INBOX".into(), "42".into(), 7, "Replacement".into(), Some(body["plain_body"].as_str().unwrap().into())),
                    ("Projects".into(), "42".into(), 6, "Other folder".into(), Some(String::new())),
                    ("projects".into(), "42".into(), 6, "Case distinct folder".into(), Some(String::new())),
                ]);
                assert_eq!(sqlx::query_scalar::<_, u32>("SELECT uid_validity FROM local_mail_cache").fetch_one(&other).await.unwrap(), 6);
                sqlx::query(
                    "INSERT INTO local_mail_cache (uid, folder, uid_validity) VALUES
                     ('2', 'Projects', 6), ('10', 'Projects', 6)",
                ).execute(&pool).await.unwrap();
                let search = serde_json::from_value(serde_json::json!({
                    "scope": "mailboxes", "mailboxes": ["Projects"],
                })).unwrap();
                let Json(results) = offline_routes::search_advanced(State(state.clone()), Path(1), Json(search))
                    .await.unwrap();
                assert_eq!(results.total_count, 3);
                assert_eq!(results.mails.into_iter().map(|mail| mail.mail.id).collect::<Vec<_>>(), ["42", "10", "2"]);
                sqlx::query("UPDATE sync_checkpoints SET last_synced_uid = 40 WHERE folder_path = 'INBOX'")
                    .execute(&pool).await.unwrap();
                let Json(sync) = offline_routes::sync_now(State(state.clone()), Path(1)).await.unwrap();
                assert_eq!(sync.failed, 1);

                drop(state);
                pool.close().await;
                other.close().await;
                drop(db);
                let reopened = db::AppState::initialize(Some(dir)).await.unwrap();
                let pool = db::get_user_db_pool(&reopened, 1).await.unwrap();
                let checkpoint: (u32, u32) = sqlx::query_as(
                    "SELECT last_synced_uid, last_uid_validity FROM sync_checkpoints WHERE folder_path = 'INBOX'",
                ).fetch_one(&pool).await.unwrap();
                assert_eq!(checkpoint, (42, 7));
                let queued: (u32, String) = sqlx::query_as("SELECT uid_validity, status FROM offline_sync_queue")
                    .fetch_one(&pool).await.unwrap();
                assert_eq!(queued, (6, "dead".into()));
                pool.close().await;
            });
        });
        let commands: Vec<_> = commands.iter().filter(|command| *command != "NOOP").collect();
        assert_eq!(commands.len(), 13, "{commands:?}");
        assert_eq!(commands[0], "SELECT \"INBOX\"");
        assert_eq!(commands[1], "UID SEARCH UID 1:*");
        assert_eq!(commands[2], "SELECT \"INBOX\"");
        assert!(commands[3].starts_with("UID FETCH 42 "));
        assert_eq!(commands[4], "SELECT \"INBOX\"");
        assert_eq!(commands[5], "SELECT \"INBOX\"");
        assert!(commands[6].starts_with("UID FETCH 42 (UID BODY.PEEK[]"));
        assert_eq!(commands[7], "SELECT \"INBOX\"");
        assert!(commands[8].starts_with("LIST "));
        assert_eq!(commands[9], "SELECT \"INBOX\"");
        assert_eq!(commands[10], "UID SEARCH UID 41:*");
        assert_eq!(commands[11], "SELECT \"INBOX\"");
        assert!(commands[12].starts_with("UID FETCH 42 "));
    }

    #[test]
    fn labels_with_spaces_use_a_quoted_folder_and_report_copy_rejection() {
        for reject in [None, Some("UID COPY")] {
            let commands = record_imap_commands("UIDPLUS LABEL-FIXTURE", reject, |session| {
                let state = std::sync::Arc::new(super::ImapState::new());
                state.insert_session(1, super::ImapSession::Plain(session));
                let result = super::apply_queued_mail_action(
                    &state, 1, "INBOX", "42", Some(7),
                    super::QueuedMailAction::Label("My Label".to_string(), true),
                );
                assert_eq!(result.is_err(), reject.is_some());
            });
            assert!(commands.iter().any(|command| command == "UID COPY 42 \"Etiketler/My Label\""));
            assert!(!commands.iter().any(|command| command.starts_with("UID STORE")));
        }
    }

    #[test]
    fn unicode_search_counts_utf8_literal_bytes() {
        assert_eq!(super::imap_search_string("güvercin"), "{9}\r\ngüvercin");
        assert_eq!(super::imap_search_string("first\r\nsecond"), "\"first  second\"");
    }

    #[test]
    fn unicode_search_waits_for_each_literal_on_a_strict_imap_server() {
        let commands = record_imap_commands("", None, |session| {
            let mut session = super::ImapSession::Plain(session);
            let subject = super::imap_search_string("güvercin");
            let text = super::imap_search_string("İstanbul");
            let uids = session.uid_search_query(&format!("CHARSET UTF-8 SUBJECT {subject} TEXT {text}"))
                .unwrap();
            assert_eq!(uids, [41, 42]);
            assert!(session.noop().is_ok());
        });
        assert_eq!(commands, [
            "UID SEARCH CHARSET UTF-8 SUBJECT {9}",
            "güvercin TEXT {9}",
            "İstanbul",
            "NOOP",
        ]);
    }

    #[test]
    fn rejected_search_literal_does_not_send_text_or_break_the_next_command() {
        let commands = record_imap_commands("", Some(super::UTF8_UID_SEARCH), |session| {
            let mut session = super::ImapSession::Plain(session);
            let subject = super::imap_search_string("güvercin");
            assert!(session.uid_search_query(&format!("CHARSET UTF-8 SUBJECT {subject}")).is_err());
            assert!(session.noop().is_ok());
        });
        assert_eq!(commands, ["UID SEARCH CHARSET UTF-8 SUBJECT {9}", "NOOP"]);
    }

    #[test]
    fn saved_draft_uses_appenduid_and_pending_draft_is_never_appended_twice() {
        let commands = record_imap_commands("UIDPLUS APPENDUID-FIXTURE", None, |session| {
            let state = std::sync::Arc::new(super::ImapState::new());
            state.insert_session(1, super::ImapSession::Plain(session));
            let raw = b"Subject: Fixture\r\n\r\nDraft body";
            let created = super::append_draft(&state, 1, "Drafts", raw, "first", true).unwrap();
            assert_eq!(created, ("77".into(), 12));
            let recovered = super::append_draft(&state, 1, "Drafts", raw, "unique", false).unwrap();
            assert_eq!(recovered, ("42".into(), 7));
        });
        assert_eq!(commands.iter().filter(|command| command.starts_with("APPEND ")).count(), 1);
        assert_eq!(commands.iter().filter(|command| command.starts_with("UID SEARCH HEADER ")).count(), 1);
    }

    #[test]
    fn appended_draft_requires_one_tracking_match_and_carries_its_generation() {
        let commands = record_imap_commands("UIDPLUS", None, |session| {
            let state = std::sync::Arc::new(super::ImapState::new());
            state.insert_session(1, super::ImapSession::Plain(session));
            let raw = b"Subject: Fixture\r\n\r\nDraft body";
            let valid = super::append_draft(&state, 1, "Drafts", raw, "unique", true).unwrap();
            assert_eq!(valid, ("42".into(), 7));
            for key in ["missing", "ambiguous"] {
                assert!(super::append_draft(&state, 1, "Drafts", raw, key, true).is_err());
            }
        });
        assert_eq!(commands.iter().filter(|command| command.starts_with("APPEND ")).count(), 3);
        assert_eq!(commands.iter().filter(|command| command.starts_with("UID SEARCH HEADER ")).count(), 3);
        assert!(commands.iter().all(|command| command != "UID SEARCH ALL"));
    }

    #[test]
    fn inbox_idle_delivers_a_change_and_terminates_the_command() {
        let commands = record_imap_commands("IDLE", None, |session| {
            let state = std::sync::Arc::new(super::ImapState::new());
            state.insert_session(1, super::ImapSession::Plain(session));
            assert_eq!(super::wait_for_mailbox_change(&state, 1, "INBOX").unwrap(), Some(true));
            super::disconnect(&state, 1);
        });
        assert_eq!(commands, ["EXAMINE \"INBOX\"", "IDLE", "DONE"]);
    }

    #[test]
    fn idle_can_watch_a_named_folder_on_its_own_session() {
        let commands = record_imap_commands("IDLE", None, |session| {
            let state = std::sync::Arc::new(super::ImapState::new());
            state.insert_session(9, super::ImapSession::Plain(session));
            assert_eq!(super::wait_for_mailbox_change(&state, 9, "Projects/İstanbul").unwrap(), Some(true));
            super::disconnect(&state, 9);
        });
        let encoded = crate::imap_utf7::encode("Projects/İstanbul");
        assert_eq!(commands, [format!("EXAMINE \"{encoded}\""), "IDLE".into(), "DONE".into()]);
    }

    #[test]
    fn implicit_calendar_parts_keep_existing_attachment_indexes() {
        let raw = b"Content-Type: multipart/mixed; boundary=parts\r\n\r\n--parts\r\nContent-Type: text/calendar; method=REQUEST\r\n\r\nBEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n--parts\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=data.bin\r\n\r\ndata\r\n--parts--\r\n";
        let content = super::parse_mail_content("1".to_owned(), raw);
        assert_eq!(content.attachments[0].filename, "data.bin");
        assert_eq!(content.attachments[1].content_type, "text/calendar");
        assert_eq!(super::find_attachment_bytes(raw, 0).unwrap().1, b"data\r\n");
        assert!(super::find_attachment_bytes(raw, 1).unwrap().1.starts_with(b"BEGIN:VCALENDAR"));
    }

    fn record_imap_commands(
        capability: &'static str,
        reject_prefix: Option<&'static str>,
        action: impl FnOnce(imap::Session<super::ImapTransport<std::net::TcpStream>>),
    ) -> Vec<String> {
        use std::io::{BufRead, BufReader, Read, Write};
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
                        if cmd == "IDLE" {
                            write!(stream, "+ idling\r\n* 2 EXISTS\r\n").unwrap();
                            let mut done = String::new();
                            reader.read_line(&mut done).unwrap();
                            assert_eq!(done, "DONE\r\n");
                            commands.push("DONE".into());
                        }
                        if cmd.starts_with("EXAMINE ") {
                            write!(stream, "* 1 EXISTS\r\n* OK [UIDVALIDITY 7] fixture\r\n").unwrap();
                        }
                        if cmd.starts_with("APPEND ") {
                            let (_, size) = cmd.rsplit_once('{').unwrap();
                            let size = size.trim_end_matches('}').parse::<usize>().unwrap();
                            write!(stream, "+ ready\r\n").unwrap();
                            let mut literal = vec![0; size + 2];
                            reader.read_exact(&mut literal).unwrap();
                            if capability.contains("APPENDUID-FIXTURE") {
                                write!(stream, "{tag} OK [APPENDUID 12 77] saved\r\n").unwrap();
                                continue;
                            }
                        }
                        if cmd.starts_with(super::UTF8_UID_SEARCH) {
                            if reject_prefix.is_some_and(|prefix| cmd.starts_with(prefix)) {
                                write!(stream, "{tag} NO unsupported charset\r\n").unwrap();
                                continue;
                            }
                            for expected in ["güvercin TEXT {9}\r\n", "İstanbul\r\n"] {
                                write!(stream, "+ continue\r\n").unwrap();
                                let mut literal_line = String::new();
                                reader.read_line(&mut literal_line).unwrap();
                                assert_eq!(literal_line, expected);
                                commands.push(literal_line.trim_end().to_string());
                            }
                        }
                        if cmd.starts_with("UID SEARCH ") {
                            let result = if capability.contains("SINGLE-UID-FIXTURE") || cmd.ends_with("\"unique\"") {
                                " 42"
                            } else if cmd.ends_with("\"missing\"") {
                                ""
                            } else {
                                " 41 42"
                            };
                            write!(stream, "* SEARCH{result}\r\n").unwrap();
                        }
                        if reject_prefix.is_some_and(|prefix| cmd.starts_with(prefix)) {
                            write!(stream, "{tag} NO fixture rejection\r\n").unwrap();
                            continue;
                        }
                        if cmd.starts_with("SELECT ") {
                            write!(stream, "* 1 EXISTS\r\n").unwrap();
                            if cmd != "SELECT \"Unknown validity\"" {
                                write!(stream, "* OK [UIDVALIDITY 7] fixture\r\n").unwrap();
                            }
                        }
                        if cmd.starts_with("LIST ") {
                            write!(stream, "* LIST () \"/\" \"INBOX\"\r\n").unwrap();
                            if capability.contains("LABEL-FIXTURE") {
                                write!(stream, "* LIST () \"/\" \"Etiketler/My Label\"\r\n").unwrap();
                            }
                        }
                        if cmd.starts_with("UID FETCH 42 (UID BODY.PEEK[]") {
                            let raw = "Subject: Replacement\r\nFrom: fixture@example.test\r\n\r\nReplacement body\r\n";
                            write!(stream, "* 1 FETCH (UID 42 BODY[] {{{}}}\r\n{})\r\n", raw.len(), raw).unwrap();
                        }
                        if cmd.starts_with("FETCH 1:1 ") || cmd.starts_with("UID FETCH 41,42 ")
                            || cmd.starts_with("UID FETCH 42 (BODY.PEEK[HEADER.FIELDS ") {
                            let headers = "Subject: Replacement\r\nFrom: fixture@example.test\r\n\r\n";
                            write!(stream, "* 1 FETCH (UID 42 FLAGS () RFC822.SIZE 100 BODY[HEADER.FIELDS (SUBJECT FROM)] {{{}}}\r\n{})\r\n", headers.len(), headers).unwrap();
                        }
                    }
                }
                write!(stream, "{tag} OK success\r\n").unwrap();
            }
            commands
        });
        let stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut client = imap::Client::new(super::ImapTransport::new(stream));
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
    fn build_imap_advanced_query_combines_boolean_terms() {
        let request = AdvancedSearchRequest {
            keywords: Some(r#"from:alice OR subject:"weekly report" AND NOT to:bob"#.into()),
            ..Default::default()
        };
        assert_eq!(
            build_imap_advanced_query(&request),
            r#"OR (FROM "alice") ((SUBJECT "weekly report" NOT (TO "bob")))"#,
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
