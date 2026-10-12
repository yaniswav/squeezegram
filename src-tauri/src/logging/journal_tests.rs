//! End-to-end journal tests: the real code paths (sign-in, settings, queue, upload, UI lines)
//! write into a temporary journal, then the files are read back.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::frontend::{self, FrontendEntry};
use super::testing::TestJournal;
use crate::queue::State;
use crate::queue::events::recorder::Recorder;
use crate::queue::events::{Phase, Progress, ProgressReporter};
use crate::queue::orchestrator::{
    MediaOutput, MediaStage, Orchestrator, PauseRange, ProbeOutcome, StageContext, StageError,
    Treatment,
};
use crate::queue::store::{Item, NewItem, SendMode, Store};
use crate::queue::test_util::TestDir;
use crate::settings::{FILE_NAME, SettingsStore};
use crate::transport::mock::{MOCK_CODE, MOCK_FORUM, MOCK_ME_ID, MockTransport};
use crate::transport::service::{
    check_destination, load_topics, log_auth_state, refresh_destinations, search_destinations,
};
use crate::transport::stage::TransportStage;
use crate::transport::{AuthInput, AuthState, TelegramTransport};

const PHONE: &str = "+33612345678";
const EMAIL: &str = "alice.secret@example.com";
const WRONG_EMAIL_CODE: &str = "73915";
const WRONG_CODE: &str = "86429";
const WRONG_PASSWORD: &str = "hunter2-Pa55word!";
const CAPTION: &str = "CAPTION-PRIVATE-TEXT";
const API_ID: i64 = 9_876_543;
const DEVICE: &str = "DEVICE-SECRET-NAME";
/// Typed in the destination search (matches "Mock old friend").
const SEARCH: &str = "old fri";

/// Sends the source as is (no ffmpeg here).
struct AsIs;

#[async_trait]
impl MediaStage for AsIs {
    async fn probe(&self, item: &Item, _: &StageContext) -> Result<ProbeOutcome, StageError> {
        Ok(ProbeOutcome {
            content_id: format!("{}:{}", item.source_size, item.id),
            identity_weak: false,
            media_json: None,
            plan_json: None,
            treatment: Treatment::Skip,
            temp_path: None,
        })
    }

    async fn encode(&self, _: &Item, _: &StageContext) -> Result<MediaOutput, StageError> {
        Err(StageError::failed("FFMPEG_FAILED"))
    }

    async fn remux(&self, _: &Item, _: &StageContext) -> Result<MediaOutput, StageError> {
        Err(StageError::failed("FFMPEG_FAILED"))
    }
}

async fn submit(mock: &MockTransport, input: AuthInput) -> bool {
    let ok = crate::commands::submit_auth(mock, input).await.is_ok();
    log_auth_state(&mock.auth_state().borrow());
    ok
}

fn events<'a>(lines: &'a [Value], name: &str) -> Vec<&'a Value> {
    lines.iter().filter(|l| l["event"] == name).collect()
}

#[tokio::test(start_paused = true)]
async fn no_secret_reaches_the_journal() {
    let dir = TestDir::new("journal-privacy");
    let logs = dir.path().join("logs");
    let journal = TestJournal::new(&logs, "trace");

    // 1. Sign-in with every kind of input, wrong values included.
    let mock = MockTransport::new().with_email().with_password();
    mock.start().await.unwrap();
    log_auth_state(&mock.auth_state().borrow());
    assert!(submit(&mock, AuthInput::RequestQr).await);
    let qr = match &*mock.auth_state().borrow() {
        AuthState::WaitQr { link } => link.clone(),
        other => panic!("{other:?}"),
    };
    assert!(submit(&mock, AuthInput::Phone(PHONE.into())).await);
    assert!(submit(&mock, AuthInput::Email(EMAIL.into())).await);
    assert!(!submit(&mock, AuthInput::EmailCode(WRONG_EMAIL_CODE.into())).await);
    assert!(submit(&mock, AuthInput::EmailCode(MOCK_CODE.into())).await);
    assert!(!submit(&mock, AuthInput::Code(WRONG_CODE.into())).await);
    assert!(submit(&mock, AuthInput::Code(MOCK_CODE.into())).await);
    let hint = match &*mock.auth_state().borrow() {
        AuthState::WaitPassword { hint } => hint.clone(),
        other => panic!("{other:?}"),
    };
    assert!(!submit(&mock, AuthInput::Password(WRONG_PASSWORD.into())).await);

    // 2. Settings holding identity values.
    let settings = Arc::new(SettingsStore::load(dir.path().join(FILE_NAME)).unwrap());
    settings
        .set(
            &json!({ "telegram": { "api_override": { "api_id": API_ID }, "device_name": DEVICE } }),
        )
        .unwrap();

    // 3. A captioned item sent to a forum topic, through the orchestrator.
    let mock = Arc::new(MockTransport::authorized());
    let queue = Orchestrator::new(
        Store::open_in_memory().unwrap(),
        Arc::new(AsIs),
        Arc::new(TransportStage::new(
            mock.clone(),
            settings.clone(),
            Arc::new(crate::media::thumb::Unavailable),
        )),
        Arc::new(Recorder::default()),
        PauseRange {
            min: Duration::ZERO,
            max: Duration::ZERO,
        },
    );
    let destinations = refresh_destinations(mock.as_ref(), &queue).await.unwrap();
    let forum = destinations
        .iter()
        .find(|d| d.peer_id == MOCK_FORUM.raw_id && d.topic_id == 0)
        .unwrap()
        .id;
    let topics = load_topics(mock.as_ref(), &queue, forum).await.unwrap();
    // A search typed by the user, and chat folders: never in the journal.
    search_destinations(mock.as_ref(), &queue, SEARCH)
        .await
        .unwrap();
    let topic = topics.iter().find(|d| d.topic_id == 2).unwrap().clone();
    check_destination(mock.as_ref(), &topic, SendMode::Video)
        .await
        .unwrap();
    let source = dir.file("clip.mp4", &[7u8; 300]);
    let id = queue
        .add(&[NewItem {
            source_path: source.clone(),
            source_size: 300,
            source_mtime: 1,
            destination_id: topic.id,
            send_mode: SendMode::Video,
            caption: CAPTION.into(),
        }])
        .unwrap()[0];
    let runner = queue.clone();
    let task = tokio::spawn(async move { runner.run().await });
    queue.start();
    for _ in 0..600 {
        if queue.with_store(|s| s.get_item(id)).unwrap().state == State::Sent {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    task.abort();
    assert_eq!(
        queue.with_store(|s| s.get_item(id)).unwrap().state,
        State::Sent
    );

    // 4. UI lines carrying secrets by mistake.
    frontend::record(&FrontendEntry {
        level: "error".into(),
        event: "js_error".into(),
        msg: format!("failed for {PHONE} / {EMAIL}"),
        fields: Some(json!({ "stack": format!("at {qr}"), "detail": EMAIL })),
    });

    let text = journal.text();
    let digits = PHONE.trim_start_matches('+');
    let forum_chat = (-1_000_000_000_000 - MOCK_FORUM.raw_id).to_string();
    for secret in [
        PHONE,
        digits,
        EMAIL,
        WRONG_EMAIL_CODE,
        WRONG_CODE,
        WRONG_PASSWORD,
        qr.as_str(),
        hint.as_str(),
        CAPTION,
        &API_ID.to_string(),
        DEVICE,
        &MOCK_FORUM.raw_id.to_string(),
        &forum_chat,
        &MOCK_ME_ID.to_string(),
        "Mock User",
        "mock_user",
        "Mock forum",
        "Mock Work",
        "Mock Family",
        "Mock old",
        SEARCH,
    ] {
        assert!(!secret.is_empty());
        assert!(!text.contains(secret), "{secret:?} leaked into the journal");
    }

    // What the journal does say.
    let lines = journal.lines();
    let kinds: Vec<&str> = events(&lines, "command")
        .iter()
        .filter(|l| l["fields"]["command"] == "auth_submit")
        .map(|l| l["fields"]["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "request_qr",
            "phone",
            "email",
            "email_code",
            "email_code",
            "code",
            "code",
            "password"
        ]
    );
    let failed: Vec<&str> = events(&lines, "command")
        .iter()
        .filter_map(|l| l["error_code"].as_str())
        .collect();
    assert_eq!(failed, ["TG_REJECTED", "TG_REJECTED", "TG_REJECTED"]);
    let states: Vec<&str> = events(&lines, "auth_state")
        .iter()
        .map(|l| l["fields"]["state"].as_str().unwrap())
        .collect();
    assert!(
        states.contains(&"wait_qr") && states.contains(&"wait_password"),
        "{states:?}"
    );
    assert_eq!(
        events(&lines, "settings_changed")[0]["fields"]["keys"],
        json!(["telegram.api_override.api_id", "telegram.device_name"])
    );
    let transitions: Vec<(String, String)> = events(&lines, "item_state")
        .iter()
        .filter(|l| l["item_id"] == id)
        .map(|l| {
            (
                format!(
                    "{}->{}",
                    l["fields"]["from"].as_str().unwrap(),
                    l["fields"]["to"].as_str().unwrap()
                ),
                l["fields"]["reason"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        transitions,
        [
            ("ready->probing".to_owned(), "picked".to_owned()),
            ("probing->encoded".to_owned(), "sendable_as_is".to_owned()),
            ("encoded->uploading".to_owned(), "upload_start".to_owned()),
            ("uploading->sent".to_owned(), "sent".to_owned()),
        ]
    );
    let upload = events(&lines, "upload_start")[0];
    assert_eq!(
        upload["item_id"], id,
        "item id from the orchestrator's span"
    );
    assert_eq!(upload["destination_id"], topic.id);
    assert!(upload["fields"]["chat"].as_str().unwrap().starts_with("c:"));
    assert_eq!(upload["fields"]["caption_chars"], CAPTION.len());
    assert!(
        upload["fields"]["path"]
            .as_str()
            .unwrap()
            .ends_with("clip.mp4"),
        "local paths are kept"
    );
    assert!(events(&lines, "upload_done")[0]["dur_ms"].is_u64());
    assert!(
        events(&lines, "destination_check")[0]["fields"]["chat"]
            .as_str()
            .unwrap()
            .starts_with("c:")
    );
    let ui = events(&lines, "js_error")[0];
    assert_eq!(ui["target"], frontend::TARGET);
    assert_eq!(ui["msg"], "failed for <phone> / <email>");
    assert_eq!(
        ui["fields"],
        json!({ "stack": "at <tg_link>", "detail": "<email>" })
    );

    // The export of this journal masks the local paths too.
    let out = dir.path().join("export.zip");
    super::export::write_zip(&out, &json!({}), &logs).unwrap();
    let mut zip = zip::ZipArchive::new(std::fs::File::open(&out).unwrap()).unwrap();
    let mut exported = String::new();
    for index in 0..zip.len() {
        std::io::Read::read_to_string(&mut zip.by_index(index).unwrap(), &mut exported).unwrap();
    }
    let folder = std::path::Path::new(&source)
        .parent()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(!exported.contains(&folder), "full path exported");
    assert!(exported.contains("<path>/clip.mp4"));
}

#[tokio::test(start_paused = true)]
async fn progress_is_sampled_every_5_seconds() {
    let dir = TestDir::new("journal-progress");
    let journal = TestJournal::new(dir.path(), "info");
    let reporter = ProgressReporter::new(9, Phase::Encoding, Arc::new(Recorder::default()));
    // 10 Hz for 20 s, then completion.
    for step in 0..200 {
        reporter.report(Progress {
            percent: f64::from(step) / 2.0,
            bytes_per_sec: Some(1000.4),
            ..Default::default()
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    reporter.report(Progress {
        percent: 100.0,
        ..Default::default()
    });
    let lines = journal.lines();
    let progress = events(&lines, "progress");
    assert_eq!(progress.len(), 4 + 1, "0, 5, 10, 15 s, then 100 %");
    assert!(
        progress
            .iter()
            .all(|l| l["item_id"] == 9 && l["fields"]["phase"] == "encoding")
    );
    assert_eq!(progress[1]["fields"]["percent"], 25.0);
    assert_eq!(progress[1]["fields"]["bytes_per_sec"], 1000.0);
    assert_eq!(progress.last().unwrap()["fields"]["percent"], 100.0);
}

/// TDLib's proxy errors name the server: the page gets the message, the journal never does
/// (command error line, startup alert).
#[tokio::test]
async fn a_proxy_error_never_writes_the_proxy_address() {
    let dir = TestDir::new("journal-proxy");
    let journal = TestJournal::new(dir.path(), "trace");
    let message = "Failed to connect to 10.9.9.9:1080";
    let error = crate::commands::CommandError::from(crate::transport::TransportError::Proxy(
        message.to_owned(),
    ));
    assert_eq!(error.code, "TG_PROXY_FAILED");
    assert_eq!(
        error.params,
        json!({ "message": message }),
        "the page gets it"
    );
    crate::queue::events::log_alert(None, error.code, &error.params);
    let text = journal.text();
    assert!(text.contains("TG_PROXY_FAILED"), "{text}");
    assert!(!text.contains("10.9.9.9"), "{text}");
}
