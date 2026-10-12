//! Read only: chats made known to TDLib, dialogs, forum topics, chat search and the
//! pre-send check. Every request and field is named after `td_api.tl` of TDLib 1.8.68
//! (commit c15d3f5a): `loadChats`, `getChats` (`chatListMain`, `chatListArchive`,
//! `chatListFolder`), `updateChatFolders`, `getChat`, `getUser`, `getBasicGroup`,
//! `getSupergroup`, `getSupergroupFullInfo`, `getForumTopics`, `getForumTopic`,
//! `searchChats`, `searchChatsOnServer`, `searchPublicChats`, `canSendMessageToUser`,
//! `createPrivateChat` / `createBasicGroupChat` / `createSupergroupChat`. None of them changes
//! anything on the server (no archive, no read mark, no join).
//!
//! Rights (`chatMemberStatus*` of 1.8.68): the current user's status comes
//! from `getBasicGroup` / `getSupergroup` (`status`, kept current by TDLib, no network), the
//! default rights of non-administrators from `chat.permissions`, individual restrictions from
//! `chatMemberStatusRestricted.permissions`.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

use super::json::{self as j, int, int_or_zero};
use super::{Inner, TdBindings, lock};
use crate::transport::{
    ChatFolder, ChatKind, Dialog, Forbidden, Me, MediaKind, Peer, PeerKind, SendCheck, SlowMode,
    Target, Topic, TransportError, Verdict,
};

/// `loadChats` page; TDLib may load fewer.
const LOAD_CHATS_PAGE: i32 = 100;
/// Bound on `loadChats` rounds (TDLib answers 404 when everything is loaded).
const MAX_LOAD_ROUNDS: usize = 200;
/// `getForumTopics.limit` (at most 100).
const TOPICS_PAGE: i32 = 100;
const MAX_TOPIC_PAGES: usize = 50;
/// Upper bound of `searchChats.limit` / `searchChatsOnServer.limit` the app asks for.
const SEARCH_LIMIT_MAX: usize = 50;
/// How often `list_dialogs` looks for the first `updateChatFolders`.
const FOLDERS_POLL: Duration = Duration::from_millis(20);
/// Title of the chat with oneself (TDLib gives the account's own name).
pub const SAVED_TITLE: &str = "Saved Messages";
/// `forum_topic_id` of a forum's General topic.
pub const GENERAL_TOPIC_ID: i32 = 1;
/// The update that carries the chat folders (`td_api.tl`: sent after sign-in and on change).
pub const UPDATE_CHAT_FOLDERS: &str = "updateChatFolders";

/// A TDLib chat list (`ChatList`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatList {
    Main,
    Archive,
    Folder(i32),
}

impl ChatList {
    pub fn to_json(self) -> Value {
        match self {
            ChatList::Main => json!({ "@type": "chatListMain" }),
            ChatList::Archive => json!({ "@type": "chatListArchive" }),
            ChatList::Folder(id) => json!({ "@type": "chatListFolder", "chat_folder_id": id }),
        }
    }
}

/// `updateChatFolders.chat_folders` (`chatFolderInfo`), in the user's order. The name is the
/// plain text of `chatFolderName.text` (custom emoji entities dropped).
pub fn parse_folders(update: &Value) -> Result<Vec<ChatFolder>, TransportError> {
    j::array(update, "chat_folders")?
        .iter()
        .enumerate()
        .map(|(position, info)| {
            j::expect(info, "chatFolderInfo")?;
            let id = i32::try_from(int(info, "id")?).map_err(|_| j::missing(info, "id"))?;
            let name = info["name"]["text"]["text"]
                .as_str()
                .ok_or_else(|| j::missing(info, "name"))?;
            Ok(ChatFolder {
                id,
                name: name.trim().to_owned(),
                position: u32::try_from(position).unwrap_or(u32::MAX),
            })
        })
        .collect()
}

/// Lists a chat was found in by `list_dialogs`.
#[derive(Default)]
struct Membership {
    archived: bool,
    folders: Vec<ChatFolder>,
}

/// Counts for the journal (never a title or an id).
#[derive(Default)]
struct Tally {
    private: usize,
    basic_groups: usize,
    supergroups: usize,
    channels: usize,
    forums: usize,
    skipped: usize,
    deleted_users: usize,
    untitled: usize,
}

/// Name of a user whose chat has no title: first and last name, else the username, else
/// empty (the UI shows a placeholder).
fn user_title(user: &Value) -> String {
    let first = user["first_name"].as_str().unwrap_or_default();
    let last = user["last_name"].as_str().unwrap_or_default();
    let name = format!("{first} {last}").trim().to_owned();
    if name.is_empty() {
        user["usernames"]["active_usernames"][0]
            .as_str()
            .map(|u| format!("@{u}"))
            .unwrap_or_default()
    } else {
        name
    }
}

/// `can_write` of a group: the rule of `decide` allows at least one media kind. Topic, slow
/// mode and individual checks stay for `can_send`; `None` if the status is not readable.
fn approx_can_write(group: Group, status: &Value, permissions: &Value) -> Option<bool> {
    let status = Status::parse(status).ok()?;
    let perms = Perms::parse(permissions).ok()?;
    Some(
        [MediaKind::Video, MediaKind::Document, MediaKind::Photo]
            .into_iter()
            .any(|media| decide(group, status, perms, media) == Verdict::Allowed),
    )
}

/// `chat.type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatType {
    Private {
        user_id: i64,
    },
    BasicGroup {
        basic_group_id: i64,
    },
    Supergroup {
        supergroup_id: i64,
        is_channel: bool,
    },
    Secret,
}

impl ChatType {
    pub fn parse(chat: &Value) -> Result<Self, TransportError> {
        let kind = &chat["type"];
        Ok(match j::kind(kind) {
            "chatTypePrivate" => ChatType::Private {
                user_id: int(kind, "user_id")?,
            },
            "chatTypeBasicGroup" => ChatType::BasicGroup {
                basic_group_id: int(kind, "basic_group_id")?,
            },
            "chatTypeSupergroup" => ChatType::Supergroup {
                supergroup_id: int(kind, "supergroup_id")?,
                is_channel: j::boolean(kind, "is_channel")?,
            },
            "chatTypeSecret" => ChatType::Secret,
            _ => return Err(j::missing(chat, "type")),
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ChatType::Private { .. } => "private",
            ChatType::BasicGroup { .. } => "basic_group",
            ChatType::Supergroup {
                is_channel: true, ..
            } => "channel",
            ChatType::Supergroup { .. } => "supergroup",
            ChatType::Secret => "secret",
        }
    }
}

pub fn chat_id(chat: &Value) -> Result<i64, TransportError> {
    j::expect(chat, "chat")?;
    int(chat, "id")
}

// ---------- rights (pure, unit tested) ----------

/// The three media rights of a `chatPermissions` object the app needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Perms {
    pub videos: bool,
    pub photos: bool,
    pub documents: bool,
}

impl Perms {
    pub const ALL: Perms = Perms {
        videos: true,
        photos: true,
        documents: true,
    };

    pub fn parse(permissions: &Value) -> Result<Self, TransportError> {
        Ok(Perms {
            videos: j::boolean(permissions, "can_send_videos")?,
            photos: j::boolean(permissions, "can_send_photos")?,
            documents: j::boolean(permissions, "can_send_documents")?,
        })
    }

    pub fn allows(self, media: MediaKind) -> bool {
        match media {
            MediaKind::Video => self.videos,
            MediaKind::Photo => self.photos,
            MediaKind::Document => self.documents,
        }
    }
}

/// `ChatMemberStatus` of the current user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Creator {
        is_member: bool,
    },
    Admin {
        can_post_messages: bool,
        can_manage_topics: bool,
    },
    Member,
    Restricted {
        is_member: bool,
        until_unix: Option<i64>,
        perms: Perms,
    },
    Left,
    Banned {
        until_unix: Option<i64>,
    },
}

/// `0` means "forever" in `restricted_until_date` / `banned_until_date`.
fn until(object: &Value, field: &str) -> Result<Option<i64>, TransportError> {
    let date = int(object, field)?;
    Ok((date > 0).then_some(date))
}

impl Status {
    pub fn parse(status: &Value) -> Result<Self, TransportError> {
        Ok(match j::kind(status) {
            "chatMemberStatusCreator" => Status::Creator {
                is_member: j::boolean(status, "is_member")?,
            },
            "chatMemberStatusAdministrator" => {
                let rights = &status["rights"];
                Status::Admin {
                    can_post_messages: j::boolean(rights, "can_post_messages")?,
                    can_manage_topics: j::boolean(rights, "can_manage_topics")?,
                }
            }
            "chatMemberStatusMember" => Status::Member,
            "chatMemberStatusRestricted" => Status::Restricted {
                is_member: j::boolean(status, "is_member")?,
                until_unix: until(status, "restricted_until_date")?,
                perms: Perms::parse(&status["permissions"])?,
            },
            "chatMemberStatusLeft" => Status::Left,
            "chatMemberStatusBanned" => Status::Banned {
                until_unix: until(status, "banned_until_date")?,
            },
            _ => return Err(j::missing(status, "@type")),
        })
    }

    /// Creator or administrator: exempt from slow mode and paid messages.
    pub fn is_privileged(self) -> bool {
        matches!(self, Status::Creator { .. } | Status::Admin { .. })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Creator { .. } => "creator",
            Status::Admin { .. } => "administrator",
            Status::Member => "member",
            Status::Restricted { .. } => "restricted",
            Status::Left => "left",
            Status::Banned { .. } => "banned",
        }
    }
}

/// What the rule needs of a basic group or a supergroup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    /// `basicGroup.is_active` and not upgraded (`upgraded_to_supergroup_id == 0`).
    Basic { active: bool },
    Super {
        is_channel: bool,
        /// `supergroup.is_broadcast_group`: only administrators post.
        is_broadcast_group: bool,
        join_to_send_messages: bool,
        paid_message_star_count: i64,
    },
}

/// Rights of the current user: banned or left → no; restricted → its own permissions; member →
/// `chat.permissions`; creator / administrator → yes (in a channel, `can_post_messages`);
/// channel or broadcast group → administrators only; then paid messages for non-privileged
/// supergroup members. Topic and slow mode are checked by the caller.
pub fn decide(group: Group, status: Status, chat_perms: Perms, media: MediaKind) -> Verdict {
    let denied = Verdict::Denied;
    let member_rule = |perms: Perms, refused: Forbidden, paid: i64| {
        if !perms.allows(media) {
            denied(refused)
        } else if paid > 0 {
            denied(Forbidden::PaidMessages { stars: paid })
        } else {
            Verdict::Allowed
        }
    };
    match group {
        Group::Basic { active: false } => denied(Forbidden::NotMember),
        Group::Basic { active: true } => match status {
            Status::Creator { is_member: false } | Status::Left => denied(Forbidden::NotMember),
            Status::Banned { until_unix } => denied(Forbidden::Banned { until_unix }),
            Status::Creator { .. } | Status::Admin { .. } => Verdict::Allowed,
            Status::Member => member_rule(chat_perms, Forbidden::MediaNotAllowed(media), 0),
            Status::Restricted {
                until_unix, perms, ..
            } => member_rule(perms, Forbidden::Restricted { until_unix }, 0),
        },
        Group::Super {
            is_channel,
            is_broadcast_group,
            join_to_send_messages,
            paid_message_star_count: paid,
        } => {
            let admins_only = is_channel || is_broadcast_group;
            match status {
                Status::Banned { until_unix } => denied(Forbidden::Banned { until_unix }),
                Status::Creator { is_member: false } => denied(Forbidden::NotMember),
                Status::Creator { .. } => Verdict::Allowed,
                Status::Admin {
                    can_post_messages, ..
                } => {
                    if is_channel && !can_post_messages {
                        denied(Forbidden::AdminsOnly)
                    } else {
                        Verdict::Allowed
                    }
                }
                _ if admins_only => denied(Forbidden::AdminsOnly),
                Status::Left if join_to_send_messages => denied(Forbidden::JoinRequired),
                Status::Left | Status::Member => {
                    member_rule(chat_perms, Forbidden::MediaNotAllowed(media), paid)
                }
                Status::Restricted {
                    is_member: false, ..
                } if join_to_send_messages => denied(Forbidden::JoinRequired),
                Status::Restricted {
                    until_unix, perms, ..
                } => member_rule(perms, Forbidden::Restricted { until_unix }, paid),
            }
        }
    }
}

/// `canSendMessageToUserResult*` → verdict (private chats with someone else).
pub fn user_verdict(result: &Value) -> Result<Verdict, TransportError> {
    Ok(match j::kind(result) {
        "canSendMessageToUserResultOk" => Verdict::Allowed,
        "canSendMessageToUserResultUserHasPaidMessages" => {
            Verdict::Denied(Forbidden::PaidMessages {
                stars: int(result, "outgoing_paid_message_star_count")?,
            })
        }
        "canSendMessageToUserResultUserIsDeleted" => Verdict::Denied(Forbidden::NotMember),
        "canSendMessageToUserResultUserRestrictsNewChats" => {
            Verdict::Denied(Forbidden::Restricted { until_unix: None })
        }
        _ => return Err(j::missing(result, "@type")),
    })
}

fn verdict_code(verdict: &Verdict) -> String {
    match verdict {
        Verdict::Allowed => "allowed".to_owned(),
        Verdict::Denied(forbidden) => {
            let (code, params) = forbidden.code_and_params();
            match params["reason"].as_str() {
                Some(reason) => format!("{code}:{reason}"),
                None => code.to_owned(),
            }
        }
    }
}

fn redact(chat_id: i64) -> String {
    crate::logging::redact::chat(chat_id)
}

// ---------- requests ----------

impl<B: TdBindings> Inner<B> {
    async fn call(&self, what: &'static str, body: Value) -> Result<Value, TransportError> {
        self.request(what, body, self.config.request_timeout).await
    }

    /// One `create*Chat` (`force = false`: TDLib checks the peer it knows).
    async fn create_chat(&self, peer: &Peer) -> Result<Value, TransportError> {
        let (what, body) = match peer.kind {
            PeerKind::User => (
                "createPrivateChat",
                json!({ "@type": "createPrivateChat", "user_id": peer.raw_id, "force": false }),
            ),
            PeerKind::BasicGroup => (
                "createBasicGroupChat",
                json!({ "@type": "createBasicGroupChat", "basic_group_id": peer.raw_id, "force": false }),
            ),
            PeerKind::Channel => (
                "createSupergroupChat",
                json!({ "@type": "createSupergroupChat", "supergroup_id": peer.raw_id, "force": false }),
            ),
        };
        let chat = self.call(what, body).await?;
        let id =
            chat_id(&chat).map_err(|_| TransportError::Protocol(format!("{what}: not a chat")))?;
        if id != peer.tdlib_chat_id() {
            return Err(TransportError::Protocol(format!(
                "{what}: chat id does not match the peer"
            )));
        }
        Ok(chat)
    }

    /// With `use_message_database = false` a chat not loaded in memory is
    /// `400 Chat not found`. `create*Chat` first; if TDLib refuses (peer unknown), `loadChats`
    /// of the main list until 404 and one more `create*Chat`; then the archive and the
    /// folders (an archived chat, or one only in a folder). Loading changes nothing on the
    /// server. Returns the `chat` object.
    pub(super) async fn known_chat(&self, peer: &Peer) -> Result<Value, TransportError> {
        match self.create_chat(peer).await {
            Err(TransportError::Other { code: 400, .. }) => {
                tracing::info!(
                    event = "chat_unknown",
                    chat = redact(peer.tdlib_chat_id()).as_str(),
                    peer_kind = peer.kind.as_str(),
                    "chat unknown to TDLib, loading the chat lists"
                );
                self.load_all_chats(ChatList::Main).await?;
                match self.create_chat(peer).await {
                    Err(TransportError::Other { code: 400, .. }) => {
                        let mut others = vec![ChatList::Archive];
                        let folders = lock(&self.folders).clone().unwrap_or_default();
                        others.extend(folders.iter().map(|f| ChatList::Folder(f.id)));
                        for list in others {
                            match self.load_all_chats(list).await {
                                Ok(_) | Err(TransportError::Other { code: 400, .. }) => {}
                                Err(error) => return Err(error),
                            }
                        }
                        self.create_chat(peer).await
                    }
                    other => other,
                }
            }
            other => other,
        }
    }

    /// `loadChats(list)` until TDLib answers 404 (everything loaded). Returns the number of
    /// rounds.
    pub(super) async fn load_all_chats(&self, list: ChatList) -> Result<usize, TransportError> {
        for round in 1..=MAX_LOAD_ROUNDS {
            let body = json!({
                "@type": "loadChats",
                "chat_list": list.to_json(),
                "limit": LOAD_CHATS_PAGE,
            });
            match self.call("loadChats", body).await {
                Ok(_) => {}
                Err(TransportError::Other { code: 404, .. }) => return Ok(round),
                Err(error) => return Err(error),
            }
        }
        Err(TransportError::Protocol(
            "loadChats: no end of the chat list".into(),
        ))
    }

    /// Chat ids of a `chats` answer.
    async fn chat_ids(&self, what: &'static str, body: Value) -> Result<Vec<i64>, TransportError> {
        let chats = self.call(what, body).await?;
        j::expect(&chats, "chats")?;
        j::array(&chats, "chat_ids")?
            .iter()
            .map(|id| j::int_value(id).ok_or_else(|| j::missing(&chats, "chat_ids")))
            .collect()
    }

    /// `loadChats` until 404, then `getChats` (TDLib order: pinned first, then by
    /// `chatPosition.order`). Returns the rounds and the ids.
    pub(super) async fn list_ids(
        &self,
        list: ChatList,
        limit: i32,
    ) -> Result<(usize, Vec<i64>), TransportError> {
        let rounds = self.load_all_chats(list).await?;
        let ids = self
            .chat_ids(
                "getChats",
                json!({ "@type": "getChats", "chat_list": list.to_json(), "limit": limit }),
            )
            .await?;
        Ok((rounds, ids))
    }

    /// Folders of the last `updateChatFolders`; waits `folders_wait` at most for the first
    /// one of the session. `false`: still unknown (the folders are then left out).
    pub(super) async fn chat_folders(&self) -> (Vec<ChatFolder>, bool) {
        let deadline = Instant::now() + self.config.folders_wait;
        loop {
            if let Some(folders) = lock(&self.folders).clone() {
                return (folders, true);
            }
            if Instant::now() >= deadline {
                return (Vec::new(), false);
            }
            tokio::time::sleep(FOLDERS_POLL).await;
        }
    }

    async fn basic_group(&self, basic_group_id: i64) -> Result<Value, TransportError> {
        let group = self
            .call(
                "getBasicGroup",
                json!({ "@type": "getBasicGroup", "basic_group_id": basic_group_id }),
            )
            .await?;
        j::expect(&group, "basicGroup")?;
        Ok(group)
    }

    async fn supergroup(&self, supergroup_id: i64) -> Result<Value, TransportError> {
        let group = self
            .call(
                "getSupergroup",
                json!({ "@type": "getSupergroup", "supergroup_id": supergroup_id }),
            )
            .await?;
        j::expect(&group, "supergroup")?;
        Ok(group)
    }

    pub(super) async fn user(&self, user_id: i64) -> Result<Value, TransportError> {
        let user = self
            .call("getUser", json!({ "@type": "getUser", "user_id": user_id }))
            .await?;
        j::expect(&user, "user")?;
        Ok(user)
    }

    pub(super) async fn get_chat(&self, id: i64) -> Result<Value, TransportError> {
        self.call("getChat", json!({ "@type": "getChat", "chat_id": id }))
            .await
    }

    /// The `Dialog` of a `chat` object (`listed`, `archived` and `folders` left to the
    /// caller), or `None` for what the app cannot send to: secret chats, basic groups
    /// upgraded to a supergroup, channel direct-message groups. A deleted account comes back
    /// with `deleted` (so the cache drops it). Reads only local TDLib data (`getBasicGroup`,
    /// `getSupergroup`, `getUser`).
    async fn dialog_of(
        &self,
        chat: &Value,
        me: &Me,
        tally: &mut Tally,
    ) -> Result<Option<Dialog>, TransportError> {
        chat_id(chat)?;
        let title = j::text(chat, "title")?.trim().to_owned();
        let dialog = |peer, title, kind, is_forum, can_write, username| Dialog {
            peer,
            title,
            username,
            is_forum,
            is_broadcast: kind == ChatKind::Channel,
            is_saved_messages: kind == ChatKind::Saved,
            kind,
            listed: false,
            archived: false,
            folders: Vec::new(),
            can_write,
            deleted: false,
        };
        Ok(Some(match ChatType::parse(chat)? {
            ChatType::Private { user_id } => {
                let peer = Peer {
                    kind: PeerKind::User,
                    raw_id: user_id,
                };
                if user_id == me.user_id {
                    tally.private += 1;
                    return Ok(Some(dialog(
                        peer,
                        SAVED_TITLE.to_owned(),
                        ChatKind::Saved,
                        false,
                        Some(true),
                        None,
                    )));
                }
                // TDLib gives a deleted account an empty title: those were the blank rows of
                // the picker. Nothing can be sent there (`canSendMessageToUser`: not a
                // member); flagged so the cache drops them, never a destination.
                let title = if title.is_empty() {
                    let user = self.user(user_id).await?;
                    if j::kind(&user["type"]) == "userTypeDeleted" {
                        tally.deleted_users += 1;
                        return Ok(Some(Dialog {
                            deleted: true,
                            ..dialog(
                                peer,
                                String::new(),
                                ChatKind::Private,
                                false,
                                Some(false),
                                None,
                            )
                        }));
                    }
                    tally.untitled += 1;
                    user_title(&user)
                } else {
                    title
                };
                tally.private += 1;
                dialog(peer, title, ChatKind::Private, false, None, None)
            }
            ChatType::BasicGroup { basic_group_id } => {
                let group = self.basic_group(basic_group_id).await?;
                if !j::flag(&group, "is_active")
                    || int_or_zero(&group, "upgraded_to_supergroup_id") != 0
                {
                    tally.skipped += 1;
                    return Ok(None);
                }
                tally.basic_groups += 1;
                let can_write = approx_can_write(
                    Group::Basic { active: true },
                    &group["status"],
                    &chat["permissions"],
                );
                let peer = Peer {
                    kind: PeerKind::BasicGroup,
                    raw_id: basic_group_id,
                };
                dialog(peer, title, ChatKind::BasicGroup, false, can_write, None)
            }
            ChatType::Supergroup {
                supergroup_id,
                is_channel,
            } => {
                let group = self.supergroup(supergroup_id).await?;
                // Sending there needs a direct-messages topic the app does not model.
                if j::flag(&group, "is_direct_messages_group") {
                    tally.skipped += 1;
                    return Ok(None);
                }
                let is_forum = j::boolean(&group, "is_forum")?;
                if is_channel {
                    tally.channels += 1;
                } else {
                    tally.supergroups += 1;
                }
                tally.forums += usize::from(is_forum);
                let shape = Group::Super {
                    is_channel,
                    is_broadcast_group: j::flag(&group, "is_broadcast_group"),
                    join_to_send_messages: j::flag(&group, "join_to_send_messages"),
                    paid_message_star_count: int_or_zero(&group, "paid_message_star_count"),
                };
                let can_write = approx_can_write(shape, &group["status"], &chat["permissions"]);
                let peer = Peer {
                    kind: PeerKind::Channel,
                    raw_id: supergroup_id,
                };
                let kind = if is_channel {
                    ChatKind::Channel
                } else {
                    ChatKind::Supergroup
                };
                // `usernames` may be null (`td_api.tl`).
                let username = group["usernames"]["active_usernames"][0]
                    .as_str()
                    .map(str::to_owned);
                dialog(peer, title, kind, is_forum, can_write, username)
            }
            ChatType::Secret => {
                tally.skipped += 1;
                return Ok(None);
            }
        }))
    }

    /// `list_dialogs`: the main list, the archive and every folder of `updateChatFolders`,
    /// each loaded with `loadChats` until 404 then read with `getChats`; deduplicated (a chat
    /// can be in the main list and in two folders), then `getChat` for each. "Saved Messages"
    /// (the chat of `my_id`) comes first, created if it is in no list; then the order of
    /// first appearance (main, archive, folders). A folder TDLib no longer knows (`400`) is
    /// skipped; the main list is required. Read only: nothing is archived, read or joined.
    pub(super) async fn list_dialogs(&self, limit: usize) -> Result<Vec<Dialog>, TransportError> {
        let started = Instant::now();
        let me = self.cached_me().await?;
        let (folders, folders_known) = self.chat_folders().await;
        let wanted = i32::try_from(limit.max(1)).unwrap_or(i32::MAX);
        let mut lists = vec![ChatList::Main, ChatList::Archive];
        lists.extend(folders.iter().map(|folder| ChatList::Folder(folder.id)));

        let mut order: Vec<i64> = Vec::new();
        let mut members: HashMap<i64, Membership> = HashMap::new();
        let (mut rounds, mut listed, mut lists_failed) = (0usize, 0usize, 0usize);
        let (mut main, mut archive) = (0usize, 0usize);
        let mut folder_chats: Vec<usize> = Vec::new();
        for list in lists {
            let ids = match self.list_ids(list, wanted).await {
                Ok((list_rounds, ids)) => {
                    rounds += list_rounds;
                    ids
                }
                Err(TransportError::Other { code: 400, .. }) if list != ChatList::Main => {
                    lists_failed += 1;
                    continue;
                }
                Err(error) => return Err(error),
            };
            listed += ids.len();
            match list {
                ChatList::Main => main = ids.len(),
                ChatList::Archive => archive = ids.len(),
                ChatList::Folder(_) => folder_chats.push(ids.len()),
            }
            for id in ids {
                let membership = members.entry(id).or_insert_with(|| {
                    order.push(id);
                    Membership::default()
                });
                match list {
                    ChatList::Main => {}
                    ChatList::Archive => membership.archived = true,
                    ChatList::Folder(folder_id) => {
                        if let Some(folder) = folders.iter().find(|f| f.id == folder_id)
                            && !membership.folders.iter().any(|f| f.id == folder_id)
                        {
                            membership.folders.push(folder.clone());
                        }
                    }
                }
            }
        }

        let mut tally = Tally::default();
        let mut dialogs = Vec::new();
        for id in &order {
            let chat = self.get_chat(*id).await?;
            let Some(mut dialog) = self.dialog_of(&chat, &me, &mut tally).await? else {
                continue;
            };
            if let Some(membership) = members.remove(id) {
                dialog.archived = membership.archived;
                dialog.folders = membership.folders;
            }
            dialog.listed = true;
            dialogs.push(dialog);
        }
        let mut saved_added = false;
        if !dialogs.iter().any(|d| d.is_saved_messages) {
            let peer = Peer {
                kind: PeerKind::User,
                raw_id: me.user_id,
            };
            self.create_chat(&peer).await?;
            dialogs.insert(
                0,
                Dialog {
                    peer,
                    title: SAVED_TITLE.to_owned(),
                    username: None,
                    is_forum: false,
                    is_broadcast: false,
                    is_saved_messages: true,
                    kind: ChatKind::Saved,
                    listed: true,
                    archived: false,
                    folders: Vec::new(),
                    can_write: Some(true),
                    deleted: false,
                },
            );
            saved_added = true;
        }
        // Saved Messages first, then the order of first appearance (stable sort).
        dialogs.sort_by_key(|d| !d.is_saved_messages);
        dialogs.truncate(limit);
        let folder_chats = json!(folder_chats).to_string();
        // Counts only: titles, folder names and ids are personal data.
        tracing::info!(
            event = "dialogs_loaded",
            dur_ms = crate::logging::ms_since(started),
            load_rounds = rounds,
            main,
            archive,
            folders = folders.len(),
            folders_known,
            folder_chats = folder_chats.as_str(),
            lists_failed,
            listed,
            unique = order.len(),
            dialogs = dialogs.iter().filter(|d| !d.deleted).count(),
            archived = dialogs.iter().filter(|d| d.archived && !d.deleted).count(),
            in_folders = dialogs
                .iter()
                .filter(|d| !d.folders.is_empty() && !d.deleted)
                .count(),
            private = tally.private,
            basic_groups = tally.basic_groups,
            supergroups = tally.supergroups,
            channels = tally.channels,
            forums = tally.forums,
            archived_forums = dialogs.iter().filter(|d| d.is_forum && d.archived).count(),
            skipped = tally.skipped,
            deleted_users = tally.deleted_users,
            untitled = tally.untitled,
            saved_added,
            "chat lists loaded from TDLib"
        );
        Ok(dialogs)
    }

    /// `search_chats`, read only: `searchChats` (chats TDLib knows, offline), then
    /// `searchChatsOnServer` (chats of the account) and `searchPublicChats` (public chats;
    /// TDLib leaves out those already in a list); deduplicated in that order, `getChat` for
    /// each. A failed server search keeps the local results. Nothing is joined, opened or
    /// added to the recently found chats.
    pub(super) async fn search_chats(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Dialog>, TransportError> {
        let started = Instant::now();
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let me = self.cached_me().await?;
        let wanted = i32::try_from(limit.clamp(1, SEARCH_LIMIT_MAX)).unwrap_or(1);
        let local = self
            .chat_ids(
                "searchChats",
                json!({ "@type": "searchChats", "query": query, "type_filter": null, "limit": wanted }),
            )
            .await?;
        let mut failed = 0usize;
        let mut tolerant = |result: Result<Vec<i64>, TransportError>| match result {
            Ok(ids) => Ok(ids),
            Err(TransportError::NotAuthorized) => Err(TransportError::NotAuthorized),
            Err(error) => {
                failed += 1;
                tracing::warn!(
                    event = "chats_search_failed",
                    error_code = error.code_and_params().0,
                    "a Telegram chat search failed, the other results are kept"
                );
                Ok(Vec::new())
            }
        };
        let server = tolerant(
            self.chat_ids(
                "searchChatsOnServer",
                json!({ "@type": "searchChatsOnServer", "query": query, "type_filter": null, "limit": wanted }),
            )
            .await,
        )?;
        let public = tolerant(
            self.chat_ids(
                "searchPublicChats",
                json!({ "@type": "searchPublicChats", "query": query, "type_filter": null }),
            )
            .await,
        )?;
        let mut seen = HashSet::new();
        let ids: Vec<i64> = local
            .iter()
            .chain(&server)
            .chain(&public)
            .copied()
            .filter(|id| seen.insert(*id))
            .collect();
        let mut tally = Tally::default();
        let mut dialogs = Vec::new();
        for id in &ids {
            if dialogs.len() >= limit {
                break;
            }
            let chat = self.get_chat(*id).await?;
            if let Some(dialog) = self.dialog_of(&chat, &me, &mut tally).await?
                && !dialog.deleted
            {
                dialogs.push(dialog);
            }
        }
        // The query is something the user typed: its length only.
        tracing::info!(
            event = "chats_searched",
            dur_ms = crate::logging::ms_since(started),
            query_chars = query.chars().count(),
            local = local.len(),
            server = server.len(),
            public = public.len(),
            unique = ids.len(),
            results = dialogs.len(),
            forums = tally.forums,
            skipped = tally.skipped + tally.deleted_users,
            failed,
            "chats searched in TDLib (read only)"
        );
        Ok(dialogs)
    }

    /// `list_topics`: `getForumTopics` paginated through `next_offset_*` (General is one of
    /// the topics, `is_general`).
    pub(super) async fn list_topics(&self, peer: &Peer) -> Result<Vec<Topic>, TransportError> {
        let started = Instant::now();
        let chat = self.known_chat(peer).await?;
        let chat_id = chat_id(&chat)?;
        let mut offsets = (0i64, 0i64, 0i64);
        let mut seen = HashSet::new();
        let mut topics = Vec::new();
        let mut pages = 0usize;
        while pages < MAX_TOPIC_PAGES {
            pages += 1;
            let page = self
                .call(
                    "getForumTopics",
                    json!({
                        "@type": "getForumTopics",
                        "chat_id": chat_id,
                        "query": "",
                        "offset_date": offsets.0,
                        "offset_message_id": offsets.1,
                        "offset_forum_topic_id": offsets.2,
                        "limit": TOPICS_PAGE,
                    }),
                )
                .await?;
            j::expect(&page, "forumTopics")?;
            let found = j::array(&page, "topics")?;
            for topic in found {
                let info = &topic["info"];
                let id = i32::try_from(int(info, "forum_topic_id")?)
                    .map_err(|_| j::missing(info, "forum_topic_id"))?;
                if seen.insert(id) {
                    topics.push(Topic {
                        id,
                        title: j::text(info, "name")?.to_owned(),
                        is_general: j::boolean(info, "is_general")?,
                        is_closed: j::boolean(info, "is_closed")?,
                    });
                }
            }
            let next = (
                int_or_zero(&page, "next_offset_date"),
                int_or_zero(&page, "next_offset_message_id"),
                int_or_zero(&page, "next_offset_forum_topic_id"),
            );
            let total = usize::try_from(int_or_zero(&page, "total_count")).unwrap_or(0);
            if found.is_empty() || next == (0, 0, 0) || next == offsets || topics.len() >= total {
                break;
            }
            offsets = next;
        }
        tracing::info!(
            event = "topics_loaded",
            chat = redact(chat_id).as_str(),
            dur_ms = crate::logging::ms_since(started),
            pages,
            topics = topics.len(),
            closed = topics.iter().filter(|t| t.is_closed).count(),
            general = topics
                .iter()
                .any(|t| t.is_general && t.id == GENERAL_TOPIC_ID),
            "forum topics loaded from TDLib"
        );
        Ok(topics)
    }

    /// The pre-send check on a chat made known by `known_chat`. `slow_mode`: also read the
    /// slow-mode wait (`getSupergroupFullInfo`, a network request); the upload skips it.
    pub(super) async fn check_rights(
        &self,
        chat: &Value,
        target: &Target,
        media: MediaKind,
        my_id: i64,
        slow_mode: bool,
    ) -> Result<SendCheck, TransportError> {
        let chat_id = chat_id(chat)?;
        let kind = ChatType::parse(chat)?;
        let mut status_name = "self";
        let mut slow = None;
        let verdict = match kind {
            ChatType::Private { user_id } => {
                if target.topic_id.is_some() {
                    return Err(TransportError::Protocol(
                        "a topic in a chat that is not a forum".into(),
                    ));
                }
                if user_id == my_id {
                    Verdict::Allowed
                } else {
                    status_name = "user";
                    let result = self
                        .call(
                            "canSendMessageToUser",
                            json!({ "@type": "canSendMessageToUser", "user_id": user_id, "only_local": false }),
                        )
                        .await?;
                    user_verdict(&result)?
                }
            }
            ChatType::BasicGroup { basic_group_id } => {
                if target.topic_id.is_some() {
                    return Err(TransportError::Protocol(
                        "a topic in a chat that is not a forum".into(),
                    ));
                }
                let group = self.basic_group(basic_group_id).await?;
                let status = Status::parse(&group["status"])?;
                status_name = status.as_str();
                let active = j::boolean(&group, "is_active")?
                    && int_or_zero(&group, "upgraded_to_supergroup_id") == 0;
                let perms = Perms::parse(&chat["permissions"])?;
                decide(Group::Basic { active }, status, perms, media)
            }
            ChatType::Supergroup { supergroup_id, .. } => {
                let group = self.supergroup(supergroup_id).await?;
                let status = Status::parse(&group["status"])?;
                status_name = status.as_str();
                let perms = Perms::parse(&chat["permissions"])?;
                let shape = Group::Super {
                    is_channel: j::boolean(&group, "is_channel")?,
                    is_broadcast_group: j::flag(&group, "is_broadcast_group"),
                    join_to_send_messages: j::flag(&group, "join_to_send_messages"),
                    paid_message_star_count: int_or_zero(&group, "paid_message_star_count"),
                };
                let mut verdict = decide(shape, status, perms, media);
                if verdict == Verdict::Allowed
                    && let Some(topic_id) = target.topic_id
                {
                    verdict = self.topic_verdict(chat_id, topic_id, status).await?;
                }
                if slow_mode
                    && verdict == Verdict::Allowed
                    && !status.is_privileged()
                    && j::flag(&group, "is_slow_mode_enabled")
                {
                    slow = Some(self.slow_mode(supergroup_id).await?);
                }
                verdict
            }
            ChatType::Secret => {
                return Err(TransportError::Protocol(
                    "secret chats are not supported".into(),
                ));
            }
        };
        tracing::debug!(
            event = "send_rights",
            chat = redact(chat_id).as_str(),
            chat_type = kind.as_str(),
            status = status_name,
            topic = target.topic_id.is_some(),
            media = media.as_str(),
            verdict = verdict_code(&verdict).as_str(),
            slow_mode_wait_s = slow.as_ref().map(|s: &SlowMode| s.wait_remaining.as_secs()),
            "send rights checked"
        );
        Ok(SendCheck {
            verdict,
            slow_mode: slow,
            checked_at: SystemTime::now(),
        })
    }

    /// `forumTopicInfo.is_closed`: then only the topic's creator (`is_outgoing`), the chat's
    /// creator or an administrator with `can_manage_topics` may post.
    async fn topic_verdict(
        &self,
        chat_id: i64,
        topic_id: i32,
        status: Status,
    ) -> Result<Verdict, TransportError> {
        let topic = self
            .call(
                "getForumTopic",
                json!({ "@type": "getForumTopic", "chat_id": chat_id, "forum_topic_id": topic_id }),
            )
            .await?;
        j::expect(&topic, "forumTopic")?;
        let info = &topic["info"];
        let manager = matches!(
            status,
            Status::Creator { .. }
                | Status::Admin {
                    can_manage_topics: true,
                    ..
                }
        );
        Ok(
            if j::boolean(info, "is_closed")? && !manager && !j::flag(info, "is_outgoing") {
                Verdict::Denied(Forbidden::TopicClosed)
            } else {
                Verdict::Allowed
            },
        )
    }

    /// `supergroupFullInfo.slow_mode_delay` (seconds) and `slow_mode_delay_expires_in`
    /// (seconds left, a double; TDLib does not notify its evolution).
    async fn slow_mode(&self, supergroup_id: i64) -> Result<SlowMode, TransportError> {
        let info = self
            .call(
                "getSupergroupFullInfo",
                json!({ "@type": "getSupergroupFullInfo", "supergroup_id": supergroup_id }),
            )
            .await?;
        j::expect(&info, "supergroupFullInfo")?;
        let delay = u64::try_from(int(&info, "slow_mode_delay")?).unwrap_or(0);
        let left = info["slow_mode_delay_expires_in"]
            .as_f64()
            .ok_or_else(|| j::missing(&info, "slow_mode_delay_expires_in"))?;
        let left = if left.is_finite() && left > 0.0 {
            Duration::from_secs_f64(left.min(1e9))
        } else {
            Duration::ZERO
        };
        Ok(SlowMode {
            delay: Duration::from_secs(delay),
            wait_remaining: left,
        })
    }

    /// `can_send`: chat made known, then the rights with the slow mode.
    pub(super) async fn can_send(
        &self,
        target: &Target,
        media: MediaKind,
    ) -> Result<SendCheck, TransportError> {
        let me = self.cached_me().await?;
        let chat = self.known_chat(&target.peer).await?;
        self.check_rights(&chat, target, media, me.user_id, true)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Perms = Perms {
        videos: false,
        photos: false,
        documents: false,
    };
    const NO_VIDEO: Perms = Perms {
        videos: false,
        photos: true,
        documents: true,
    };

    fn group(is_channel: bool, broadcast: bool, join: bool, paid: i64) -> Group {
        Group::Super {
            is_channel,
            is_broadcast_group: broadcast,
            join_to_send_messages: join,
            paid_message_star_count: paid,
        }
    }

    fn denied(forbidden: Forbidden) -> Verdict {
        Verdict::Denied(forbidden)
    }

    #[test]
    fn statuses_parse_from_td_api_1_8_68() {
        let restricted = json!({
            "@type": "chatMemberStatusRestricted", "is_member": true, "restricted_until_date": 0,
            "permissions": { "@type": "chatPermissions", "can_send_basic_messages": true,
                "can_send_audios": true, "can_send_documents": true, "can_send_photos": false,
                "can_send_videos": true, "can_send_video_notes": true, "can_send_voice_notes": true,
                "can_send_polls": true, "can_send_other_messages": true, "can_add_link_previews": true,
                "can_react_to_messages": true, "can_edit_tag": false, "can_change_info": false,
                "can_invite_users": false, "can_pin_messages": false, "can_create_topics": false }
        });
        assert_eq!(
            Status::parse(&restricted).unwrap(),
            Status::Restricted {
                is_member: true,
                until_unix: None,
                perms: Perms {
                    videos: true,
                    photos: false,
                    documents: true
                }
            }
        );
        let admin = json!({ "@type": "chatMemberStatusAdministrator", "can_be_edited": false,
            "rights": { "@type": "chatAdministratorRights", "can_post_messages": true,
                "can_manage_topics": false } });
        assert_eq!(
            Status::parse(&admin).unwrap(),
            Status::Admin {
                can_post_messages: true,
                can_manage_topics: false
            }
        );
        assert_eq!(
            Status::parse(&json!({ "@type": "chatMemberStatusBanned", "banned_until_date": 99 }))
                .unwrap(),
            Status::Banned {
                until_unix: Some(99)
            }
        );
        assert_eq!(
            Status::parse(&json!({ "@type": "chatMemberStatusMember", "member_until_date": 0 }))
                .unwrap(),
            Status::Member
        );
        assert!(Status::parse(&json!({ "@type": "chatMemberStatusCreator" })).is_err());
        assert!(Status::parse(&json!({ "@type": "somethingNew" })).is_err());
    }

    #[test]
    fn basic_group_rules() {
        let basic = Group::Basic { active: true };
        let video = MediaKind::Video;
        assert_eq!(
            decide(basic, Status::Member, Perms::ALL, video),
            Verdict::Allowed
        );
        assert_eq!(
            decide(basic, Status::Member, NO_VIDEO, video),
            denied(Forbidden::MediaNotAllowed(video))
        );
        assert_eq!(
            decide(basic, Status::Member, NO_VIDEO, MediaKind::Photo),
            Verdict::Allowed
        );
        // Administrators ignore the default permissions.
        let admin = Status::Admin {
            can_post_messages: false,
            can_manage_topics: false,
        };
        assert_eq!(decide(basic, admin, NONE, video), Verdict::Allowed);
        assert_eq!(
            decide(basic, Status::Left, Perms::ALL, video),
            denied(Forbidden::NotMember)
        );
        assert_eq!(
            decide(
                Group::Basic { active: false },
                Status::Member,
                Perms::ALL,
                video
            ),
            denied(Forbidden::NotMember)
        );
        assert_eq!(
            decide(
                basic,
                Status::Banned { until_unix: None },
                Perms::ALL,
                video
            ),
            denied(Forbidden::Banned { until_unix: None })
        );
    }

    #[test]
    fn supergroup_and_channel_rules() {
        let video = MediaKind::Video;
        let plain = group(false, false, true, 0);
        assert_eq!(
            decide(plain, Status::Member, Perms::ALL, video),
            Verdict::Allowed
        );
        assert_eq!(
            decide(plain, Status::Member, NO_VIDEO, video),
            denied(Forbidden::MediaNotAllowed(video))
        );
        assert_eq!(
            decide(plain, Status::Left, Perms::ALL, video),
            denied(Forbidden::JoinRequired)
        );
        // A discussion group open to non-members.
        assert_eq!(
            decide(
                group(false, false, false, 0),
                Status::Left,
                Perms::ALL,
                video
            ),
            Verdict::Allowed
        );
        let restricted = Status::Restricted {
            is_member: true,
            until_unix: Some(5),
            perms: NO_VIDEO,
        };
        assert_eq!(
            decide(plain, restricted, Perms::ALL, video),
            denied(Forbidden::Restricted {
                until_unix: Some(5)
            })
        );
        assert_eq!(
            decide(plain, restricted, NONE, MediaKind::Document),
            Verdict::Allowed,
            "individual permissions replace the defaults"
        );
        // Paid messages: members pay, administrators do not.
        let paid = group(false, false, true, 25);
        assert_eq!(
            decide(paid, Status::Member, Perms::ALL, video),
            denied(Forbidden::PaidMessages { stars: 25 })
        );
        let admin = Status::Admin {
            can_post_messages: false,
            can_manage_topics: false,
        };
        assert_eq!(decide(paid, admin, Perms::ALL, video), Verdict::Allowed);
        // Channels: administrators with can_post_messages only.
        let channel = group(true, false, true, 0);
        assert_eq!(
            decide(channel, Status::Member, Perms::ALL, video),
            denied(Forbidden::AdminsOnly)
        );
        assert_eq!(
            decide(channel, admin, Perms::ALL, video),
            denied(Forbidden::AdminsOnly)
        );
        let poster = Status::Admin {
            can_post_messages: true,
            can_manage_topics: false,
        };
        assert_eq!(decide(channel, poster, NONE, video), Verdict::Allowed);
        assert_eq!(
            decide(channel, Status::Creator { is_member: true }, NONE, video),
            Verdict::Allowed
        );
        assert_eq!(
            decide(channel, Status::Left, Perms::ALL, video),
            denied(Forbidden::AdminsOnly)
        );
        // Broadcast group: administrators only, but any administrator.
        let broadcast = group(false, true, true, 0);
        assert_eq!(
            decide(broadcast, Status::Member, Perms::ALL, video),
            denied(Forbidden::AdminsOnly)
        );
        assert_eq!(decide(broadcast, admin, NONE, video), Verdict::Allowed);
        assert_eq!(
            decide(
                plain,
                Status::Banned {
                    until_unix: Some(7)
                },
                Perms::ALL,
                video
            ),
            denied(Forbidden::Banned {
                until_unix: Some(7)
            })
        );
    }

    #[test]
    fn private_chat_results() {
        assert_eq!(
            user_verdict(&json!({ "@type": "canSendMessageToUserResultOk" })).unwrap(),
            Verdict::Allowed
        );
        assert_eq!(
            user_verdict(
                &json!({ "@type": "canSendMessageToUserResultUserHasPaidMessages",
                "outgoing_paid_message_star_count": 50 })
            )
            .unwrap(),
            denied(Forbidden::PaidMessages { stars: 50 })
        );
        assert_eq!(
            user_verdict(&json!({ "@type": "canSendMessageToUserResultUserRestrictsNewChats" }))
                .unwrap(),
            denied(Forbidden::Restricted { until_unix: None })
        );
        assert!(user_verdict(&json!({ "@type": "ok" })).is_err());
    }

    #[test]
    fn chat_types_parse() {
        let chat = |kind: Value| json!({ "@type": "chat", "id": 1, "type": kind });
        assert_eq!(
            ChatType::parse(&chat(
                json!({ "@type": "chatTypeSupergroup", "supergroup_id": 5, "is_channel": true })
            ))
            .unwrap(),
            ChatType::Supergroup {
                supergroup_id: 5,
                is_channel: true
            }
        );
        assert_eq!(
            ChatType::parse(&chat(
                json!({ "@type": "chatTypeSecret", "secret_chat_id": 1, "user_id": 2 })
            ))
            .unwrap(),
            ChatType::Secret
        );
        assert!(ChatType::parse(&chat(json!({ "@type": "chatTypePrivate" }))).is_err());
        assert!(chat_id(&json!({ "@type": "ok" })).is_err());
    }
}
