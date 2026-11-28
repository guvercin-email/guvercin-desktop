use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::extract::{Json, Path, State};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::{db, error::AppError, imap_session, mail_routes::MailAppState};

#[derive(Deserialize)]
pub struct SpamSettings {
    enabled: bool,
}

#[derive(Deserialize)]
pub struct SpamFeedback {
    key: String,
    spam: bool,
    features: Vec<String>,
}

pub(crate) async fn init(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS spam_settings (
        id INTEGER PRIMARY KEY CHECK(id = 1), enabled BOOLEAN NOT NULL,
        start_uid INTEGER NOT NULL DEFAULT 0, uid_validity INTEGER
    )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS spam_checked (
        uid INTEGER NOT NULL, uid_validity INTEGER NOT NULL, checked BOOLEAN NOT NULL,
        retry_after TEXT, PRIMARY KEY (uid_validity, uid)
    )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS spam_feedback (id INTEGER PRIMARY KEY AUTOINCREMENT, message_key TEXT NOT NULL UNIQUE,
         spam BOOLEAN NOT NULL, features TEXT NOT NULL)",
    ).execute(pool).await?;
    Ok(())
}

pub(crate) async fn enabled(pool: &SqlitePool) -> Result<bool, sqlx::Error> {
    Ok(
        sqlx::query_scalar("SELECT enabled FROM spam_settings WHERE id = 1")
            .fetch_optional(pool)
            .await?
            .unwrap_or(false),
    )
}

pub async fn settings(
    State(state): State<Arc<MailAppState>>,
    Path(account_id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let pool = db::get_user_db_pool(&state._db, account_id).await?;
    Ok(Json(json!({ "enabled": enabled(&pool).await? })))
}

pub async fn update_settings(
    State(state): State<Arc<MailAppState>>,
    Path(account_id): Path<i64>,
    Json(settings): Json<SpamSettings>,
) -> Result<Json<Value>, AppError> {
    let pool = db::get_user_db_pool(&state._db, account_id).await?;
    let lock = state.account_sync_lock(account_id).await;
    let _guard = lock.lock().await;
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO spam_settings (id, enabled, start_uid, uid_validity)
         VALUES (1, ?, COALESCE((SELECT MAX(CAST(uid AS INTEGER)) FROM local_mail_cache WHERE folder = 'INBOX'), 0),
                 (SELECT last_uid_validity FROM sync_checkpoints WHERE folder_path = 'INBOX'))
         ON CONFLICT(id) DO UPDATE SET enabled = excluded.enabled, start_uid = excluded.start_uid,
             uid_validity = excluded.uid_validity WHERE spam_settings.enabled != excluded.enabled",
    ).bind(settings.enabled).execute(&mut *tx).await?;
    if !settings.enabled {
        // Pending moves made by the filter can still be canceled. Processing
        // moves are protected by the same account lock.
        sqlx::query("DELETE FROM offline_sync_queue WHERE status IN ('pending', 'failed') AND action_type = 'move'
            AND json_extract(payload_json, '$.automatic_spam') = 1").execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(Json(json!({ "enabled": settings.enabled })))
}

fn feature_hash(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

pub async fn save_feedback(
    State(state): State<Arc<MailAppState>>,
    Path(account_id): Path<i64>,
    Json(feedback): Json<SpamFeedback>,
) -> Result<Json<Value>, AppError> {
    if feedback.key.len() > 1024
        || feedback.features.len() > 112
        || feedback.features.iter().any(|value| value.len() > 512)
    {
        return Err(AppError::BadRequest("Invalid spam training data".into()));
    }
    let pool = db::get_user_db_pool(&state._db, account_id).await?;
    let hashes: HashSet<_> = feedback
        .features
        .iter()
        .map(|value| feature_hash(value))
        .collect();
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT OR REPLACE INTO spam_feedback (message_key, spam, features) VALUES (?, ?, ?)",
    )
    .bind(feature_hash(&feedback.key))
    .bind(feedback.spam)
    .bind(serde_json::to_string(&hashes).expect("string set is serializable"))
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM spam_feedback WHERE id NOT IN (SELECT id FROM spam_feedback ORDER BY id DESC LIMIT 600)")
        .execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({ "saved": true })))
}

struct Model {
    counts: HashMap<String, [u32; 2]>,
    examples: [u32; 2],
    feedback: HashSet<String>,
}

impl Model {
    async fn load(pool: &SqlitePool) -> Result<Self, sqlx::Error> {
        let rows: Vec<(String, bool, String)> =
            sqlx::query_as("SELECT message_key, spam, features FROM spam_feedback")
                .fetch_all(pool)
                .await?;
        let mut model = Self {
            counts: HashMap::new(),
            examples: [0; 2],
            feedback: HashSet::new(),
        };
        for (key, spam, features) in rows {
            let Ok(features) = serde_json::from_str::<HashSet<String>>(&features) else {
                continue;
            };
            let index = usize::from(!spam);
            model.examples[index] += 1;
            model.feedback.insert(key);
            for feature in features {
                model.counts.entry(feature).or_default()[index] += 1;
            }
        }
        Ok(model)
    }

    fn ready(&self) -> bool {
        self.examples.iter().all(|count| *count >= 3)
    }

    fn has_feedback(&self, mail: &Candidate, generation: u32) -> bool {
        let key = json!(["INBOX", mail.uid, generation]).to_string();
        self.feedback.contains(&feature_hash(&key))
    }

    fn likely_spam(&self, mail: &Candidate, body: &str) -> bool {
        let address = mail.address.trim().to_lowercase();
        let mut features: HashSet<_> = [
            format!("sender:{address}"),
            format!("domain:{}", address.rsplit('@').next().unwrap_or_default()),
        ]
        .into_iter()
        .collect();
        let body_limit = body
            .char_indices()
            .take_while(|(index, _)| *index <= 32_768)
            .last()
            .map(|(index, character)| index + character.len_utf8())
            .unwrap_or_default();
        for (prefix, value, limit) in [
            ("subject", mail.subject.as_str(), 46),
            ("body", &body[..body_limit], 64),
        ] {
            for word in value
                .to_lowercase()
                .split(|character: char| !character.is_alphanumeric())
                .filter(|word| word.chars().nth(2).is_some())
                .take(limit)
            {
                features.insert(format!(
                    "{prefix}:{}",
                    word.chars().take(48).collect::<String>()
                ));
            }
        }
        let mut score = 0.0;
        let mut body_evidence = 0;
        for feature in features {
            let Some([spam, clean]) = self.counts.get(&feature_hash(&feature)) else {
                continue;
            };
            if spam + clean < 2 {
                continue;
            }
            let ratio = (f64::from(*spam) + 1.0)
                / (f64::from(self.examples[0]) + 2.0)
                / ((f64::from(*clean) + 1.0) / (f64::from(self.examples[1]) + 2.0));
            if feature.starts_with("body:") && ratio > 1.0 {
                body_evidence += 1;
            }
            score += ratio.ln();
        }
        body_evidence >= 2 && score >= 10.0_f64.ln()
    }
}

#[derive(sqlx::FromRow)]
struct Candidate {
    uid: String,
    address: String,
    subject: String,
    plain_body: Option<String>,
    html_body: Option<String>,
}

pub(crate) async fn scan_inbox(
    state: &Arc<MailAppState>,
    pool: &SqlitePool,
    account_id: i64,
    generation: u32,
) -> Result<(), AppError> {
    if !enabled(pool).await? {
        return Ok(());
    }
    let model = Model::load(pool).await?;
    if !model.ready() {
        return Ok(());
    }
    let candidates: Vec<Candidate> = sqlx::query_as(
        "SELECT mail.uid, COALESCE(mail.sender_address, '') AS address, COALESCE(mail.subject, '') AS subject,
                mail.plain_body, mail.html_body
         FROM local_mail_cache mail CROSS JOIN spam_settings preferences
         LEFT JOIN spam_checked c ON c.uid_validity = mail.uid_validity AND c.uid = CAST(mail.uid AS INTEGER)
         WHERE mail.folder = 'INBOX' AND mail.uid_validity = ? AND mail.seen = 0
             AND COALESCE(mail.size_bytes, 0) BETWEEN 1 AND 2000000
             AND (preferences.uid_validity IS NOT mail.uid_validity OR CAST(mail.uid AS INTEGER) > preferences.start_uid)
             AND (c.uid IS NULL OR (c.checked = 0 AND c.retry_after <= CURRENT_TIMESTAMP))
         ORDER BY CAST(mail.uid AS INTEGER) ASC LIMIT 25",
    ).bind(generation).fetch_all(pool).await?;
    if candidates.is_empty() {
        return Ok(());
    }
    let imap = state.imap.clone();
    let destination = tokio::task::spawn_blocking(move || {
        let entries = imap_session::list_mailboxes_detailed(&imap, account_id);
        entries
            .iter()
            .find(|entry| entry.selectable && entry.role.as_deref() == Some("junk"))
            .or_else(|| {
                entries.iter().find(|entry| {
                    entry.selectable
                        && ["spam", "junk", "junk e-mail", "[gmail]/spam"]
                            .contains(&entry.name.to_lowercase().as_str())
                })
            })
            .map(|entry| entry.name.clone())
    })
    .await
    .map_err(|error| AppError::BadRequest(error.to_string()))?;
    let Some(destination) = destination else {
        return Ok(());
    };

    for mail in candidates {
        let mut checked = true;
        let mut spam = false;
        if !model.has_feedback(&mail, generation) {
            let body = tokio::task::spawn_blocking({
                let imap = state.imap.clone();
                let uid = mail.uid.clone();
                let cached = mail
                    .plain_body
                    .as_ref()
                    .filter(|body| !body.trim().is_empty())
                    .cloned()
                    .or_else(|| {
                        mail.html_body
                            .as_ref()
                            .filter(|body| !body.trim().is_empty())
                            .map(|body| html_text(body))
                    });
                move || {
                    cached.or_else(|| {
                        let raw = imap_session::fetch_mail_raw_in_mailbox(
                            &imap,
                            account_id,
                            "INBOX",
                            &uid,
                            Some(generation),
                        )?;
                        let content = imap_session::parse_mail_content(uid, &raw);
                        Some(if content.plain_body.trim().is_empty() {
                            html_text(&content.html_body)
                        } else {
                            content.plain_body
                        })
                    })
                }
            })
            .await
            .map_err(|error| AppError::BadRequest(error.to_string()))?;
            checked = body.is_some();
            spam = body
                .as_deref()
                .is_some_and(|body| model.likely_spam(&mail, body));
        }
        let mut tx = pool.begin().await?;
        if spam {
            sqlx::query(
                "INSERT INTO offline_sync_queue (action_type, target_uid, target_folder, payload_json, uid_validity)
                 SELECT 'move', ?, 'INBOX', ?, ? WHERE NOT EXISTS (
                    SELECT 1 FROM offline_sync_queue WHERE target_folder = 'INBOX' AND target_uid = ? AND uid_validity = ?
                    AND action_type IN ('move', 'delete') AND status IN ('pending', 'processing', 'failed', 'dead')
                 )",
            ).bind(&mail.uid).bind(json!({ "destination": destination, "automatic_spam": true }).to_string())
                .bind(generation).bind(&mail.uid).bind(generation).execute(&mut *tx).await?;
        }
        sqlx::query(
            "INSERT OR REPLACE INTO spam_checked (uid, uid_validity, checked, retry_after)
             VALUES (CAST(? AS INTEGER), ?, ?, DATETIME('now', '+5 minutes'))",
        )
        .bind(&mail.uid)
        .bind(generation)
        .bind(checked)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
    }
    Ok(())
}

fn html_text(body: &str) -> String {
    scraper::Html::parse_fragment(body)
        .root_element()
        .text()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_suspect_sender_without_body_evidence_will_not_trigger_an_automatic_move() {
        let mut counts = HashMap::new();
        for feature in [
            "sender:offer@example.test",
            "domain:example.test",
            "body:claim",
            "body:reward",
        ] {
            counts.insert(feature_hash(feature), [3, 0]);
        }
        let model = Model {
            counts,
            examples: [3, 3],
            feedback: HashSet::new(),
        };
        let mail = Candidate {
            uid: "17".into(),
            address: "offer@example.test".into(),
            subject: "New mail".into(),
            plain_body: None,
            html_body: None,
        };
        assert!(model.likely_spam(&mail, "Claim your reward"));
        assert!(!model.likely_spam(&mail, "A meeting update"));
    }
}
