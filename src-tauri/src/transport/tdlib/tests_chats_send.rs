//! Chats and sending over `Replay` fixtures shaped after `td_api.tl` of TDLib 1.8.68 (no account, no
//! network), plus one ignored, read-only test on the user's real TDLib session.
//!
//! `SQUEEZEGRAM_DUMP_TDLIB_REQUESTS=<folder>`: every flow test writes the requests it sent
//! (one JSON per line, `<test>.jsonl`) so they can be checked against the schema.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::chats::SAVED_TITLE;
use super::replay::Replay;
use super::tests::{CLOSE, START, TempDir, config, fixture};
use super::*;
use crate::logging::testing::TestJournal;
use crate::queue::orchestrator::{SendError, SendOutcome};
use crate::transport::stage::outcome;
use crate::transport::{Caption, Forbidden, Payload, PeerKind, Phase, Verdict};

const DIALOGS: &str = include_str!("fixtures/dialogs.json");
const DIALOGS_WITHOUT_SAVED: &str = include_str!("fixtures/dialogs_without_saved.json");
const DIALOGS_ALL_LISTS: &str = include_str!("fixtures/dialogs_all_lists.json");
const SEARCH: &str = include_str!("fixtures/search.json");
const TOPICS: &str = include_str!("fixtures/topics.json");
const CAN_SEND: &str = include_str!("fixtures/can_send.json");
const UPLOAD_OK: &str = include_str!("fixtures/upload_ok.json");
const UPLOAD_TOPIC_DOCUMENT: &str = include_str!("fixtures/upload_topic_document.json");
const UPLOAD_FAILED: &str = include_str!("fixtures/upload_failed.json");
const UPLOAD_FLOOD: &str = include_str!("fixtures/upload_flood.json");
const UPLOAD_CANCEL: &str = include_str!("fixtures/upload_cancel.json");
const UPLOAD_CANCEL_AFTER_SUCCESS: &str = include_str!("fixtures/upload_cancel_after_success.json");
const UPLOAD_CANCEL_CROSSING: &str = include_str!("fixtures/upload_cancel_crossing.json");
const UPLOAD_REFUSED: &str = include_str!("fixtures/upload_refused.json");
const VERIFY: &str = include_str!("fixtures/verify.json");

const ME: i64 = 777_000_001;
const OTHER: i64 = 5_550_001;
const FORUM: i64 = 1_234_567_890;
const CHANNEL: i64 = 2_000_000_001;
const SLOW: i64 = 3_000_000_002;
const PAID: i64 = 4_000_000_003;
const BASIC: i64 = 42;
const TEMP: i64 = 2_097_153;
const FAILED: i64 = 2_097_154;
const FINAL: i64 = 3_145_728;
const NOT_BEFORE: i64 = 1_760_000_000;
const CAPTION: &str = "caption text";

fn peer(kind: PeerKind, raw_id: i64) -> Peer {
    Peer { kind, raw_id }
}

fn target(kind: PeerKind, raw_id: i64, topic_id: Option<i32>) -> Target {
    Target {
        peer: peer(kind, raw_id),
        topic_id,
    }
}

fn saved() -> Target {
    target(PeerKind::User, ME, None)
}

/// `START`, the parts, `CLOSE`; started (offline, `WaitPhone`).
async fn started(parts: &[&str], dir: &TempDir) -> Arc<TdlibJson<Replay>> {
    let mut all = vec![START];
    all.extend_from_slice(parts);
    all.push(CLOSE);
    let transport = TdlibJson::new(Replay::new(fixture(&all)), config(dir.path()));
    transport.start().await.unwrap();
    Arc::new(transport)
}

/// Requests sent, for the schema check (`SQUEEZEGRAM_DUMP_TDLIB_REQUESTS`).
fn dump(name: &str, replay: &Replay) {
    let Some(dir) = std::env::var_os("SQUEEZEGRAM_DUMP_TDLIB_REQUESTS") else {
        return;
    };
    let lines: Vec<String> = replay.sent().iter().map(Value::to_string).collect();
    std::fs::write(
        PathBuf::from(dir).join(format!("{name}.jsonl")),
        lines.join("\n"),
    )
    .unwrap();
}

/// Shutdown, then every step used and nothing unexpected.
async fn finish(transport: &TdlibJson<Replay>, name: &str) {
    transport.shutdown().await;
    let replay = transport.bindings();
    assert!(replay.unexpected().is_empty(), "{:?}", replay.unexpected());
    assert!(replay.remaining().is_empty(), "{:?}", replay.remaining());
    dump(name, replay);
}

fn clip(dir: &TempDir, name: &str) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, vec![7u8; 1000]).unwrap();
    path
}

fn request(path: PathBuf, target: Target, payload: Payload) -> UploadRequest {
    UploadRequest {
        path,
        size: 1000,
        target,
        payload,
        caption: Caption {
            text: CAPTION.into(),
            entities: Vec::new(),
        },
        limit_bytes: None,
    }
}

fn video() -> Payload {
    Payload::Video {
        duration_s: 61,
        width: 1920,
        height: 1080,
        supports_streaming: true,
        thumbnail: None,
    }
}

fn drain(rx: &mut mpsc::Receiver<Progress>) -> Vec<(Phase, u64)> {
    let mut seen = Vec::new();
    while let Ok(p) = rx.try_recv() {
        seen.push((p.phase, p.sent));
    }
    seen
}

/// Waits (real time, bounded) until `check` holds.
async fn until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ---------- chats ----------

#[tokio::test]
async fn dialogs_follow_the_main_list_with_saved_messages_first() {
    let dir = TempDir::new("m3-dialogs");
    let journal = TestJournal::new(&dir.path().join("logs"), "debug");
    let transport = started(&[DIALOGS], &dir).await;
    let dialogs = transport.list_dialogs(500).await.unwrap();
    let peers: Vec<(PeerKind, i64)> = dialogs
        .iter()
        .map(|d| (d.peer.kind, d.peer.raw_id))
        .collect();
    // Saved first, then TDLib's order; secret chat, upgraded basic group and the channel
    // direct-messages group are skipped.
    assert_eq!(
        peers,
        [
            (PeerKind::User, ME),
            (PeerKind::User, OTHER),
            (PeerKind::Channel, FORUM),
            (PeerKind::Channel, CHANNEL),
            (PeerKind::BasicGroup, BASIC),
            (PeerKind::Channel, SLOW),
        ]
    );
    assert!(dialogs[0].is_saved_messages);
    assert_eq!(dialogs[0].title, SAVED_TITLE);
    assert!(dialogs[1..].iter().all(|d| !d.is_saved_messages));
    assert!(dialogs[2].is_forum && !dialogs[2].is_broadcast);
    assert_eq!(dialogs[2].username.as_deref(), Some("forum_public"));
    assert!(dialogs[3].is_broadcast && !dialogs[3].is_forum);
    assert_eq!(dialogs[4].title, "Family Secret Title");

    let replay = transport.bindings();
    let loads = replay.sent_of("loadChats");
    assert_eq!(loads.len(), 4, "main until the 404, then the archive");
    assert_eq!(loads[2]["chat_list"]["@type"], "chatListMain");
    assert_eq!(loads[3]["chat_list"]["@type"], "chatListArchive");
    let get_chats = &replay.sent_of("getChats")[0];
    assert_eq!(get_chats["chat_list"]["@type"], "chatListMain");
    assert_eq!(get_chats["limit"], 500);
    assert_eq!(
        replay.sent_of("getChats")[1]["chat_list"]["@type"],
        "chatListArchive"
    );
    assert!(
        dialogs
            .iter()
            .all(|d| d.listed && !d.archived && d.folders.is_empty())
    );
    assert!(replay.sent_of("createPrivateChat").is_empty());

    let lines = journal.lines();
    let loaded = lines
        .iter()
        .find(|l| l["event"] == "dialogs_loaded")
        .expect("dialogs_loaded line");
    let fields = &loaded["fields"];
    assert_eq!(
        (
            &fields["dialogs"],
            &fields["listed"],
            &fields["skipped"],
            &fields["private"],
            &fields["basic_groups"],
            &fields["supergroups"],
            &fields["channels"],
            &fields["forums"],
            &fields["load_rounds"],
        ),
        (
            &json!(6),
            &json!(9),
            &json!(3),
            &json!(2),
            &json!(1),
            &json!(2),
            &json!(1),
            &json!(1),
            &json!(4)
        )
    );
    assert_eq!(
        (
            &fields["main"],
            &fields["archive"],
            &fields["folders"],
            &fields["folders_known"]
        ),
        (&json!(9), &json!(0), &json!(0), &json!(false))
    );
    // Counts only: no title, no raw id.
    let text = journal.text();
    for secret in [
        "Secret Title",
        "Alice",
        "forum_public",
        "1234567890",
        "777000001",
        "5550001",
    ] {
        assert!(!text.contains(secret), "{secret} leaked");
    }
    finish(&transport, "dialogs").await;

    // The limit applies after the sort.
    let dir = TempDir::new("m3-dialogs-limit");
    let transport = started(&[DIALOGS], &dir).await;
    let two = transport.list_dialogs(2).await.unwrap();
    assert_eq!(
        two.iter().map(|d| d.peer.raw_id).collect::<Vec<_>>(),
        [ME, OTHER]
    );
    assert_eq!(transport.bindings().sent_of("getChats")[0]["limit"], 2);
    finish(&transport, "dialogs_limit").await;
}

#[tokio::test]
async fn saved_messages_are_added_when_not_in_the_main_list() {
    let dir = TempDir::new("m3-saved");
    let transport = started(&[DIALOGS_WITHOUT_SAVED], &dir).await;
    let dialogs = transport.list_dialogs(500).await.unwrap();
    assert_eq!(dialogs.len(), 2);
    assert!(dialogs[0].is_saved_messages);
    assert_eq!(dialogs[0].peer, peer(PeerKind::User, ME));
    assert_eq!(dialogs[0].title, SAVED_TITLE);
    // `list_dialogs` read `me` once and keeps it for the session (cleared on close).
    let me = lock(&transport.inner.me_cache).clone().expect("me cached");
    assert!(!me.is_premium);
    assert_eq!(me.upload_limit_bytes, 2000 * MIB);
    assert_eq!(me.caption_max_chars, 1024);
    let replay = transport.bindings();
    assert_eq!(replay.sent_of("createPrivateChat")[0]["user_id"], ME);
    assert_eq!(replay.sent_of("getMe").len(), 1);
    finish(&transport, "dialogs_without_saved").await;
}

/// Requests that change something on the server or in the user's lists: never sent by the
/// chat-list, topic, search or check paths.
const WRITES: &[&str] = &[
    "sendMessage",
    "deleteMessages",
    "joinChat",
    "leaveChat",
    "resendMessages",
    "openChat",
    "viewMessages",
    "readAllChatMentions",
    "toggleChatIsMarkedAsUnread",
    "addChatToList",
    "addRecentlyFoundChat",
    "setChatFolder",
    "editChatFolder",
    "toggleForumTopicIsClosed",
];

fn assert_read_only(replay: &Replay) {
    for kind in WRITES {
        assert!(replay.sent_of(kind).is_empty(), "{kind} sent");
    }
}

/// Main list, archive, two folders (and one TDLib no longer knows), a chat in the main list
/// and both folders, an archived forum in a folder, a deleted account; then that forum's
/// topics and `can_send` on archived chats, all read only.
#[tokio::test]
async fn dialogs_cover_the_archive_and_every_folder() {
    const ALICE: i64 = 5_550_001;
    const BOB: i64 = 5_550_002;
    const ARCHIVED_FORUM: i64 = 6_000_000_005;
    let dir = TempDir::new("m3-all-lists");
    let journal = TestJournal::new(&dir.path().join("logs"), "debug");
    let transport = started(&[DIALOGS_ALL_LISTS], &dir).await;
    let dialogs = transport.list_dialogs(500).await.unwrap();
    let summary: Vec<(i64, bool, Vec<i32>, bool)> = dialogs
        .iter()
        .map(|d| {
            (
                d.peer.raw_id,
                d.archived,
                d.folders.iter().map(|f| f.id).collect(),
                d.is_forum,
            )
        })
        .collect();
    // Saved first (created), then the order of first appearance; each chat once. The
    // deleted account (empty title, `userTypeDeleted`) is flagged, never a destination.
    assert_eq!(
        summary,
        [
            (ME, false, vec![], false),
            (ALICE, false, vec![], false),
            (SLOW, false, vec![2, 5], false),
            (5_550_009, false, vec![], false),
            (ARCHIVED_FORUM, true, vec![2], true),
            (BOB, true, vec![5], false),
        ]
    );
    let deleted: Vec<bool> = dialogs.iter().map(|d| d.deleted).collect();
    assert_eq!(deleted, [false, false, false, true, false, false]);
    assert_eq!(dialogs[3].can_write, Some(false));
    assert!(
        dialogs
            .iter()
            .all(|d| d.listed && (d.deleted || !d.title.is_empty()))
    );
    let slow = &dialogs[2];
    assert_eq!(slow.kind, crate::transport::ChatKind::Supergroup);
    assert_eq!(slow.can_write, Some(true));
    let names: Vec<&str> = slow.folders.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["Work Secret Folder", "Family Secret Folder"]);
    assert_eq!(
        dialogs[5].can_write, None,
        "private: unknown until can_send"
    );
    assert_eq!(dialogs[0].can_write, Some(true));

    let replay = transport.bindings();
    let lists: Vec<Value> = replay
        .sent_of("getChats")
        .iter()
        .map(|r| r["chat_list"].clone())
        .collect();
    assert_eq!(
        lists,
        [
            json!({ "@type": "chatListMain" }),
            json!({ "@type": "chatListArchive" }),
            json!({ "@type": "chatListFolder", "chat_folder_id": 2 }),
            json!({ "@type": "chatListFolder", "chat_folder_id": 5 }),
        ]
    );
    assert_eq!(replay.sent_of("loadChats").len(), 6);
    assert_eq!(
        replay.sent_of("getChat").len(),
        5,
        "deduplicated before getChat"
    );
    assert_eq!(replay.sent_of("getUser")[0]["user_id"], 5_550_009);

    // Topics of the archived forum (General is topic 1), loaded on demand.
    let topics = transport
        .list_topics(&peer(PeerKind::Channel, ARCHIVED_FORUM))
        .await
        .unwrap();
    let topics: Vec<(i32, bool, bool)> = topics
        .iter()
        .map(|t| (t.id, t.is_general, t.is_closed))
        .collect();
    assert_eq!(
        topics,
        [(1, true, false), (5, false, false), (6, false, true)]
    );
    // can_send on a topic of the archived forum and on the archived private chat.
    let topic = transport
        .can_send(
            &target(PeerKind::Channel, ARCHIVED_FORUM, Some(5)),
            MediaKind::Video,
        )
        .await
        .unwrap();
    assert_eq!(topic.verdict, Verdict::Allowed);
    let archived = transport
        .can_send(&target(PeerKind::User, BOB, None), MediaKind::Video)
        .await
        .unwrap();
    assert_eq!(archived.verdict, Verdict::Allowed);
    assert_read_only(replay);

    // Journal: counts per list, never a title, a folder name or a raw id.
    let lines = journal.lines();
    let fields = &lines
        .iter()
        .find(|l| l["event"] == "dialogs_loaded")
        .expect("dialogs_loaded")["fields"];
    let expected = json!({
        "main": 3, "archive": 2, "folders": 3, "folder_chats": "[2,2]", "lists_failed": 1,
        "listed": 9, "unique": 5, "dialogs": 5, "archived": 2, "in_folders": 3, "forums": 1,
        "archived_forums": 1, "deleted_users": 1, "folders_known": true, "saved_added": true,
    });
    for (key, value) in expected.as_object().unwrap() {
        assert_eq!(&fields[key], value, "dialogs_loaded.{key}");
    }
    let text = journal.text();
    for secret in [
        "Secret Title",
        "Secret Folder",
        "Gone Folder",
        "Clips",
        "5550001",
        "5550002",
        "6000000005",
        "777000001",
    ] {
        assert!(!text.contains(secret), "{secret} leaked");
    }
    finish(&transport, "dialogs_all_lists").await;
}

/// `search_chats`: local, then server, then public results, deduplicated; a failed server
/// search keeps the rest. Read only.
#[tokio::test]
async fn search_finds_chats_outside_the_lists() {
    const PUBLIC: i64 = 7_000_000_006;
    const NEWS: i64 = 8_000_000_007;
    let dir = TempDir::new("m3-search");
    let journal = TestJournal::new(&dir.path().join("logs"), "debug");
    let transport = started(&[SEARCH], &dir).await;
    let found = transport.search_chats(" secret q ", 20).await.unwrap();
    let summary: Vec<(i64, bool, Option<bool>, bool)> = found
        .iter()
        .map(|d| (d.peer.raw_id, d.is_broadcast, d.can_write, d.listed))
        .collect();
    assert_eq!(
        summary,
        [
            (SLOW, false, Some(true), false),
            // Not a member, but the group lets non-members post.
            (PUBLIC, false, Some(true), false),
            // A channel the account does not administer.
            (NEWS, true, Some(false), false),
        ]
    );
    // Second search: the server search fails, nothing else does.
    assert!(transport.search_chats("zz", 20).await.unwrap().is_empty());
    assert!(transport.search_chats("   ", 20).await.unwrap().is_empty());

    let replay = transport.bindings();
    let local = &replay.sent_of("searchChats")[0];
    assert_eq!(
        (&local["query"], &local["limit"], &local["type_filter"]),
        (&json!("secret q"), &json!(20), &Value::Null)
    );
    assert_eq!(
        replay.sent_of("searchChatsOnServer")[0]["query"],
        "secret q"
    );
    assert!(replay.sent_of("searchPublicChats")[0]["type_filter"].is_null());
    assert_eq!(
        replay.sent_of("getChat").len(),
        3,
        "the duplicate is read once"
    );
    assert_read_only(replay);

    let lines = journal.lines();
    let searches: Vec<&Value> = lines
        .iter()
        .filter(|l| l["event"] == "chats_searched")
        .map(|l| &l["fields"])
        .collect();
    assert_eq!(searches.len(), 2);
    assert_eq!(
        (
            &searches[0]["query_chars"],
            &searches[0]["local"],
            &searches[0]["server"],
            &searches[0]["public"],
            &searches[0]["unique"],
            &searches[0]["results"]
        ),
        (
            &json!(8),
            &json!(1),
            &json!(2),
            &json!(1),
            &json!(3),
            &json!(3)
        )
    );
    assert_eq!(searches[1]["failed"], 1);
    let text = journal.text();
    for secret in ["secret q", "Secret Title", "7000000006", "8000000007"] {
        assert!(!text.contains(secret), "{secret} leaked");
    }
    finish(&transport, "search").await;
}

#[tokio::test]
async fn forum_topics_are_paginated_with_the_next_offsets() {
    let dir = TempDir::new("m3-topics");
    let transport = started(&[TOPICS], &dir).await;
    let topics = transport
        .list_topics(&peer(PeerKind::Channel, FORUM))
        .await
        .unwrap();
    let summary: Vec<(i32, &str, bool, bool)> = topics
        .iter()
        .map(|t| (t.id, t.title.as_str(), t.is_general, t.is_closed))
        .collect();
    assert_eq!(
        summary,
        [
            (1, "General", true, false),
            (2, "Videos", false, false),
            (3, "Archive", false, true)
        ]
    );
    let replay = transport.bindings();
    // The chat is made known first.
    let calls = replay.calls();
    let create = calls
        .iter()
        .position(|c| c == "send:createSupergroupChat")
        .unwrap();
    let topics_call = calls
        .iter()
        .position(|c| c == "send:getForumTopics")
        .unwrap();
    assert!(create < topics_call);
    let pages = replay.sent_of("getForumTopics");
    assert_eq!(pages.len(), 2);
    assert_eq!(pages[0]["chat_id"], -1_001_234_567_890i64);
    assert_eq!(pages[0]["query"], "");
    assert_eq!(pages[0]["limit"], 100);
    assert_eq!(
        (
            &pages[0]["offset_date"],
            &pages[0]["offset_message_id"],
            &pages[0]["offset_forum_topic_id"]
        ),
        (&json!(0), &json!(0), &json!(0))
    );
    assert_eq!(
        (
            &pages[1]["offset_date"],
            &pages[1]["offset_message_id"],
            &pages[1]["offset_forum_topic_id"]
        ),
        (&json!(1_700_000_500), &json!(2048), &json!(2))
    );
    finish(&transport, "topics").await;
}

#[tokio::test]
async fn can_send_reads_rights_topics_slow_mode_and_paid_messages() {
    let dir = TempDir::new("m3-cansend");
    let transport = started(&[CAN_SEND], &dir).await;
    let video = MediaKind::Video;
    let check = |target: Target| {
        let transport = Arc::clone(&transport);
        async move { transport.can_send(&target, video).await.unwrap() }
    };
    let saved_check = check(saved()).await;
    assert_eq!(saved_check.verdict, Verdict::Allowed);
    assert!(saved_check.slow_mode.is_none());

    let slow = check(target(PeerKind::Channel, SLOW, None)).await;
    assert_eq!(slow.verdict, Verdict::Allowed);
    let slow_mode = slow.slow_mode.expect("slow mode");
    assert_eq!(slow_mode.delay, Duration::from_secs(30));
    assert_eq!(slow_mode.wait_remaining, Duration::from_secs_f64(11.4));

    let closed = check(target(PeerKind::Channel, FORUM, Some(3))).await;
    assert_eq!(closed.verdict, Verdict::Denied(Forbidden::TopicClosed));
    // An administrator with can_manage_topics posts in a closed topic, and ignores slow mode.
    let manager = check(target(PeerKind::Channel, FORUM, Some(3))).await;
    assert_eq!(manager.verdict, Verdict::Allowed);
    assert!(manager.slow_mode.is_none());

    let channel = check(target(PeerKind::Channel, CHANNEL, None)).await;
    assert_eq!(channel.verdict, Verdict::Denied(Forbidden::AdminsOnly));
    let paid = check(target(PeerKind::Channel, PAID, None)).await;
    assert_eq!(
        paid.verdict,
        Verdict::Denied(Forbidden::PaidMessages { stars: 25 })
    );
    let basic = check(target(PeerKind::BasicGroup, BASIC, None)).await;
    assert_eq!(
        basic.verdict,
        Verdict::Denied(Forbidden::MediaNotAllowed(MediaKind::Video))
    );
    let user = check(target(PeerKind::User, OTHER, None)).await;
    assert_eq!(
        user.verdict,
        Verdict::Denied(Forbidden::PaidMessages { stars: 50 })
    );
    // Unknown to TDLib: loadChats until 404, then the chat is created and checked.
    let unknown = check(target(PeerKind::Channel, SLOW, None)).await;
    assert_eq!(unknown.verdict, Verdict::Allowed);
    assert!(
        unknown.slow_mode.is_none(),
        "administrators ignore slow mode"
    );

    let replay = transport.bindings();
    assert_eq!(replay.sent_of("getForumTopic")[0]["forum_topic_id"], 3);
    assert_eq!(replay.sent_of("getSupergroupFullInfo").len(), 1);
    let user_check = &replay.sent_of("canSendMessageToUser")[0];
    assert_eq!(
        (&user_check["user_id"], &user_check["only_local"]),
        (&json!(OTHER), &json!(false))
    );
    // Nothing that writes: no send, no delete, no join.
    for kind in [
        "sendMessage",
        "deleteMessages",
        "joinChat",
        "resendMessages",
    ] {
        assert!(replay.sent_of(kind).is_empty(), "{kind}");
    }
    finish(&transport, "can_send").await;
}

// ---------- upload ----------

async fn upload(
    transport: &Arc<TdlibJson<Replay>>,
    req: UploadRequest,
    cancel: CancellationToken,
) -> (Result<Sent, TransportError>, Vec<(Phase, u64)>) {
    let (tx, mut rx) = mpsc::channel(64);
    let result = transport.upload(req, tx, cancel).await;
    (result, drain(&mut rx))
}

#[tokio::test]
async fn upload_succeeds_only_on_update_message_send_succeeded() {
    let dir = TempDir::new("m4-upload");
    let journal = TestJournal::new(&dir.path().join("logs"), "debug");
    let transport = started(&[UPLOAD_OK], &dir).await;
    let path = clip(&dir, "clip_sq.mp4");
    let (result, phases) = upload(
        &transport,
        request(path.clone(), saved(), video()),
        CancellationToken::new(),
    )
    .await;
    let sent = result.unwrap();
    // The final id (not the temporary one), and only the update about our message.
    assert_eq!(sent.message_id, FINAL);
    assert_eq!(sent.date_unix, 1_760_000_000);
    assert_eq!(sent.file_name, "clip_sq.mp4");
    assert_eq!(sent.size, 1000);
    assert_eq!(
        phases,
        [
            (Phase::Preparing, 0),
            (Phase::Uploading, 0),
            (Phase::Uploading, 512),
            (Phase::Uploading, 1000),
            (Phase::AwaitingServer, 1000),
        ]
    );

    let replay = transport.bindings();
    let send = &replay.sent_of("sendMessage")[0];
    assert_eq!(send["chat_id"], ME);
    assert_eq!(send["topic_id"], Value::Null);
    assert_eq!(send["options"]["paid_message_star_count"], 0);
    let content = &send["input_message_content"];
    assert_eq!(content["@type"], "inputMessageVideo");
    assert_eq!(content["video"]["@type"], "inputVideo");
    assert_eq!(
        PathBuf::from(content["video"]["video"]["path"].as_str().unwrap()),
        path
    );
    assert_eq!(content["video"]["supports_streaming"], true);
    assert_eq!(content["video"]["duration"], 61);
    assert_eq!(content["caption"]["text"], CAPTION);
    assert!(replay.sent_of("deleteMessages").is_empty());
    assert!(replay.sent_of("resendMessages").is_empty());

    let lines = journal.lines();
    for event in ["upload_accepted", "upload_uploaded"] {
        assert!(lines.iter().any(|l| l["event"] == event), "{event}");
    }
    let progress = lines
        .iter()
        .find(|l| l["event"] == "upload_progress")
        .expect("upload_progress");
    assert_eq!(progress["fields"]["total"], 1000);
    let text = journal.text();
    assert!(!text.contains(CAPTION), "caption leaked");
    assert!(!text.contains("777000001"), "raw chat id leaked");
    finish(&transport, "upload_ok").await;
}

#[tokio::test]
async fn upload_to_a_forum_topic_checks_the_rights_first() {
    let dir = TempDir::new("m4-topic");
    let transport = started(&[UPLOAD_TOPIC_DOCUMENT], &dir).await;
    let path = clip(&dir, "clip_sq.mkv");
    let (result, phases) = upload(
        &transport,
        request(
            path,
            target(PeerKind::Channel, FORUM, Some(2)),
            Payload::Document { thumbnail: None },
        ),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(result.unwrap().message_id, FINAL);
    assert_eq!(phases.last(), Some(&(Phase::AwaitingServer, 1000)));
    let replay = transport.bindings();
    let calls = replay.calls();
    let at = |call: &str| calls.iter().position(|c| c == call).unwrap();
    assert!(at("send:createSupergroupChat") < at("send:getSupergroup"));
    assert!(at("send:getForumTopic") < at("send:sendMessage"));
    let send = &replay.sent_of("sendMessage")[0];
    assert_eq!(
        send["topic_id"],
        json!({ "@type": "messageTopicForum", "forum_topic_id": 2 })
    );
    assert_eq!(
        send["input_message_content"]["@type"],
        "inputMessageDocument"
    );
    assert_eq!(
        send["input_message_content"]["document"]["@type"],
        "inputDocument"
    );
    finish(&transport, "upload_topic_document").await;
}

#[tokio::test]
async fn a_failed_send_is_typed_and_its_message_deleted() {
    let dir = TempDir::new("m4-failed");
    let transport = started(&[UPLOAD_FAILED], &dir).await;
    let (result, _) = upload(
        &transport,
        request(
            clip(&dir, "p_sq.jpg"),
            saved(),
            Payload::Photo {
                width: 1280,
                height: 960,
            },
        ),
        CancellationToken::new(),
    )
    .await;
    let error = result.unwrap_err();
    assert!(
        matches!(&error, TransportError::Other { code: 400, message } if message == "PHOTO_INVALID_DIMENSIONS"),
        "{error}"
    );
    assert!(matches!(
        outcome(Err(error)),
        Err(SendError::Failed { code, .. }) if code == "TG_UPLOAD_FAILED"
    ));
    let replay = transport.bindings();
    let delete = &replay.sent_of("deleteMessages")[0];
    assert_eq!(delete["chat_id"], ME);
    assert_eq!(delete["message_ids"], json!([FAILED]));
    assert_eq!(delete["revoke"], true);
    let photo = &replay.sent_of("sendMessage")[0]["input_message_content"];
    assert_eq!(photo["@type"], "inputMessagePhoto");
    assert_eq!(photo["photo"]["@type"], "inputPhoto");
    finish(&transport, "upload_failed").await;
}

#[tokio::test]
async fn a_429_is_flood_wait_and_the_temporary_message_is_deleted() {
    let dir = TempDir::new("m4-flood");
    let transport = started(&[UPLOAD_FLOOD], &dir).await;
    let (result, _) = upload(
        &transport,
        request(clip(&dir, "clip_sq.mp4"), saved(), video()),
        CancellationToken::new(),
    )
    .await;
    let error = result.unwrap_err();
    assert!(
        matches!(error, TransportError::FloodWait { retry_after } if retry_after == Duration::from_secs(1234)),
        "{error}"
    );
    assert_eq!(
        outcome(Err(error)),
        Ok(SendOutcome::FloodWait {
            retry_after: Duration::from_secs(1234)
        })
    );
    let replay = transport.bindings();
    assert_eq!(
        replay.sent_of("deleteMessages")[0]["message_ids"],
        json!([FAILED])
    );
    assert!(replay.sent_of("resendMessages").is_empty(), "never resent");
    finish(&transport, "upload_flood").await;
}

#[tokio::test]
async fn refused_by_send_message_nothing_to_delete() {
    let dir = TempDir::new("m4-refused");
    let transport = started(&[UPLOAD_REFUSED], &dir).await;
    let path = clip(&dir, "clip_sq.mp4");
    let (flood, phases) = upload(
        &transport,
        request(path.clone(), saved(), video()),
        CancellationToken::new(),
    )
    .await;
    assert!(
        matches!(flood, Err(TransportError::FloodWait { retry_after }) if retry_after == Duration::from_secs(4000))
    );
    assert_eq!(phases, [(Phase::Preparing, 0)]);
    let (forbidden, _) = upload(
        &transport,
        request(path, saved(), video()),
        CancellationToken::new(),
    )
    .await;
    assert!(matches!(
        forbidden,
        Err(TransportError::WriteForbidden(Forbidden::MediaNotAllowed(
            MediaKind::Video
        )))
    ));
    assert!(transport.bindings().sent_of("deleteMessages").is_empty());
    finish(&transport, "upload_refused").await;
}

#[tokio::test]
async fn local_checks_send_nothing() {
    let dir = TempDir::new("m4-local");
    // Premium account: 4000 MiB.
    let transport = started(
        &[r#"{"steps":[
        {"expect":"getMe","reply":{"@type":"user","id":777000001,"first_name":"T","last_name":"",
          "usernames":null,"is_premium":true}},
        {"expect":"getOption","reply":{"@type":"optionValueInteger","value":"4096"}}]}"#],
        &dir,
    )
    .await;
    let missing = request(dir.path().join("gone.mp4"), saved(), video());
    let (result, _) = upload(&transport, missing, CancellationToken::new()).await;
    assert!(matches!(result, Err(TransportError::FileUnreadable(_))));
    let mut huge = request(clip(&dir, "big.mp4"), saved(), video());
    huge.size = 4001 * MIB;
    let (result, _) = upload(&transport, huge, CancellationToken::new()).await;
    assert!(matches!(result, Err(TransportError::FileTooLarge { limit }) if limit == 4000 * MIB));
    // The effective limit of the plan (custom or forced) wins over the account's.
    let mut capped = request(clip(&dir, "capped.mp4"), saved(), video());
    capped.limit_bytes = Some(999);
    let (result, _) = upload(&transport, capped, CancellationToken::new()).await;
    assert!(matches!(
        result,
        Err(TransportError::FileTooLarge { limit: 999 })
    ));
    let cancel = CancellationToken::new();
    cancel.cancel();
    let (result, _) = upload(
        &transport,
        request(clip(&dir, "a.mp4"), saved(), video()),
        cancel,
    )
    .await;
    assert!(matches!(result, Err(TransportError::Cancelled)));
    let replay = transport.bindings();
    assert!(replay.sent_of("sendMessage").is_empty());
    finish(&transport, "local_checks").await;
}

#[tokio::test]
async fn cancel_deletes_the_temporary_message() {
    let dir = TempDir::new("m4-cancel");
    let transport = started(&[UPLOAD_CANCEL], &dir).await;
    let cancel = CancellationToken::new();
    let (tx, mut rx) = mpsc::channel(64);
    let task = {
        let transport = Arc::clone(&transport);
        let req = request(clip(&dir, "clip_sq.mp4"), saved(), video());
        let cancel = cancel.clone();
        tokio::spawn(async move { transport.upload(req, tx, cancel).await })
    };
    // Bytes are going out.
    loop {
        let p = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        if p.phase == Phase::Uploading {
            assert_eq!(p.sent, 300);
            break;
        }
    }
    let cancelled_at = Instant::now();
    cancel.cancel();
    let result = task.await.unwrap();
    assert!(
        matches!(result, Err(TransportError::Cancelled)),
        "{result:?}"
    );
    // `updateDeleteMessages` ends the grace at once.
    assert!(cancelled_at.elapsed() < Duration::from_secs(1));
    let replay = transport.bindings();
    let delete = &replay.sent_of("deleteMessages")[0];
    assert_eq!(delete["message_ids"], json!([TEMP]));
    assert_eq!(delete["revoke"], true);
    finish(&transport, "upload_cancel").await;
}

#[tokio::test]
async fn a_cancel_after_the_success_never_deletes() {
    let dir = TempDir::new("m4-cancel-late");
    let transport = started(&[UPLOAD_CANCEL_AFTER_SUCCESS], &dir).await;
    let cancel = CancellationToken::new();
    let task = {
        let transport = Arc::clone(&transport);
        let req = request(clip(&dir, "clip_sq.mp4"), saved(), video());
        let cancel = cancel.clone();
        tokio::spawn(async move { upload(&transport, req, cancel).await })
    };
    // Cancelled while `sendMessage` is in flight; the success is already queued when the
    // upload looks at its updates.
    until("sendMessage", || {
        !transport.bindings().sent_of("sendMessage").is_empty()
    })
    .await;
    cancel.cancel();
    let (result, _) = task.await.unwrap();
    assert_eq!(result.unwrap().message_id, FINAL);
    assert!(transport.bindings().sent_of("deleteMessages").is_empty());
    finish(&transport, "upload_cancel_after_success").await;
}

#[tokio::test]
async fn a_success_crossing_the_cancel_is_reported_as_sent() {
    let dir = TempDir::new("m4-cancel-cross");
    let transport = started(&[UPLOAD_CANCEL_CROSSING], &dir).await;
    let cancel = CancellationToken::new();
    let (tx, mut rx) = mpsc::channel(64);
    let task = {
        let transport = Arc::clone(&transport);
        let req = request(clip(&dir, "clip_sq.mp4"), saved(), video());
        let cancel = cancel.clone();
        tokio::spawn(async move { transport.upload(req, tx, cancel).await })
    };
    loop {
        let p = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        if p.phase == Phase::AwaitingServer {
            break;
        }
    }
    cancel.cancel();
    let result = task.await.unwrap();
    assert_eq!(result.unwrap().message_id, FINAL, "the message went out");
    assert_eq!(transport.bindings().sent_of("deleteMessages").len(), 1);
    finish(&transport, "upload_cancel_crossing").await;
}

#[tokio::test]
async fn closing_during_the_upload_leaves_it_in_doubt() {
    let dir = TempDir::new("m4-close");
    let transport = started(&[UPLOAD_CANCEL], &dir).await;
    let task = {
        let transport = Arc::clone(&transport);
        let req = request(clip(&dir, "clip_sq.mp4"), saved(), video());
        tokio::spawn(async move { upload(&transport, req, CancellationToken::new()).await })
    };
    until("sendMessage", || {
        !transport.bindings().sent_of("sendMessage").is_empty()
    })
    .await;
    transport.shutdown().await;
    let (result, _) = tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
    let error = result.unwrap_err();
    assert!(matches!(error, TransportError::Protocol(_)), "{error}");
    assert!(matches!(
        outcome(Err(error)),
        Err(SendError::Unknown { code, .. }) if code == "TG_PROTOCOL"
    ));
    // Nothing deleted: the message may have gone out.
    let replay = transport.bindings();
    assert!(replay.sent_of("deleteMessages").is_empty());
    assert_eq!(replay.remaining(), ["deleteMessages"]);
}

// ---------- verify_sent ----------

#[tokio::test]
async fn verify_sent_is_read_only_and_matches_name_size_and_date() {
    let dir = TempDir::new("m4-verify");
    let transport = started(&[VERIFY], &dir).await;
    let video = SentProbe {
        target: saved(),
        media: MediaKind::Video,
        file_name: Some("clip_sq.mp4".into()),
        size: 1000,
        not_before_unix: NOT_BEFORE,
    };
    let found = transport.verify_sent(&video).await.unwrap();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].message_id, 900);
    assert_eq!(found[0].file_name.as_deref(), Some("clip_sq.mp4"));
    assert_eq!(found[0].size, Some(1000));
    assert_eq!(found[0].caption_preview, CAPTION);

    let photo = SentProbe {
        media: MediaKind::Photo,
        file_name: None,
        size: 123,
        ..video.clone()
    };
    let photos = transport.verify_sent(&photo).await.unwrap();
    assert_eq!(
        photos.iter().map(|c| c.message_id).collect::<Vec<_>>(),
        [950, 949]
    );
    assert!(
        photos
            .iter()
            .all(|c| c.file_name.is_none() && c.size.is_none())
    );

    let in_channel = SentProbe {
        target: target(PeerKind::Channel, CHANNEL, None),
        media: MediaKind::Document,
        file_name: Some("clip_sq.mkv".into()),
        size: 1000,
        not_before_unix: NOT_BEFORE,
    };
    let posts = transport.verify_sent(&in_channel).await.unwrap();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].kind, MediaKind::Document);

    let as_chat = SentProbe {
        target: target(PeerKind::Channel, SLOW, None),
        ..in_channel.clone()
    };
    assert!(transport.verify_sent(&as_chat).await.unwrap().is_empty());

    let replay = transport.bindings();
    let searches = replay.sent_of("searchChatMessages");
    assert_eq!(searches.len(), 5);
    let first = &searches[0];
    assert_eq!(first["chat_id"], ME);
    assert_eq!(first["topic_id"], Value::Null);
    assert_eq!(first["query"], "");
    assert_eq!(
        first["sender_id"],
        json!({ "@type": "messageSenderUser", "user_id": ME })
    );
    assert_eq!(first["filter"]["@type"], "searchMessagesFilterVideo");
    assert_eq!(
        (&first["from_message_id"], &first["limit"]),
        (&json!(0), &json!(50))
    );
    assert_eq!(searches[1]["from_message_id"], 899, "second page");
    assert_eq!(searches[2]["filter"]["@type"], "searchMessagesFilterPhoto");
    assert_eq!(
        searches[3]["sender_id"],
        Value::Null,
        "channel: no sender filter"
    );
    assert_eq!(
        searches[3]["filter"]["@type"],
        "searchMessagesFilterDocument"
    );
    assert_eq!(
        searches[4]["sender_id"],
        json!({ "@type": "messageSenderChat", "chat_id": -1_002_000_000_001i64 })
    );
    for kind in ["sendMessage", "deleteMessages", "resendMessages"] {
        assert!(replay.sent_of(kind).is_empty(), "{kind}: read only");
    }
    finish(&transport, "verify").await;
}

// ---------- regression: a subscriber holding the state ----------

/// Real read-only run of 2026-10-09: the caller kept the `watch::Ref` returned by
/// `auth_state().wait_for()` across `.await`s; the next state TDLib sent (`Closing`, after
/// `close`) blocked the receive thread on the watch's write lock, so `authorizationStateClosed`
/// was never dispatched and `shutdown` never returned. Requests and `shutdown` must stay
/// bounded whatever a subscriber holds.
#[tokio::test]
async fn a_held_auth_state_guard_never_blocks_tdlib_or_shutdown() {
    let dir = TempDir::new("held-guard");
    let transport = started(&[DIALOGS_WITHOUT_SAVED], &dir).await;
    let mut states = transport.auth_state();
    let held = states
        .wait_for(|s| matches!(s, AuthState::WaitPhone))
        .await
        .unwrap();
    // TDLib moves on while the guard is held.
    transport.bindings().inject(
        json!({ "@type": "updateAuthorizationState", "@client_id": 1,
            "authorization_state": { "@type": "authorizationStateWaitPassword",
                "password_hint": "", "has_recovery_email_address": false,
                "has_passport_data": false, "recovery_email_address_pattern": "" } })
        .to_string(),
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    // The receive thread still answers requests.
    let dialogs = tokio::time::timeout(Duration::from_secs(3), transport.list_dialogs(10))
        .await
        .expect("TDLib messages still dispatched")
        .unwrap();
    assert_eq!(dialogs.len(), 2);
    // `close` → `Closing` → `Closed` are dispatched; only the final `Closed` publication waits,
    // bounded.
    let closing = Instant::now();
    tokio::time::timeout(Duration::from_secs(5), transport.shutdown())
        .await
        .expect("bounded shutdown");
    let took = closing.elapsed();
    assert!(took < Duration::from_secs(1), "{took:?}: Closed not seen?");
    assert!(lock(&transport.inner.receiver).is_none());
    drop(held);
    tokio::time::timeout(
        Duration::from_secs(3),
        states.wait_for(|s| matches!(s, AuthState::Closed)),
    )
    .await
    .expect("Closed once the guard is released")
    .unwrap();
    let replay = transport.bindings();
    assert!(replay.unexpected().is_empty(), "{:?}", replay.unexpected());
    assert!(replay.remaining().is_empty(), "{:?}", replay.remaining());
}

// ---------- real session, read only ----------

/// The user's real TDLib session, READ ONLY: `me`, `list_dialogs` (main list, archive, every
/// folder), `list_topics`, `can_send`. Prints counts and types only (never a title, a folder
/// name or an id). Never sends, deletes,
/// joins or signs in: if the session is not signed in it stops.
///
/// The app must be CLOSED (TDLib locks its database). Same identity and device name as the
/// app (embedded identity or `telegram.api_override` with its DPAPI-protected hash).
/// Run alone:
/// `SQUEEZEGRAM_TEST_TDLIB_DIR=%LOCALAPPDATA%\squeezegram\tdlib`
/// `SQUEEZEGRAM_TEST_TDJSON=%LOCALAPPDATA%\squeezegram\tdlib\bin\tdjson.dll` (default: `bin\`
/// of that folder) `cargo test --lib real_tdjson_readonly_dialogs -- --ignored --nocapture`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "opens the user's real TDLib session (SQUEEZEGRAM_TEST_TDLIB_DIR, app closed); network on, read only"]
async fn real_tdjson_readonly_dialogs() {
    use std::collections::BTreeMap;

    // Diagnostics: the app's journal (request types and counters only, `tdlib_ffi` /
    // `tdlib_recv`, chats hashed) in `SQUEEZEGRAM_TEST_JOURNAL_DIR`, and each step on stderr.
    let journal = std::env::var_os("SQUEEZEGRAM_TEST_JOURNAL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("squeezegram-real-readonly-journal"));
    crate::logging::init(journal.clone(), Some("debug")).expect("journal");
    eprintln!("journal: {}", journal.display());
    let begun = Instant::now();
    let step = |name: &str| {
        eprintln!("[{:>7} ms] step {name}", begun.elapsed().as_millis());
        tracing::info!(
            event = "real_test_step",
            step = name,
            "real read-only test step"
        );
    };

    let dir = PathBuf::from(
        std::env::var_os("SQUEEZEGRAM_TEST_TDLIB_DIR")
            .expect("set SQUEEZEGRAM_TEST_TDLIB_DIR to the app's TDLib folder"),
    );
    // Never a new key next to an existing database: that would only fail with 401.
    assert!(
        dir.join(key::KEY_FILE).is_file() && dir.join("db").is_dir(),
        "not a signed-in squeezegram TDLib folder (key.bin and db\\ expected); nothing created"
    );
    let lib = std::env::var_os("SQUEEZEGRAM_TEST_TDJSON")
        .map(PathBuf::from)
        .unwrap_or_else(|| dir.join("bin").join(library::DLL_NAME));
    let db_path = crate::queue::db::default_path().expect("app data folder");
    let settings = crate::settings::SettingsStore::load(
        crate::settings::SettingsStore::path_next_to(&db_path),
    )
    .expect("settings.json")
    .get();
    let inputs = crate::transport::select::Inputs::from_process(&settings);
    let identity = crate::transport::select::resolve_identity(&inputs, &dir)
        .expect("API identity of this build (embedded or override)");
    let config = TdlibConfig::new(
        dir.clone(),
        identity.api_id,
        identity.api_hash,
        inputs.device_model(),
    );
    let transport = TdlibJson::with_dll(lib, config);
    step("start");
    transport
        .start()
        .await
        .expect("TDLib start (is the app closed?)");
    let mut states = transport.auth_state();
    let ready = tokio::time::timeout(
        Duration::from_secs(30),
        states.wait_for(|s| {
            matches!(
                s,
                AuthState::Ready(_)
                    | AuthState::OtherStep { .. }
                    | AuthState::WaitPhone
                    | AuthState::WaitQr { .. }
                    | AuthState::WaitCode { .. }
                    | AuthState::WaitPassword { .. }
                    | AuthState::Closed
            )
        }),
    )
    .await
    // Only whether it came: never keep the `watch::Ref` (it blocks every state publication).
    .is_ok_and(|result| result.is_ok());
    let state = format!("{:?}", *transport.auth_state().borrow());
    let signed_in = matches!(*transport.auth_state().borrow(), AuthState::Ready(_));
    if !signed_in {
        transport.shutdown().await;
        panic!(
            "session not ready ({}): sign in through the app first",
            if !ready { "timeout" } else { state.as_str() }
        );
    }
    println!("TDLib {:?}", transport.tdlib_version());
    step("me");
    let me = transport.me().await.expect("me");
    println!(
        "me: premium={} caption_max_chars={} upload_limit_mib={}",
        me.is_premium,
        me.caption_max_chars,
        me.upload_limit_bytes / MIB
    );
    let started = Instant::now();
    step("list_dialogs");
    let dialogs = transport.list_dialogs(500).await.expect("list_dialogs");
    // Deleted accounts are only flagged for the cache to drop them.
    let deleted_accounts = dialogs.iter().filter(|d| d.deleted).count();
    let dialogs: Vec<_> = dialogs.into_iter().filter(|d| !d.deleted).collect();
    println!("deleted accounts flagged: {deleted_accounts}");
    let mut kinds: BTreeMap<&str, usize> = BTreeMap::new();
    for d in &dialogs {
        let kind = match (d.peer.kind, d.is_broadcast, d.is_forum, d.is_saved_messages) {
            (_, _, _, true) => "saved_messages",
            (PeerKind::User, ..) => "private",
            (PeerKind::BasicGroup, ..) => "basic_group",
            (PeerKind::Channel, true, ..) => "channel",
            (PeerKind::Channel, _, true, _) => "forum",
            (PeerKind::Channel, ..) => "supergroup",
        };
        *kinds.entry(kind).or_default() += 1;
    }
    println!(
        "dialogs: {} in {:?}, by type {kinds:?}, saved first: {}",
        dialogs.len(),
        started.elapsed(),
        dialogs.first().is_some_and(|d| d.is_saved_messages)
    );
    // Per list (counts only; folders by rank, never their name or id).
    let mut per_folder: BTreeMap<u32, usize> = BTreeMap::new();
    for folder in dialogs.iter().flat_map(|d| &d.folders) {
        *per_folder.entry(folder.position).or_default() += 1;
    }
    println!(
        "lists: main {}, archive {}, folders {} (chats per folder rank {:?}), in a folder {}, \
         forums {} (archived {}), empty titles {}",
        dialogs.iter().filter(|d| !d.archived).count(),
        dialogs.iter().filter(|d| d.archived).count(),
        per_folder.len(),
        per_folder.values().collect::<Vec<_>>(),
        dialogs.iter().filter(|d| !d.folders.is_empty()).count(),
        dialogs.iter().filter(|d| d.is_forum).count(),
        dialogs.iter().filter(|d| d.is_forum && d.archived).count(),
        dialogs.iter().filter(|d| d.title.is_empty()).count(),
    );
    // The transport's own counts (`dialogs_loaded`: deleted accounts, failed lists…).
    if let Ok(entries) = std::fs::read_dir(&journal) {
        let mut text = String::new();
        for entry in entries.flatten() {
            text.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
        }
        if let Some(fields) = text
            .lines()
            .rev()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|line| line["event"] == "dialogs_loaded")
            .map(|line| line["fields"].clone())
        {
            let keys = [
                "main",
                "archive",
                "folders",
                "folders_known",
                "folder_chats",
                "lists_failed",
                "listed",
                "unique",
                "dialogs",
                "skipped",
                "deleted_users",
                "untitled",
                "load_rounds",
            ];
            let counts: Vec<String> = keys.iter().map(|k| format!("{k}={}", fields[k])).collect();
            println!("dialogs_loaded: {}", counts.join(" "));
        }
    }
    let mut verdicts: BTreeMap<String, usize> = BTreeMap::new();
    let mut slow_modes = 0usize;
    let mut record = |result: Result<SendCheck, TransportError>| match result {
        Ok(check) => {
            slow_modes += usize::from(check.slow_mode.is_some());
            let code = match check.verdict {
                Verdict::Allowed => "allowed".to_owned(),
                Verdict::Denied(f) => {
                    let (code, params) = f.code_and_params();
                    format!("{code}:{}", params["reason"].as_str().unwrap_or(""))
                }
            };
            *verdicts.entry(code).or_default() += 1;
        }
        Err(error) => {
            *verdicts
                .entry(format!("error:{}", error.code_and_params().0))
                .or_default() += 1;
        }
    };
    // Private chats whose TDLib title is empty: what `getUser` says about them, where they
    // are listed, and whether `can_send` allows them. Counts only.
    step("untitled");
    {
        use super::chats::ChatList;
        let inner = &transport.inner;
        let (folders, _) = inner.chat_folders().await;
        let mut lists = vec![(ChatList::Main, "main"), (ChatList::Archive, "archive")];
        lists.extend(folders.iter().map(|f| (ChatList::Folder(f.id), "folder")));
        let mut membership: std::collections::HashMap<i64, Vec<&str>> = Default::default();
        let mut order = Vec::new();
        for (list, name) in &lists {
            let (_, ids) = inner.list_ids(*list, 500).await.expect("getChats");
            for id in ids {
                let entry = membership.entry(id).or_insert_with(|| {
                    order.push(id);
                    Vec::new()
                });
                if !entry.contains(name) {
                    entry.push(name);
                }
            }
        }
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut bump = |key: String| *counts.entry(key).or_default() += 1;
        for id in order {
            let chat = inner.get_chat(id).await.expect("getChat");
            let Ok(super::chats::ChatType::Private { user_id }) =
                super::chats::ChatType::parse(&chat)
            else {
                continue;
            };
            if user_id == me.user_id || !chat["title"].as_str().unwrap_or("").trim().is_empty() {
                continue;
            }
            bump("untitled_private".into());
            for list in &membership[&id] {
                bump(format!("in_{list}"));
            }
            bump(format!(
                "has_last_message={}",
                !chat["last_message"].is_null()
            ));
            match inner.user(user_id).await {
                Ok(user) => {
                    bump(format!(
                        "type={}",
                        user["type"]["@type"].as_str().unwrap_or("?")
                    ));
                    bump(format!("have_access={}", user["have_access"]));
                    let named = |k: &str| !user[k].as_str().unwrap_or("").trim().is_empty();
                    let username = user["usernames"]["active_usernames"][0].is_string();
                    bump(format!(
                        "first={} last={} username={}",
                        named("first_name"),
                        named("last_name"),
                        username
                    ));
                }
                Err(error) => bump(format!("getUser_error={}", error.code_and_params().0)),
            }
            let verdict = match transport
                .can_send(
                    &Target {
                        peer: Peer {
                            kind: PeerKind::User,
                            raw_id: user_id,
                        },
                        topic_id: None,
                    },
                    MediaKind::Video,
                )
                .await
            {
                Ok(check) => match check.verdict {
                    Verdict::Allowed => "allowed".to_owned(),
                    Verdict::Denied(f) => {
                        let (code, params) = f.code_and_params();
                        format!("{code}:{}", params["reason"].as_str().unwrap_or(""))
                    }
                },
                Err(error) => format!("error:{}", error.code_and_params().0),
            };
            bump(format!("can_send={verdict}"));
        }
        println!("untitled private chats: {counts:?}");
    }
    step("topics");
    for (index, forum) in dialogs.iter().filter(|d| d.is_forum).take(10).enumerate() {
        match transport.list_topics(&forum.peer).await {
            Ok(topics) => {
                println!(
                    "forum #{index}: {} topics, {} closed, general present: {}",
                    topics.len(),
                    topics.iter().filter(|t| t.is_closed).count(),
                    topics.iter().any(|t| t.is_general)
                );
                if let Some(topic) = topics.iter().find(|t| !t.is_general) {
                    record(
                        transport
                            .can_send(
                                &Target {
                                    peer: forum.peer,
                                    topic_id: Some(topic.id),
                                },
                                MediaKind::Video,
                            )
                            .await,
                    );
                }
            }
            Err(error) => println!("forum #{index}: error {}", error.code_and_params().0),
        }
    }
    step("can_send");
    for dialog in dialogs.iter().filter(|d| !d.is_forum).take(40) {
        record(
            transport
                .can_send(
                    &Target {
                        peer: dialog.peer,
                        topic_id: None,
                    },
                    MediaKind::Video,
                )
                .await,
        );
    }
    println!("can_send (video): {verdicts:?}, slow mode active: {slow_modes}");
    step("shutdown");
    transport.shutdown().await;
    step("shut_down");
    assert!(matches!(
        *transport.auth_state().borrow(),
        AuthState::Closed
    ));
    drop(transport);
    step("dropped");
}

/// `message_link`, read only: the message is loaded first (no message database), then linked
/// inside its forum topic; a message deleted since has no link, and a private chat none
/// either, without a single request.
#[tokio::test]
async fn message_links_load_the_message_then_ask_for_its_link() {
    let dir = TempDir::new("message-link");
    let transport = started(&[], &dir).await;
    let forum = -1_000_000_000_000 - FORUM;
    let link = "https://t.me/c/1234567890/2/3145728";
    transport.bindings().push_steps(super::tests::steps(json!([
        { "expect": "createSupergroupChat", "reply": { "@type": "chat", "id": forum } },
        { "expect": "getMessage", "reply": { "@type": "message", "id": FINAL, "chat_id": forum } },
        { "expect": "getMessageLink",
          "reply": { "@type": "messageLink", "link": link, "is_public": false } },
        { "expect": "createSupergroupChat", "reply": { "@type": "chat", "id": forum } },
        { "expect": "getMessage",
          "reply": { "@type": "error", "code": 404, "message": "Message not found" } }
    ])));
    let topic = target(PeerKind::Channel, FORUM, Some(2));
    assert_eq!(
        transport
            .message_link(&topic, FINAL)
            .await
            .unwrap()
            .as_deref(),
        Some(link)
    );
    let replay = transport.bindings();
    let request = &replay.sent_of("getMessageLink")[0];
    assert_eq!(
        (
            &request["chat_id"],
            &request["message_id"],
            &request["media_timestamp"],
            &request["checklist_task_id"],
            &request["poll_option_id"],
            &request["for_album"],
            &request["in_message_thread"],
        ),
        (
            &json!(forum),
            &json!(FINAL),
            &json!(0),
            &json!(0),
            &json!(""),
            &json!(false),
            &json!(true),
        )
    );
    assert!(
        position_of(&replay.calls(), "send:getMessage")
            < position_of(&replay.calls(), "send:getMessageLink")
    );

    assert_eq!(
        transport.message_link(&topic, FINAL).await.unwrap(),
        None,
        "deleted since"
    );
    let sent = replay.sent().len();
    assert_eq!(transport.message_link(&saved(), FINAL).await.unwrap(), None);
    assert_eq!(replay.sent().len(), sent, "no request for a private chat");
    assert_read_only(replay);
    finish(&transport, "message_link").await;
}

fn position_of(calls: &[String], call: &str) -> usize {
    calls.iter().position(|c| c == call).unwrap()
}
