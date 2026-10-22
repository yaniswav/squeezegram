//! What the Telegram commands do, kept out of `commands.rs` so it runs in tests on the mock:
//! destinations cache (`destinations_refresh`), pre-send check (`destination_check`),
//! reconciliation (`item_reconcile`, read only), what a duplicate was sent as
//! (`item_duplicate`, read only) and the `auth://state` payload.

use std::path::Path;
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};

use super::select::TransportKind;
use super::{
    AuthState, Dialog, MediaKind, Peer, PeerKind, SentCandidate, SentProbe, Target,
    TelegramTransport, TransportError, Verdict, seconds_up,
};
use crate::media::plan::{Kind, Plan};
use crate::queue::orchestrator::Orchestrator;
use crate::queue::store::{
    Destination, HOLD_DUPLICATE, HistoryFilter, HistoryStatus, Item, NewDestination, SendMode,
};
use crate::queue::{QueueError, State, iso, parse_iso};

/// `loadChats` page size; about 84 chats on the developer's own account.
pub const DIALOG_LIMIT: usize = 500;
/// Margin before the item was queued, for clock differences with the server.
const RECONCILE_SLACK_S: i64 = 120;

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error(transparent)]
    Queue(#[from] QueueError),
    #[error(transparent)]
    Transport(#[from] TransportError),
}

pub type Result<T> = std::result::Result<T, ServiceError>;

/// `Target` of a destination row; `None` if the row is not one this app wrote.
pub fn target_of(destination: &Destination) -> Option<Target> {
    let kind = PeerKind::parse(&destination.peer_kind)?;
    let topic_id = match destination.topic_id {
        0 => None,
        id => Some(i32::try_from(id).ok()?),
    };
    Some(Target {
        peer: Peer {
            kind,
            raw_id: destination.peer_id,
        },
        topic_id,
    })
}

fn bad_destination(destination: &Destination) -> ServiceError {
    QueueError::InvalidValue(format!("destination {}", destination.id)).into()
}

pub fn mode_kind(mode: SendMode) -> MediaKind {
    match mode {
        SendMode::Video => MediaKind::Video,
        SendMode::Document => MediaKind::Document,
        SendMode::Photo => MediaKind::Photo,
    }
}

pub fn plan_of(item: &Item) -> Option<Plan> {
    serde_json::from_str(item.plan_json.as_deref()?).ok()
}

/// What actually goes out: a photo plan is a photo whatever the mode, and a photo that
/// Telegram would refuse (ratio) is left as is and sent as a file.
pub fn item_kind(item: &Item, plan: Option<&Plan>) -> MediaKind {
    if item.send_mode == SendMode::Document {
        return MediaKind::Document;
    }
    match plan {
        Some(plan) if plan.kind == Kind::Photo => MediaKind::Photo,
        Some(plan) if plan.note.as_deref() == Some("photo_ratio") => MediaKind::Document,
        _ => mode_kind(item.send_mode),
    }
}

// ---------- destinations ----------

/// Row of a chat (`topic_id = 0`). For a forum it is the container its topics are listed
/// under; sending goes to a topic (General is topic 1).
fn chat_row(dialog: &Dialog) -> NewDestination {
    NewDestination {
        peer_kind: dialog.peer.kind.as_str().to_owned(),
        peer_id: dialog.peer.raw_id,
        topic_id: 0,
        is_forum: dialog.is_forum,
        is_broadcast: dialog.is_broadcast,
        is_saved: dialog.is_saved_messages,
        title: dialog.title.clone(),
        kind: dialog.kind.as_str().to_owned(),
        archived: dialog.archived,
        folders: dialog.folders.clone(),
        can_write: dialog.can_write,
        listed: dialog.listed,
        topic_closed: false,
    }
}

/// `destinations_refresh`: one row per chat of the main list, the archive and every folder
/// (deduplicated by the transport). Forum topics are loaded on demand (`load_topics`). Local
/// settings (alias, favorite…) are kept; destinations gone from the lists stay with
/// `listed = false` (items refer to them).
pub async fn refresh_destinations(
    transport: &dyn TelegramTransport,
    queue: &Orchestrator,
) -> Result<Vec<Destination>> {
    let started = std::time::Instant::now();
    let dialogs = transport.list_dialogs(DIALOG_LIMIT).await?;
    let (deleted, dialogs): (Vec<Dialog>, Vec<Dialog>) =
        dialogs.into_iter().partition(|d| d.deleted);
    let rows: Vec<NewDestination> = dialogs.iter().map(chat_row).collect();
    let deleted: Vec<(String, i64)> = deleted
        .iter()
        .map(|d| (d.peer.kind.as_str().to_owned(), d.peer.raw_id))
        .collect();
    // Counts only: chat titles, folder names and ids are personal data.
    tracing::info!(
        event = "destinations_refreshed",
        dur_ms = crate::logging::ms_since(started),
        dialogs = dialogs.len(),
        destinations = rows.len(),
        forums = rows.iter().filter(|r| r.is_forum).count(),
        archived = rows.iter().filter(|r| r.archived).count(),
        in_folders = rows.iter().filter(|r| !r.folders.is_empty()).count(),
        "destinations refreshed from Telegram"
    );
    Ok(queue.with_store(|store| {
        store.replace_listed(&rows)?;
        // Deleted accounts: out of the cache (cached before they were recognised).
        let (removed, kept) = store.purge_deleted(&deleted)?;
        tracing::info!(
            event = "deleted_accounts_purged",
            deleted_accounts = deleted.len(),
            removed,
            kept,
            "deleted accounts dropped from the destinations"
        );
        store.list_destinations()
    })?)
}

/// At most this many chats per `destinations_search`.
pub const SEARCH_LIMIT: usize = 30;

/// `destinations_search(query)`, read only on Telegram: chats found by title or username,
/// added to the cache (new ones `listed = false`) and returned in the transport's order.
pub async fn search_destinations(
    transport: &dyn TelegramTransport,
    queue: &Orchestrator,
    query: &str,
) -> Result<Vec<Destination>> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let dialogs = transport.search_chats(query, SEARCH_LIMIT).await?;
    let found = queue.with_store(|store| {
        let mut found = Vec::with_capacity(dialogs.len());
        for dialog in &dialogs {
            let id = store.upsert_found(&chat_row(dialog))?;
            found.push(store.get_destination(id)?);
        }
        Ok(found)
    })?;
    tracing::info!(
        event = "destinations_searched",
        query_chars = query.chars().count(),
        results = found.len(),
        unlisted = found.iter().filter(|d| !d.listed).count(),
        forums = found.iter().filter(|d| d.is_forum).count(),
        "destinations found by search"
    );
    Ok(found)
}

/// `destination_topics(destination_id)`: the topics of a forum, loaded when the user opens
/// it. One row per topic (`Chat › Topic`), with the chat's list flags; closed topics are
/// flagged (`topic_closed`). Returned in TDLib's order (General first when TDLib puts it
/// there). Topics gone from Telegram stay in the cache.
pub async fn load_topics(
    transport: &dyn TelegramTransport,
    queue: &Orchestrator,
    destination_id: i64,
) -> Result<Vec<Destination>> {
    let chat = queue.with_store(|store| store.get_destination(destination_id))?;
    let target = target_of(&chat).ok_or_else(|| bad_destination(&chat))?;
    if !chat.is_forum || target.topic_id.is_some() {
        return Err(bad_destination(&chat));
    }
    let topics = transport.list_topics(&target.peer).await?;
    let chat_title = chat.title.clone();
    Ok(queue.with_store(|store| {
        let mut rows = Vec::with_capacity(topics.len());
        for topic in &topics {
            let id = store.upsert_destination(&NewDestination {
                peer_kind: chat.peer_kind.clone(),
                peer_id: chat.peer_id,
                topic_id: i64::from(topic.id),
                is_forum: true,
                is_broadcast: chat.is_broadcast,
                is_saved: false,
                title: format!("{chat_title} › {}", topic.title),
                kind: chat.kind.clone(),
                archived: chat.archived,
                folders: chat.folders.clone(),
                can_write: chat.can_write,
                listed: chat.listed,
                topic_closed: topic.is_closed,
            })?;
            rows.push(store.get_destination(id)?);
        }
        Ok(rows)
    })?)
}

/// Result of `destination_check`. `code`/`params`: why it is refused
/// (`TG_WRITE_FORBIDDEN{reason}`, `TG_PAID_MESSAGES{stars}`), or a warning when allowed
/// (`TG_SLOWMODE{retry_after}`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DestinationCheck {
    pub allowed: bool,
    pub code: Option<&'static str>,
    pub params: Value,
    pub slow_mode_delay_s: Option<u64>,
    pub slow_mode_wait_s: Option<u64>,
    pub checked_at: String,
}

pub async fn check_destination(
    transport: &dyn TelegramTransport,
    destination: &Destination,
    mode: SendMode,
) -> Result<DestinationCheck> {
    let target = target_of(destination).ok_or_else(|| bad_destination(destination))?;
    let check = transport.can_send(&target, mode_kind(mode)).await?;
    let wait = check
        .slow_mode
        .as_ref()
        .map_or(Duration::ZERO, |s| s.wait_remaining);
    let (allowed, code, params) = match &check.verdict {
        Verdict::Denied(forbidden) => {
            let (code, params) = forbidden.code_and_params();
            (false, Some(code), params)
        }
        Verdict::Allowed if !wait.is_zero() => (
            true,
            Some("TG_SLOWMODE"),
            json!({ "retry_after": seconds_up(wait) }),
        ),
        Verdict::Allowed => (true, None, json!({})),
    };
    let reason = json!({ "params": params }).to_string();
    tracing::info!(
        event = "destination_check",
        destination_id = destination.id,
        chat = crate::logging::redact::chat(target.peer.tdlib_chat_id()).as_str(),
        topic_id = target.topic_id,
        send_mode = mode.as_str(),
        allowed,
        error_code = code,
        json = reason.as_str(),
        "destination checked"
    );
    Ok(DestinationCheck {
        allowed,
        code,
        params,
        slow_mode_delay_s: check.slow_mode.as_ref().map(|s| s.delay.as_secs()),
        slow_mode_wait_s: check
            .slow_mode
            .as_ref()
            .map(|s| seconds_up(s.wait_remaining)),
        checked_at: iso(chrono::DateTime::<chrono::Utc>::from(check.checked_at)),
    })
}

// ---------- duplicates ----------

/// The earlier send of the content an item is held for (`item_duplicate`): when, where, and
/// the link of that message when Telegram has one (supergroups and channels).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DuplicateInfo {
    pub sent_at: String,
    pub destination_id: i64,
    pub message_id: Option<i64>,
    /// Always `https://t.me/…`.
    pub link: Option<String>,
}

/// `item_duplicate`, read only: the last `sent` history line of the item's content and
/// destination. `None` when the item is not held as a duplicate, or the line is gone (history
/// cleared). A link that cannot be read (signed out, offline) leaves the date and destination.
pub async fn duplicate_info(
    transport: &dyn TelegramTransport,
    queue: &Orchestrator,
    item_id: i64,
) -> Result<Option<DuplicateInfo>> {
    let found = queue.with_store(|store| {
        let item = store.get_item(item_id)?;
        let Some(content_id) = item
            .content_id
            .filter(|_| item.hold_reason.as_deref() == Some(HOLD_DUPLICATE))
        else {
            return Ok(None);
        };
        let filter = HistoryFilter {
            destination_id: Some(item.destination_id),
            status: Some(HistoryStatus::Sent),
            content_id: Some(content_id),
            limit: Some(1),
            ..HistoryFilter::default()
        };
        let Some(entry) = store.query_history(&filter)?.into_iter().next() else {
            return Ok(None);
        };
        let destination = store.get_destination(entry.destination_id)?;
        Ok(Some((entry, destination)))
    })?;
    let Some((entry, destination)) = found else {
        return Ok(None);
    };
    let link = match (entry.message_id, target_of(&destination)) {
        (Some(message_id), Some(target)) => {
            match transport.message_link(&target, message_id).await {
                Ok(link) => link,
                Err(error) => {
                    tracing::warn!(
                        event = "message_link_failed",
                        item_id,
                        destination_id = destination.id,
                        error_code = error.code_and_params().0,
                        error = %error,
                        "link of the earlier message unavailable"
                    );
                    None
                }
            }
        }
        _ => None,
    };
    Ok(Some(DuplicateInfo {
        sent_at: entry.sent_at,
        destination_id: destination.id,
        message_id: entry.message_id,
        link,
    }))
}

// ---------- reconciliation ----------

/// `item_reconcile`: read only, candidates for an `unknown` item; the user then calls
/// `item_resolve_unknown`.
pub async fn reconcile(
    transport: &dyn TelegramTransport,
    queue: &Orchestrator,
    item_id: i64,
) -> Result<Vec<SentCandidate>> {
    let (item, destination) = queue.with_store(|store| {
        let item = store.get_item(item_id)?;
        let destination = store.get_destination(item.destination_id)?;
        Ok((item, destination))
    })?;
    if item.state != State::Unknown {
        return Err(QueueError::IllegalTransition {
            item_id,
            from: item.state,
            to: State::Sent,
        }
        .into());
    }
    let target = target_of(&destination).ok_or_else(|| bad_destination(&destination))?;
    let media = item_kind(&item, plan_of(&item).as_ref());
    let sent_path = item.output_path.as_deref().unwrap_or(&item.source_path);
    let file_name = (media != MediaKind::Photo)
        .then(|| Path::new(sent_path).file_name())
        .flatten()
        .map(|name| name.to_string_lossy().into_owned());
    let size = item
        .output_size
        .and_then(|size| u64::try_from(size).ok())
        .unwrap_or(0);
    let not_before_unix = parse_iso(&item.created_at)
        .map_or(0, |at| at.timestamp() - RECONCILE_SLACK_S)
        .max(0);
    let probe = SentProbe {
        target,
        media,
        file_name,
        size,
        not_before_unix,
    };
    let candidates = transport.verify_sent(&probe).await?;
    queue.with_store(|store| {
        store.remember_reconciliation_candidates(
            &item,
            candidates
                .iter()
                .map(|candidate| candidate.message_id)
                .collect(),
        )
    })?;
    tracing::info!(
        event = "reconcile",
        item_id,
        destination_id = destination.id,
        chat = crate::logging::redact::chat(target.peer.tdlib_chat_id()).as_str(),
        media = media.as_str(),
        size,
        candidates = candidates.len(),
        "reconciliation search done (read only)"
    );
    Ok(candidates)
}

// ---------- auth://state ----------

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MePayload {
    pub display_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    pub is_premium: bool,
    pub caption_max_chars: u32,
    pub upload_limit_bytes: u64,
}

/// Why the start failed (`start_failed`): the error as every command reports it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StartErrorPayload {
    pub code: &'static str,
    pub params: Value,
}

/// Payload of `auth://state`, plus `transport` (`mock` | `tdlib` | `none`) so
/// the UI can flag the test transport.
#[derive(Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthPayload {
    pub state: &'static str,
    pub transport: TransportKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qr_link: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resend_in_s: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password_hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub me: Option<MePayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub other_step: Option<String>,
    /// `start_failed` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<StartErrorPayload>,
    /// `start_failed` only: `auth_reset_session` may fix this failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_reset_session: Option<bool>,
}

/// `auth_state` journal line: the state name and non-identifying facts only. Never the QR
/// link, the password hint, the account name or anything the user typed.
pub fn log_auth_state(state: &AuthState) {
    // The transport kind is not part of this line (`transport_selected` has it).
    let payload = auth_payload(state, TransportKind::None);
    let me = payload.me.as_ref();
    tracing::info!(
        event = "auth_state",
        state = payload.state,
        resend_in_s = payload.resend_in_s,
        has_password_hint = payload
            .password_hint
            .as_deref()
            .is_some_and(|h| !h.is_empty()),
        other_step = payload.other_step.as_deref(),
        // The code only: a proxy error's params name the proxy server.
        error_code = payload.error.as_ref().map(|error| error.code),
        is_premium = me.map(|m| m.is_premium),
        caption_max_chars = me.map(|m| m.caption_max_chars),
        upload_limit_bytes = me.map(|m| m.upload_limit_bytes),
        "Telegram sign-in state"
    );
}

pub fn auth_payload(state: &AuthState, transport: TransportKind) -> AuthPayload {
    let mut payload = AuthPayload {
        state: "",
        transport,
        qr_link: None,
        resend_in_s: None,
        password_hint: None,
        me: None,
        other_step: None,
        error: None,
        can_reset_session: None,
    };
    payload.state = match state {
        AuthState::Starting => "starting",
        AuthState::WaitPhone => "wait_phone",
        AuthState::WaitQr { link } => {
            payload.qr_link = Some(link.clone());
            "wait_qr"
        }
        AuthState::WaitCode { resend_in } => {
            payload.resend_in_s = resend_in.map(seconds_up);
            "wait_code"
        }
        AuthState::WaitEmail => "wait_email",
        AuthState::WaitEmailCode => "wait_email_code",
        AuthState::WaitPassword { hint } => {
            payload.password_hint = Some(hint.clone());
            "wait_password"
        }
        AuthState::Ready(me) => {
            payload.me = Some(MePayload {
                display_name: me.display_name.clone(),
                username: me.username.clone(),
                is_premium: me.is_premium,
                caption_max_chars: me.caption_max_chars,
                upload_limit_bytes: me.upload_limit_bytes,
            });
            "ready"
        }
        AuthState::LoggingOut => "logging_out",
        AuthState::Closed => "closed",
        AuthState::StartFailed(error) => {
            let (code, params) = error.code_and_params();
            payload.error = Some(StartErrorPayload { code, params });
            payload.can_reset_session = Some(error.session_reset_may_help());
            "start_failed"
        }
        AuthState::OtherStep { name } => {
            payload.other_step = Some(name.clone());
            "other_step"
        }
    };
    payload
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::queue::events::recorder::Recorder;
    use crate::queue::orchestrator::{PauseRange, Unavailable};
    use crate::queue::store::{NewItem, Store};
    use crate::queue::test_util::{TestDir, force_state};
    use crate::transport::Me;
    use crate::transport::mock::{
        MOCK_ARCHIVED_CHAT, MOCK_ARCHIVED_FORUM, MOCK_DELETED, MOCK_FORUM, MOCK_OLD_GROUP,
        MOCK_PAID_GROUP, MOCK_PUBLIC_GROUP, MOCK_READ_ONLY_CHANNEL, MOCK_SAVED, MOCK_SLOW_GROUP,
        MockTransport,
    };

    fn queue() -> Orchestrator {
        Orchestrator::new(
            Store::open_in_memory().unwrap(),
            Arc::new(Unavailable),
            Arc::new(Unavailable),
            Arc::new(Recorder::default()),
            PauseRange {
                min: Duration::ZERO,
                max: Duration::ZERO,
            },
        )
    }

    fn find(destinations: &[Destination], peer: Peer, topic_id: i64) -> Destination {
        destinations
            .iter()
            .find(|d| d.peer_id == peer.raw_id && d.topic_id == topic_id)
            .cloned()
            .unwrap()
    }

    /// Forum topics, loaded the way the picker does when a forum is opened.
    async fn with_topics(
        mock: &MockTransport,
        queue: &Orchestrator,
        list: &[Destination],
    ) -> Vec<Destination> {
        for forum in [MOCK_FORUM, MOCK_ARCHIVED_FORUM] {
            load_topics(mock, queue, find(list, forum, 0).id)
                .await
                .unwrap();
        }
        queue.with_store(|s| s.list_destinations()).unwrap()
    }

    #[tokio::test]
    async fn refresh_caches_every_list_and_topics_load_on_demand() {
        let mock = MockTransport::authorized();
        let queue = queue();
        let list = refresh_destinations(&mock, &queue).await.unwrap();
        // 8 chats of the main list, the archive and the folders, deduplicated; no topic yet.
        assert_eq!(list.len(), 8);
        assert!(list.iter().all(|d| d.topic_id == 0 && d.listed));
        let saved = find(&list, MOCK_SAVED, 0);
        assert!(saved.is_saved);
        assert_eq!(
            (saved.peer_kind.as_str(), saved.kind.as_str()),
            ("user", "saved")
        );
        assert!(find(&list, MOCK_READ_ONLY_CHANNEL, 0).is_broadcast);
        assert_eq!(
            find(&list, MOCK_READ_ONLY_CHANNEL, 0).can_write,
            Some(false)
        );
        let old_friend = find(&list, MOCK_ARCHIVED_CHAT, 0);
        assert!(old_friend.archived);
        assert_eq!(old_friend.can_write, None, "private: unknown until checked");
        let slow = find(&list, MOCK_SLOW_GROUP, 0);
        let names: Vec<&str> = slow.folders.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["Mock Work", "Mock Family"]);
        let archived_forum = find(&list, MOCK_ARCHIVED_FORUM, 0);
        assert!(archived_forum.is_forum && archived_forum.archived);

        // Topics of an archived forum in a folder: on demand, with the chat's flags.
        let topics = load_topics(&mock, &queue, archived_forum.id).await.unwrap();
        let summary: Vec<(i64, &str, bool)> = topics
            .iter()
            .map(|t| (t.topic_id, t.title.as_str(), t.topic_closed))
            .collect();
        assert_eq!(
            summary,
            [
                (1, "Mock archived forum › General", false),
                (5, "Mock archived forum › Clips", false),
                (6, "Mock archived forum › Old", true)
            ]
        );
        assert!(topics.iter().all(|t| t.archived && t.is_forum && t.listed));
        assert_eq!(topics[0].folders, archived_forum.folders);
        // Not a forum, or already a topic: refused.
        assert!(load_topics(&mock, &queue, saved.id).await.is_err());
        assert!(load_topics(&mock, &queue, topics[1].id).await.is_err());
        let list = with_topics(&mock, &queue, &list).await;
        let topic = find(&list, MOCK_FORUM, 2);
        assert!(topic.is_forum);
        assert_eq!(topic.title, "Mock forum › Videos");
        assert_eq!(list.len(), 14);

        // Local settings survive a refresh; nothing is duplicated.
        queue
            .with_store(|s| {
                s.update_destination(
                    saved.id,
                    &crate::queue::store::DestinationPatch {
                        alias: Some(Some("Me".into())),
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        let again = refresh_destinations(&mock, &queue).await.unwrap();
        assert_eq!(
            again.len(),
            14,
            "chats and cached topics, nothing duplicated"
        );
        assert_eq!(find(&again, MOCK_SAVED, 0).alias.as_deref(), Some("Me"));
        assert!(again.iter().all(|d| d.listed));

        let signed_out = MockTransport::new();
        assert!(matches!(
            refresh_destinations(&signed_out, &queue).await,
            Err(ServiceError::Transport(TransportError::NotAuthorized))
        ));
    }

    #[tokio::test]
    async fn check_reports_rights_slow_mode_and_paid_messages() {
        let mock = MockTransport::authorized();
        let queue = queue();
        let list = refresh_destinations(&mock, &queue).await.unwrap();
        let list = with_topics(&mock, &queue, &list).await;
        let check = |peer, topic| {
            let destination = find(&list, peer, topic);
            let mock = &mock;
            async move {
                check_destination(mock, &destination, SendMode::Video)
                    .await
                    .unwrap()
            }
        };
        let ok = check(MOCK_SAVED, 0).await;
        assert!(ok.allowed);
        assert_eq!(ok.code, None);
        let read_only = check(MOCK_READ_ONLY_CHANNEL, 0).await;
        assert!(!read_only.allowed);
        assert_eq!(read_only.code, Some("TG_WRITE_FORBIDDEN"));
        assert_eq!(read_only.params, json!({ "reason": "admins_only" }));
        let paid = check(MOCK_PAID_GROUP, 0).await;
        assert_eq!((paid.allowed, paid.code), (false, Some("TG_PAID_MESSAGES")));
        let slow = check(MOCK_SLOW_GROUP, 0).await;
        assert!(slow.allowed);
        assert_eq!(slow.code, Some("TG_SLOWMODE"));
        assert_eq!(slow.params, json!({ "retry_after": 12 }));
        assert_eq!(
            (slow.slow_mode_delay_s, slow.slow_mode_wait_s),
            (Some(30), Some(12))
        );
        let closed = check(MOCK_FORUM, 3).await;
        assert_eq!(closed.params, json!({ "reason": "topic_closed" }));
        // Archived chats and chats in folders are checked like the others.
        assert!(check(MOCK_ARCHIVED_FORUM, 5).await.allowed);
        assert_eq!(
            check(MOCK_ARCHIVED_FORUM, 6).await.params,
            json!({ "reason": "topic_closed" })
        );
        assert!(check(MOCK_ARCHIVED_CHAT, 0).await.allowed);
        let json = serde_json::to_value(&slow).unwrap();
        assert!(json.get("checked_at").is_some());
    }

    /// Rows of deleted accounts cached before they were recognised (empty title, migrated from
    /// v1): a refresh drops them, unless something refers to one (kept, unnamed, unlisted).
    #[tokio::test]
    async fn refresh_drops_deleted_accounts_from_the_cache() {
        let mock = MockTransport::authorized();
        let queue = queue();
        let stale = |peer_id| NewDestination {
            peer_kind: "user".into(),
            peer_id,
            topic_id: 0,
            is_forum: false,
            is_broadcast: false,
            is_saved: false,
            title: String::new(),
            kind: "private".into(),
            archived: false,
            folders: Vec::new(),
            can_write: None,
            listed: true,
            topic_closed: false,
        };
        let (gone, used) = queue
            .with_store(|s| {
                let gone = s.upsert_destination(&stale(MOCK_DELETED.raw_id))?;
                // An untitled chat Telegram does not report as deleted.
                Ok((gone, s.upsert_destination(&stale(5_550_010))?))
            })
            .unwrap();
        let list = refresh_destinations(&mock, &queue).await.unwrap();
        assert!(
            !list.iter().any(|d| d.id == gone),
            "deleted account dropped"
        );
        assert!(!list.iter().any(|d| d.peer_id == MOCK_DELETED.raw_id));
        // Not deleted for Telegram: kept, unlisted (gone from the lists).
        assert!(!list.iter().find(|d| d.id == used).unwrap().listed);

        // Referenced by an item: kept, unnamed, unlisted, not writable.
        queue
            .with_store(|s| s.upsert_destination(&stale(MOCK_DELETED.raw_id)))
            .unwrap();
        let referenced = queue
            .with_store(|s| {
                let id = s
                    .list_destinations()?
                    .into_iter()
                    .find(|d| d.peer_id == MOCK_DELETED.raw_id)
                    .unwrap()
                    .id;
                s.add_items(&[NewItem {
                    source_path: "C:\\a.mp4".into(),
                    source_size: 1,
                    source_mtime: 1,
                    destination_id: id,
                    send_mode: SendMode::Video,
                    caption: String::new(),
                }])?;
                Ok(id)
            })
            .unwrap();
        let list = refresh_destinations(&mock, &queue).await.unwrap();
        let kept = list.iter().find(|d| d.id == referenced).unwrap();
        assert_eq!(
            (kept.title.as_str(), kept.listed, kept.can_write),
            ("", false, Some(false))
        );
    }

    #[tokio::test]
    async fn search_adds_unlisted_chats_and_keeps_list_flags() {
        let mock = MockTransport::authorized();
        let queue = queue();
        let list = refresh_destinations(&mock, &queue).await.unwrap();
        let favorite = find(&list, MOCK_ARCHIVED_CHAT, 0);
        queue
            .with_store(|s| {
                s.update_destination(
                    favorite.id,
                    &crate::queue::store::DestinationPatch {
                        favorite: Some(true),
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        // "old" matches an archived chat (already cached) and a chat in no list.
        let found = search_destinations(&mock, &queue, "  old ").await.unwrap();
        let peers: Vec<i64> = found.iter().map(|d| d.peer_id).collect();
        assert_eq!(peers, [MOCK_ARCHIVED_CHAT.raw_id, MOCK_OLD_GROUP.raw_id]);
        assert_eq!(found[0].id, favorite.id, "same row");
        assert!(found[0].archived && found[0].listed && found[0].favorite);
        assert_eq!(found[0].folders, favorite.folders);
        assert!(!found[1].listed && !found[1].archived);
        assert_eq!(found[1].kind, "basic_group");
        let public = search_destinations(&mock, &queue, "public").await.unwrap();
        assert_eq!(public.len(), 1);
        let cached = queue.with_store(|s| s.list_destinations()).unwrap();
        assert_eq!(cached.len(), 10, "8 listed + 2 found");
        // A found chat can be checked; a refresh keeps it, unlisted.
        let check = check_destination(&mock, &public[0], SendMode::Video)
            .await
            .unwrap();
        assert!(check.allowed);
        let again = refresh_destinations(&mock, &queue).await.unwrap();
        assert!(!find(&again, MOCK_PUBLIC_GROUP, 0).listed);
        assert!(find(&again, MOCK_ARCHIVED_CHAT, 0).listed);
        assert!(
            search_destinations(&mock, &queue, " ")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(matches!(
            search_destinations(&MockTransport::new(), &queue, "x").await,
            Err(ServiceError::Transport(TransportError::NotAuthorized))
        ));
    }

    #[tokio::test]
    async fn reconcile_is_read_only_and_needs_an_unknown_item() {
        let dir = TestDir::new("reconcile");
        let source = dir.file("a.mp4", b"0123456789");
        let output = dir.file("a_sq.mp4", b"123");
        let mock = MockTransport::authorized();
        let queue = queue();
        let list = refresh_destinations(&mock, &queue).await.unwrap();
        let saved = find(&list, MOCK_SAVED, 0);
        let id = queue
            .add(&[NewItem {
                source_path: source.clone(),
                source_size: 10,
                source_mtime: 1,
                destination_id: saved.id,
                send_mode: SendMode::Video,
                caption: String::new(),
            }])
            .unwrap()[0];
        assert!(matches!(
            reconcile(&mock, &queue, id).await,
            Err(ServiceError::Queue(QueueError::IllegalTransition { .. }))
        ));
        queue
            .with_store(|s| {
                s.conn().execute(
                    "UPDATE items SET output_path = ?1, output_size = 3,
                         content_id = '10:reconciliation' WHERE id = ?2",
                    rusqlite::params![output, id],
                )?;
                Ok(())
            })
            .unwrap();
        queue
            .with_store(|s| {
                force_state(s, id, State::Unknown);
                Ok(())
            })
            .unwrap();
        let target = target_of(&saved).unwrap();
        let now = chrono::Utc::now().timestamp();
        let found = mock.seed_message(target, MediaKind::Video, "a_sq.mp4", 3, now);
        mock.seed_message(target, MediaKind::Video, "a_sq.mp4", 3, now - 86_400);
        let other = mock.seed_message(target, MediaKind::Video, "other.mp4", 3, now);
        let candidates = reconcile(&mock, &queue, id).await.unwrap();
        let ids: Vec<i64> = candidates.iter().map(|c| c.message_id).collect();
        assert_eq!(ids, [found], "name, size and date window");
        assert_eq!(
            queue.with_store(|s| s.get_item(id)).unwrap().state,
            State::Unknown,
            "read only"
        );
        let json = serde_json::to_value(&candidates[0]).unwrap();
        assert_eq!(json["kind"], "video");
        assert_eq!(json["file_name"], "a_sq.mp4");

        let before = queue.with_store(|s| s.get_item(id)).unwrap();
        assert!(matches!(
            queue.resolve_unknown(
                id,
                crate::queue::orchestrator::ResolveAction::MarkSent,
                Some(other),
            ),
            Err(QueueError::InvalidValue(_))
        ));
        assert_eq!(queue.with_store(|s| s.get_item(id)).unwrap(), before);
        assert!(Path::new(&output).is_file());
        assert!(queue.history(&Default::default()).unwrap().is_empty());
        assert_eq!(
            queue
                .resolve_unknown(
                    id,
                    crate::queue::orchestrator::ResolveAction::MarkSent,
                    Some(found),
                )
                .unwrap(),
            State::Sent
        );
        assert!(!Path::new(&output).exists());
        assert!(Path::new(&source).is_file());
        let history = queue.history(&Default::default()).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].message_id, Some(found));
    }

    /// A held duplicate in a supergroup gets the link of the earlier message; in Saved
    /// Messages only the date and destination; signed out, the same without failing.
    #[tokio::test]
    async fn a_held_duplicate_links_to_the_earlier_message_where_telegram_has_links() {
        let mock = MockTransport::authorized();
        let queue = queue();
        let list = refresh_destinations(&mock, &queue).await.unwrap();
        let group = find(&list, MOCK_SLOW_GROUP, 0);
        let saved = find(&list, MOCK_SAVED, 0);
        let now = chrono::Utc::now().timestamp();
        let mut held = Vec::new();
        for destination in [&group, &saved] {
            let target = target_of(destination).unwrap();
            let message_id = mock.seed_message(target, MediaKind::Video, "a_sq.mp4", 3, now);
            let id = queue
                .add(&[NewItem {
                    source_path: "C:\\videos\\a.mp4".into(),
                    source_size: 10,
                    source_mtime: 1,
                    destination_id: destination.id,
                    send_mode: SendMode::Video,
                    caption: String::new(),
                }])
                .unwrap()[0];
            queue
                .with_store(|s| {
                    s.conn().execute(
                        "INSERT INTO history (content_id, source_path, file_name, destination_id,
                                              size_sent, mode, status, message_id, sent_at)
                         VALUES ('10:abc', 'a.mp4', 'a.mp4', ?1, 3, 'video', 'sent', ?2,
                                 '2026-10-01T08:00:00.000Z')",
                        rusqlite::params![destination.id, message_id],
                    )?;
                    force_state(s, id, State::PendingReview);
                    s.conn().execute(
                        "UPDATE items SET content_id = '10:abc', hold_reason = 'duplicate'
                         WHERE id = ?1",
                        [id],
                    )?;
                    Ok(())
                })
                .unwrap();
            held.push((id, message_id));
        }

        let (group_item, group_message) = held[0];
        let info = duplicate_info(&mock, &queue, group_item)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            info,
            DuplicateInfo {
                sent_at: "2026-10-01T08:00:00.000Z".into(),
                destination_id: group.id,
                message_id: Some(group_message),
                link: Some(format!(
                    "https://t.me/c/{}/{group_message}",
                    MOCK_SLOW_GROUP.raw_id
                )),
            }
        );
        let in_saved = duplicate_info(&mock, &queue, held[1].0)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (in_saved.destination_id, in_saved.link),
            (saved.id, None),
            "Saved Messages has no message links"
        );

        // Signed out: the date and destination are still given.
        mock.log_out().await.unwrap();
        let offline = duplicate_info(&mock, &queue, group_item)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (offline.message_id, offline.link),
            (Some(group_message), None)
        );

        // Not held as a duplicate, or history cleared: nothing to show.
        queue
            .with_store(|s| {
                s.conn().execute(
                    "UPDATE items SET hold_reason = NULL WHERE id = ?1",
                    [group_item],
                )?;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            duplicate_info(&mock, &queue, group_item).await.unwrap(),
            None
        );
        queue.clear_history().unwrap();
        assert_eq!(
            duplicate_info(&mock, &queue, held[1].0).await.unwrap(),
            None
        );
    }

    #[test]
    fn kinds_follow_the_plan() {
        let mut item = Item {
            id: 1,
            position: 1,
            source_path: "a.png".into(),
            source_size: 1,
            source_mtime: 1,
            content_id: None,
            identity_weak: false,
            destination_id: 1,
            send_mode: SendMode::Video,
            caption: String::new(),
            batch_index: 1,
            batch_count: 1,
            state: State::Encoded,
            hold_reason: None,
            origin: "manual".into(),
            media_json: None,
            plan_json: None,
            temp_path: None,
            output_path: None,
            output_size: None,
            progress_phase: None,
            progress_pct: None,
            attempts: 0,
            last_error: None,
            next_run_at: None,
            tg_message_id: None,
            tg_sent_at: None,
            created_at: String::new(),
            updated_at: String::new(),
            encoding_override: None,
            frozen_path: None,
        };
        let plan = |kind: &str, note: Option<&str>| -> Plan {
            serde_json::from_value(json!({
                "kind": kind, "limit_bytes": 1, "duration_s": null, "video_bps": null,
                "audio_kbps": null, "video_index": null, "audio_index": null, "vf": null,
                "hdr": false, "encoder": null, "output_name": "x", "photo": null,
                "source_dims": null, "alpha": false, "note": note
            }))
            .unwrap()
        };
        assert_eq!(item_kind(&item, None), MediaKind::Video);
        assert_eq!(
            item_kind(&item, Some(&plan("photo", None))),
            MediaKind::Photo
        );
        assert_eq!(
            item_kind(&item, Some(&plan("skip", Some("photo_ratio")))),
            MediaKind::Document
        );
        item.send_mode = SendMode::Document;
        assert_eq!(
            item_kind(&item, Some(&plan("photo", None))),
            MediaKind::Document
        );
    }

    #[test]
    fn auth_payload_uses_the_spec_names() {
        let ready = AuthState::Ready(Me {
            user_id: 1,
            display_name: "Ann".into(),
            username: None,
            is_premium: true,
            caption_max_chars: 4096,
            upload_limit_bytes: 42,
        });
        let payload = |state: &AuthState| {
            serde_json::to_value(auth_payload(state, TransportKind::Tdlib)).unwrap()
        };
        assert_eq!(
            payload(&ready),
            json!({ "state": "ready", "transport": "tdlib", "me": {
                "displayName": "Ann", "isPremium": true, "captionMaxChars": 4096,
                "uploadLimitBytes": 42
            }})
        );
        let code = AuthState::WaitCode {
            resend_in: Some(Duration::from_millis(59_100)),
        };
        assert_eq!(
            payload(&code),
            json!({ "state": "wait_code", "transport": "tdlib", "resendInS": 60 })
        );
        let qr = AuthState::WaitQr {
            link: "tg://x".into(),
        };
        assert_eq!(
            payload(&qr),
            json!({ "state": "wait_qr", "transport": "tdlib", "qrLink": "tg://x" })
        );
        let other = AuthState::OtherStep {
            name: "registration".into(),
        };
        assert_eq!(
            payload(&other),
            json!({ "state": "other_step", "transport": "tdlib", "otherStep": "registration" })
        );
        assert_eq!(
            payload(&AuthState::StartFailed(TransportError::BadDatabaseKey)),
            json!({ "state": "start_failed", "transport": "tdlib",
                "error": { "code": "TG_BAD_DB_KEY", "params": {} }, "canResetSession": true })
        );
        assert_eq!(
            payload(&AuthState::StartFailed(
                TransportError::ProxySecretUnavailable
            )),
            json!({ "state": "start_failed", "transport": "tdlib",
                "error": { "code": "TG_PROXY_SECRET_UNAVAILABLE", "params": {} },
                "canResetSession": false })
        );
        for kind in [TransportKind::Mock, TransportKind::None] {
            assert_eq!(
                serde_json::to_value(auth_payload(&AuthState::Closed, kind)).unwrap(),
                json!({ "state": "closed", "transport": kind.as_str() })
            );
        }
        for (state, name) in [
            (AuthState::Starting, "starting"),
            (AuthState::WaitPhone, "wait_phone"),
            (AuthState::WaitEmail, "wait_email"),
            (AuthState::WaitEmailCode, "wait_email_code"),
            (
                AuthState::WaitPassword { hint: "h".into() },
                "wait_password",
            ),
            (AuthState::LoggingOut, "logging_out"),
            (AuthState::Closed, "closed"),
        ] {
            assert_eq!(auth_payload(&state, TransportKind::Mock).state, name);
        }
    }
}
