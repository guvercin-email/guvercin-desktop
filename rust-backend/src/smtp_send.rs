use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use lettre::{
    message::{header, header::ContentType, Attachment, MultiPart, SinglePart},
    transport::smtp::authentication::{Credentials, Mechanism, DEFAULT_MECHANISMS},
    transport::smtp::{
        client::{AsyncSmtpConnection, TlsParameters},
        commands::{Data, Mail, Rcpt},
        extension::{ClientId, Extension, MailBodyParameter, MailParameter},
    },
    Message,
};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use tracing::{error, info};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum OutgoingMailFormat {
    #[default]
    Plain,
    Html,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum OutgoingAttachmentDisposition {
    #[default]
    Attachment,
    Inline,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct OutgoingAttachment {
    pub filename: String,
    pub content_type: String,
    pub data_base64: String,
    #[serde(default)]
    pub disposition: OutgoingAttachmentDisposition,
    #[serde(default)]
    pub content_id: Option<String>,
}

enum MessageBody {
    Single(SinglePart),
    Multi(MultiPart),
}

fn is_loopback_smtp_host(host: &str) -> bool {
    let host = host.trim();
    if host.eq_ignore_ascii_case("localhost") || host == "::1" {
        return true;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return ip.is_loopback();
    }
    host.starts_with("127.")
}

fn build_local_tls_parameters() -> Result<TlsParameters, String> {
    TlsParameters::builder("localhost".to_string())
        .dangerous_accept_invalid_hostnames(true)
        .dangerous_accept_invalid_certs(true)
        .build()
        .map_err(|e| format!("Failed to build local TLS parameters: {}", e))
}

fn parse_content_type(content_type: &str) -> Result<ContentType, String> {
    ContentType::parse(content_type.trim())
        .map_err(|e| format!("Invalid content type '{}': {}", content_type, e))
}

fn build_plain_part(plain_body: &str) -> SinglePart {
    SinglePart::builder()
        .header(ContentType::TEXT_PLAIN)
        .body(plain_body.to_string())
}

fn build_html_part(html_body: &str) -> SinglePart {
    SinglePart::builder()
        .header(ContentType::TEXT_HTML)
        .body(html_body.to_string())
}

fn decode_attachment_bytes(attachment: &OutgoingAttachment) -> Result<Vec<u8>, String> {
    BASE64_STANDARD
        .decode(attachment.data_base64.trim())
        .map_err(|e| {
            format!(
                "Attachment '{}' is not valid base64: {}",
                attachment.filename, e
            )
        })
}

fn build_attachment_part(
    attachment: &OutgoingAttachment,
    force_regular_attachment: bool,
) -> Result<SinglePart, String> {
    let body = decode_attachment_bytes(attachment)?;
    let content_type = parse_content_type(&attachment.content_type)?;
    let filename = if attachment.filename.trim().is_empty() {
        "attachment".to_string()
    } else {
        attachment.filename.trim().to_string()
    };

    if !force_regular_attachment
        && matches!(
            attachment.disposition,
            OutgoingAttachmentDisposition::Inline
        )
    {
        let content_id = attachment
            .content_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("Inline attachment '{}' is missing content_id", filename))?;
        Ok(
            Attachment::new_inline_with_name(content_id.to_string(), filename)
                .body(body, content_type),
        )
    } else {
        Ok(Attachment::new(filename).body(body, content_type))
    }
}

fn build_message_body(
    format: OutgoingMailFormat,
    plain_body: &str,
    html_body: &str,
    attachments: &[OutgoingAttachment],
) -> Result<MessageBody, String> {
    let plain_part = build_plain_part(plain_body);

    if matches!(format, OutgoingMailFormat::Plain) || html_body.trim().is_empty() {
        if attachments.is_empty() {
            return Ok(MessageBody::Single(plain_part));
        }

        let mut mixed = MultiPart::mixed().singlepart(plain_part);
        for attachment in attachments {
            mixed = mixed.singlepart(build_attachment_part(attachment, true)?);
        }
        return Ok(MessageBody::Multi(mixed));
    }

    let mut inline_attachments = Vec::new();
    let mut regular_attachments = Vec::new();
    for attachment in attachments {
        if matches!(
            attachment.disposition,
            OutgoingAttachmentDisposition::Inline
        ) {
            inline_attachments.push(attachment);
        } else {
            regular_attachments.push(attachment);
        }
    }

    let alternative = if inline_attachments.is_empty() {
        MultiPart::alternative()
            .singlepart(plain_part)
            .singlepart(build_html_part(html_body))
    } else {
        let mut related = MultiPart::related().singlepart(build_html_part(html_body));
        for attachment in inline_attachments {
            related = related.singlepart(build_attachment_part(attachment, false)?);
        }
        MultiPart::alternative()
            .singlepart(plain_part)
            .multipart(related)
    };

    if regular_attachments.is_empty() {
        return Ok(MessageBody::Multi(alternative));
    }

    let mut mixed = MultiPart::mixed().multipart(alternative);
    for attachment in regular_attachments {
        mixed = mixed.singlepart(build_attachment_part(attachment, false)?);
    }
    Ok(MessageBody::Multi(mixed))
}

/// Everything that makes up the outgoing message itself, kept apart from the
/// server it is handed to so both builders and the sender take one argument.
pub struct OutgoingMessage<'a> {
    pub from: &'a str,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: &'a str,
    pub format: OutgoingMailFormat,
    pub html_body: &'a str,
    pub plain_body: &'a str,
    pub attachments: &'a [OutgoingAttachment],
    /// Extra headers written verbatim, e.g. the draft tracking key.
    pub raw_headers: &'a [(String, String)],
}

/// The SMTP endpoint and the credentials used to authenticate against it.
pub struct SmtpServer<'a> {
    pub host: &'a str,
    pub port: u16,
    pub ssl_mode: &'a str,
    pub email: &'a str,
    /// An OAuth2 access token when `xoauth2` is set, a password otherwise.
    pub password: &'a str,
    pub xoauth2: bool,
}

fn build_message(msg: OutgoingMessage<'_>) -> Result<Message, String> {
    let mut builder = Message::builder()
        .from(
            msg.from
                .parse()
                .map_err(|e| format!("Invalid 'from' address: {}", e))?,
        )
        .subject(msg.subject);

    for addr_str in msg.to {
        let addr = addr_str
            .parse()
            .map_err(|e| format!("Invalid 'to' address: {}", e))?;
        builder = builder.to(addr);
    }
    for addr_str in msg.cc {
        let addr = addr_str
            .parse()
            .map_err(|e| format!("Invalid 'cc' address: {}", e))?;
        builder = builder.cc(addr);
    }
    for addr_str in msg.bcc {
        let addr = addr_str
            .parse()
            .map_err(|e| format!("Invalid 'bcc' address: {}", e))?;
        builder = builder.bcc(addr);
    }
    for (name, value) in msg.raw_headers {
        let header_name = header::HeaderName::new_from_ascii(name.trim().to_string())
            .map_err(|e| format!("Invalid header name '{}': {}", name, e))?;
        builder = builder.raw_header(header::HeaderValue::new(
            header_name,
            value.trim().to_string(),
        ));
    }

    match build_message_body(msg.format, msg.plain_body, msg.html_body, msg.attachments)? {
        MessageBody::Single(part) => builder
            .singlepart(part)
            .map_err(|e| format!("Failed to build message: {}", e)),
        MessageBody::Multi(part) => builder
            .multipart(part)
            .map_err(|e| format!("Failed to build message: {}", e)),
    }
}

pub fn build_rfc822_message(msg: OutgoingMessage<'_>) -> Result<Vec<u8>, String> {
    Ok(build_message(msg)?.formatted())
}

pub struct SmtpSendError {
    pub message: String,
    pub delivery_uncertain: bool,
}

impl From<String> for SmtpSendError {
    fn from(message: String) -> Self {
        Self {
            message,
            delivery_uncertain: false,
        }
    }
}

impl From<&str> for SmtpSendError {
    fn from(message: &str) -> Self {
        message.to_string().into()
    }
}

/// Sends `msg` through `server`. When `server.xoauth2` is set the SMTP
/// `XOAUTH2` mechanism is used and `server.password` is an access token
/// (Gmail); otherwise plain credentials are used.
pub async fn send_mail(server: SmtpServer<'_>, msg: OutgoingMessage<'_>) -> Result<(), SmtpSendError> {
    let SmtpServer {
        host: smtp_host,
        port: smtp_port,
        ssl_mode,
        email,
        password,
        xoauth2,
    } = server;

    info!(
        "Preparing to send email to {} recipients via {}:{} ({})",
        msg.to.len() + msg.cc.len() + msg.bcc.len(),
        smtp_host,
        smtp_port,
        ssl_mode
    );

    if msg.to.is_empty() && msg.cc.is_empty() && msg.bcc.is_empty() {
        return Err("No recipients specified".into());
    }

    let upper_ssl_mode = ssl_mode.to_uppercase();
    if matches!(upper_ssl_mode.as_str(), "SSL" | "STARTTLS")
        && smtp_host.parse::<IpAddr>().is_ok()
        && !is_loopback_smtp_host(smtp_host)
    {
        return Err(format!(
            "SMTP mode {} requires a hostname, not an IP address ({}). Use a real hostname like smtp.example.com. Loopback hosts are supported separately.",
            ssl_mode, smtp_host
        )
        .into());
    }

    let email_message = build_message(msg)?;

    let tls = if matches!(upper_ssl_mode.as_str(), "SSL" | "STARTTLS") {
        Some(if is_loopback_smtp_host(smtp_host) {
            build_local_tls_parameters()?
        } else {
            TlsParameters::new(smtp_host.to_owned()).map_err(|error| error.to_string())?
        })
    } else {
        None
    };

    let timeout = std::time::Duration::from_secs(30);
    let hello = ClientId::default();
    let (implicit_tls, upgrade_tls) = if upper_ssl_mode == "SSL" {
        (tls, None)
    } else {
        (None, tls)
    };
    let mut connection = tokio::time::timeout(timeout, async {
        let mut connection = AsyncSmtpConnection::connect_tokio1(
            (smtp_host, smtp_port), Some(timeout), &hello, implicit_tls, None,
        ).await?;
        if let Some(tls) = upgrade_tls {
            connection.starttls(tls, &hello).await?;
        }
        let credentials = Credentials::new(email.to_owned(), password.to_owned());
        let mechanisms = if xoauth2 { &[Mechanism::Xoauth2] } else { DEFAULT_MECHANISMS };
        connection.auth(mechanisms, &credentials).await?;
        Ok::<_, lettre::transport::smtp::Error>(connection)
    }).await
        .map_err(|_| "SMTP connection or authentication timed out".to_string())?
        .map_err(|error| format!("SMTP connection or authentication failed: {error}"))?;

    let formatted = email_message.formatted();
    let envelope = email_message.envelope();
    let mut mail_options = Vec::new();
    if envelope.from().into_iter().chain(envelope.to()).any(|address| !AsRef::<str>::as_ref(address).is_ascii()) {
        if !connection.server_info().supports_feature(Extension::SmtpUtfEight) {
            return Err("SMTP server does not support SMTPUTF8".into());
        }
        mail_options.push(MailParameter::SmtpUtfEight);
    }
    if !formatted.is_ascii() {
        if !connection.server_info().supports_feature(Extension::EightBitMime) {
            return Err("SMTP server does not support 8BITMIME".into());
        }
        mail_options.push(MailParameter::Body(MailBodyParameter::EightBitMime));
    }

    let data_ready = tokio::time::timeout(timeout, async {
        connection.command(Mail::new(envelope.from().cloned(), mail_options)).await?;
        for recipient in envelope.to() {
            connection.command(Rcpt::new(recipient.clone(), Vec::new())).await?;
        }
        connection.command(Data).await
    }).await
        .map_err(|_| "SMTP envelope or DATA command timed out".to_string())?
        .map_err(|error| format!("SMTP rejected the envelope or DATA command: {error}"))?;
    if u16::from(data_ready.code()) != 354 {
        return Err(format!("SMTP did not accept message data: {}", data_ready.code()).into());
    }

    match tokio::time::timeout(timeout, connection.message(&formatted)).await {
        Ok(Ok(_)) => {
            info!("Email sent successfully");
            Ok(())
        }
        Ok(Err(error)) => {
            let delivery_uncertain = error.status().is_none() && !error.is_client();
            error!("SMTP send failed: {error}");
            Err(SmtpSendError {
                message: if delivery_uncertain {
                    format!("SMTP delivery could not be confirmed: {error}")
                } else {
                    format!("SMTP send failed: {error}")
                },
                delivery_uncertain,
            })
        }
        Err(_) => Err(SmtpSendError {
            message: "SMTP delivery could not be confirmed before the timeout".into(),
            delivery_uncertain: true,
        }),
    }

}

#[cfg(test)]
mod tests {
    use super::{
        build_message, OutgoingAttachment, OutgoingAttachmentDisposition, OutgoingMailFormat,
        OutgoingMessage,
    };

    #[tokio::test]
    async fn smtp_distinguishes_rejection_from_lost_delivery_confirmation() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::TcpListener;

        #[derive(Clone, Copy)]
        enum SmtpOutcome {
            RejectAuthentication,
            DropBeforeData,
            AfterData(Option<&'static str>),
        }

        for (outcome, expected_uncertainty, expected_messages) in [
            (SmtpOutcome::RejectAuthentication, false, 0),
            (SmtpOutcome::DropBeforeData, false, 0),
            (SmtpOutcome::AfterData(Some("451 try again later\r\n")), false, 1),
            (SmtpOutcome::AfterData(Some("550 message rejected\r\n")), false, 1),
            (SmtpOutcome::AfterData(None), true, 1),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let (reader, mut writer) = socket.into_split();
                let mut lines = BufReader::new(reader).lines();
                writer.write_all(b"220 localhost SMTP\r\n").await.unwrap();
                while let Some(line) = lines.next_line().await.unwrap() {
                    let response = if line.starts_with("EHLO ") {
                        "250-localhost\r\n250 AUTH PLAIN\r\n"
                    } else if line.starts_with("AUTH PLAIN ") {
                        if matches!(outcome, SmtpOutcome::RejectAuthentication) {
                            writer.write_all(b"535 invalid credentials\r\n").await.unwrap();
                            return 0;
                        }
                        "235 authenticated\r\n"
                    } else if line.starts_with("MAIL FROM:") || line.starts_with("RCPT TO:") {
                        if matches!(outcome, SmtpOutcome::DropBeforeData) {
                            return 0;
                        }
                        "250 OK\r\n"
                    } else if line == "DATA" {
                        writer.write_all(b"354 send data\r\n").await.unwrap();
                        loop {
                            let line = lines.next_line().await.unwrap().expect("SMTP message ended early");
                            if line == "." {
                                break;
                            }
                        }
                        if let SmtpOutcome::AfterData(Some(reply)) = outcome {
                            writer.write_all(reply.as_bytes()).await.unwrap();
                        }
                        return 1;
                    } else {
                        panic!("Unexpected SMTP command");
                    };
                    writer.write_all(response.as_bytes()).await.unwrap();
                }
                panic!("Client closed before sending the message");
            });
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                super::send_mail(
                    super::SmtpServer {
                        host: "127.0.0.1", port, ssl_mode: "NONE",
                        email: "sender@example.test", password: "fixture", xoauth2: false,
                    },
                    OutgoingMessage {
                        from: "sender@example.test", to: vec!["recipient@example.test".into()],
                        cc: vec![], bcc: vec![], subject: "Fixture", format: OutgoingMailFormat::Plain,
                        html_body: "", plain_body: "Fixture body", attachments: &[], raw_headers: &[],
                    },
                ),
            )
            .await
            .expect("SMTP test timeout");
            assert_eq!(
                result.expect_err("delivery error").delivery_uncertain,
                expected_uncertainty,
            );
            assert_eq!(server.await.unwrap(), expected_messages);
        }
    }

    fn sample_attachment(disposition: OutgoingAttachmentDisposition) -> OutgoingAttachment {
        OutgoingAttachment {
            filename: "sample.txt".to_string(),
            content_type: "text/plain".to_string(),
            data_base64: "SGVsbG8=".to_string(),
            disposition,
            content_id: Some("cid-123".to_string()),
        }
    }

    #[test]
    fn builds_plain_only_message() {
        let message = build_message(OutgoingMessage {
            from: "sender@example.com",
            to: vec!["to@example.com".to_string()],
            cc: vec![],
            bcc: vec![],
            subject: "Plain",
            format: OutgoingMailFormat::Plain,
            html_body: "",
            plain_body: "Hello plain",
            attachments: &[],
            raw_headers: &[],
        })
        .expect("plain message");

        let bytes = message.formatted();
        let formatted = String::from_utf8_lossy(&bytes);
        assert!(formatted.contains("Content-Type: text/plain"));
        assert!(!formatted.contains("multipart/alternative"));
        assert!(!formatted.contains("text/html"));
    }

    #[test]
    fn builds_html_alternative_message() {
        let message = build_message(OutgoingMessage {
            from: "sender@example.com",
            to: vec!["to@example.com".to_string()],
            cc: vec![],
            bcc: vec![],
            subject: "HTML",
            format: OutgoingMailFormat::Html,
            html_body: "<p>Hello html</p>",
            plain_body: "Hello html",
            attachments: &[],
            raw_headers: &[],
        })
        .expect("html message");

        let bytes = message.formatted();
        let formatted = String::from_utf8_lossy(&bytes);
        assert!(formatted.contains("multipart/alternative"));
        assert!(formatted.contains("Content-Type: text/html"));
        assert!(formatted.contains("<p>Hello html</p>"));
    }

    #[test]
    fn builds_html_message_with_inline_attachment() {
        let message = build_message(OutgoingMessage {
            from: "sender@example.com",
            to: vec!["to@example.com".to_string()],
            cc: vec![],
            bcc: vec![],
            subject: "Inline",
            format: OutgoingMailFormat::Html,
            html_body: "<p><img src=\"cid:cid-123\"></p>",
            plain_body: "Inline",
            attachments: &[sample_attachment(OutgoingAttachmentDisposition::Inline)],
            raw_headers: &[],
        })
        .expect("inline html message");

        let bytes = message.formatted();
        let formatted = String::from_utf8_lossy(&bytes);
        let related_index = formatted
            .find("multipart/related")
            .expect("related multipart");
        let html_index = formatted
            .find("Content-Type: text/html")
            .expect("html part");
        let cid_index = formatted.find("Content-ID: <cid-123>").expect("cid part");
        assert!(formatted.contains("multipart/related"));
        assert!(formatted.contains("Content-ID: <cid-123>"));
        assert!(formatted.contains("Content-Disposition: inline; filename=\"sample.txt\""));
        assert!(html_index < cid_index);
        assert!(related_index < cid_index);
    }

    #[test]
    fn builds_html_message_with_inline_and_regular_attachments() {
        let message = build_message(OutgoingMessage {
            from: "sender@example.com",
            to: vec!["to@example.com".to_string()],
            cc: vec![],
            bcc: vec![],
            subject: "Mixed",
            format: OutgoingMailFormat::Html,
            html_body: "<p><img src=\"cid:cid-123\"></p>",
            plain_body: "Inline",
            attachments: &[
                sample_attachment(OutgoingAttachmentDisposition::Inline),
                OutgoingAttachment {
                    filename: "doc.pdf".to_string(),
                    content_type: "application/pdf".to_string(),
                    data_base64: "SGVsbG8=".to_string(),
                    disposition: OutgoingAttachmentDisposition::Attachment,
                    content_id: None,
                },
            ],
            raw_headers: &[],
        })
        .expect("mixed message");

        let bytes = message.formatted();
        let formatted = String::from_utf8_lossy(&bytes);
        assert!(formatted.contains("multipart/mixed"));
        assert!(formatted.contains("multipart/related"));
        assert!(formatted.contains("filename=\"doc.pdf\""));
    }

    #[test]
    fn builds_message_with_raw_headers() {
        let message = build_message(OutgoingMessage {
            from: "sender@example.com",
            to: vec!["to@example.com".to_string()],
            cc: vec![],
            bcc: vec![],
            subject: "Headers",
            format: OutgoingMailFormat::Plain,
            html_body: "",
            plain_body: "Body",
            attachments: &[],
            raw_headers: &[("In-Reply-To".to_string(), "<msg-1@example.com>".to_string())],
        })
        .expect("message with raw headers");

        let bytes = message.formatted();
        let formatted = String::from_utf8_lossy(&bytes);
        assert!(formatted.contains("In-Reply-To: <msg-1@example.com>"));
    }
}
