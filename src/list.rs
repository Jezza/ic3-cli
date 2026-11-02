//! `ic3 list`: every chat and channel the account can post to.
//!
//! Sources: CSA's user state (every team channel and every chat, with
//! members) and the chat service's conversation listing (recent activity,
//! Notes, and any chat CSA left out). Names follow ic3-irc: `Team/Channel`
//! for a channel, the topic or members for a group chat, the other person
//! for a 1:1, `Notes` for notes. Member names come from one batched
//! `user_names` call rather than a `thread()` per chat.

use std::collections::{HashMap, HashSet};

use ic3_teams::{
    ChatKind, Client, Conversation, ConversationKind, Mri, Timestamp, UserState, is_notes,
};

use crate::output::{Kind, ListRow, iso};

/// Most conversation pages fetched.
const MAX_PAGES: usize = 50;
/// Most MRIs per `user_names` call.
const NAMES_PER_CALL: usize = 900;
/// Most member names in an untitled group chat's name.
const NAMES_IN_LABEL: usize = 3;

pub async fn fetch(client: &Client) -> anyhow::Result<Vec<ListRow>> {
    let me = client.me().await?.mri;
    let state = match client.user_state().await {
        Ok((state, _)) => Some(state),
        Err(e @ ic3_teams::Error::ReauthRequired) => return Err(e.into()),
        Err(e) => {
            tracing::warn!(error = %e, "cannot read teams and channels; listing recent chats only");
            None
        }
    };
    let mut convs = Vec::new();
    let mut cursor = None;
    for _ in 0..MAX_PAGES {
        let page = client.api().conversations(None, cursor).await?;
        convs.extend(page.items);
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }

    let wanted = unnamed_members(state.as_ref(), &convs, &me);
    let mut names = HashMap::new();
    for chunk in wanted.chunks(NAMES_PER_CALL) {
        match client.api().user_names(chunk).await {
            Ok(found) => names.extend(found),
            Err(e @ ic3_teams::Error::ReauthRequired) => return Err(e.into()),
            Err(e) => tracing::warn!(error = %e, "cannot look up member names"),
        }
    }
    Ok(rows(state.as_ref(), &convs, &me, &names))
}

/// Members (not `me`) of every chat whose name is not otherwise known.
fn unnamed_members(state: Option<&UserState>, convs: &[Conversation], me: &Mri) -> Vec<Mri> {
    let mut seen = HashSet::new();
    let csa = state
        .into_iter()
        .flat_map(|s| &s.chats)
        .filter(|c| c.title.is_none())
        .flat_map(|c| &c.members)
        .filter(|m| m.friendly_name.is_none())
        .map(|m| &m.mri);
    let listed = convs
        .iter()
        .filter(|c| c.topic.is_none())
        .flat_map(|c| c.members.iter().flatten())
        .filter(|m| m.friendly_name.is_none())
        .map(|m| &m.mri);
    csa.chain(listed)
        .filter(|m| *m != me && seen.insert((*m).clone()))
        .cloned()
        .collect()
}

/// The display name of `mri`, if any source knows it.
fn name_of<'a>(
    mri: &Mri,
    friendly: Option<&'a str>,
    names: &'a HashMap<Mri, String>,
) -> Option<&'a str> {
    friendly
        .filter(|n| !n.is_empty())
        .or_else(|| names.get(mri).map(String::as_str))
}

fn join_names<'a>(names: impl Iterator<Item = &'a str>) -> Option<String> {
    let v: Vec<&str> = names.take(NAMES_IN_LABEL).collect();
    (!v.is_empty()).then(|| v.join(", "))
}

/// Builds the rows, most recently active first.
pub fn rows(
    state: Option<&UserState>,
    convs: &[Conversation],
    me: &Mri,
    names: &HashMap<Mri, String>,
) -> Vec<ListRow> {
    let mut out: Vec<ListRow> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let push = |row: ListRow, out: &mut Vec<ListRow>, index: &mut HashMap<String, usize>| {
        index.insert(row.id.clone(), out.len());
        out.push(row);
    };

    if let Some(state) = state {
        for team in &state.teams {
            for ch in &team.channels {
                push(
                    ListRow {
                        id: ch.id.0.clone(),
                        kind: Kind::Channel,
                        name: format!("{}/{}", team.name, ch.name),
                        hidden: ch.hidden() || ch.archived || team.archived,
                        last_active: None,
                        last_active_ms: None,
                    },
                    &mut out,
                    &mut index,
                );
            }
        }
        for chat in &state.chats {
            let others = chat
                .members
                .iter()
                .filter(|m| &m.mri != me)
                .filter_map(|m| name_of(&m.mri, m.friendly_name.as_deref(), names));
            let (kind, name) = match chat.kind {
                ChatKind::OneOnOne => (Kind::Chat, join_names(others)),
                ChatKind::Group | ChatKind::Meeting => (
                    Kind::Group,
                    chat.title.clone().or_else(|| join_names(others)),
                ),
            };
            push(
                ListRow {
                    id: chat.id.0.clone(),
                    kind,
                    name: name.unwrap_or_default(),
                    hidden: chat.hidden,
                    last_active: None,
                    last_active_ms: chat.last_message_at,
                },
                &mut out,
                &mut index,
            );
        }
    }

    for c in convs {
        let active = c.last_message.as_ref().and_then(|m| m.id.as_i64());
        if let Some(&i) = index.get(&c.id.0) {
            let row = &mut out[i];
            row.last_active_ms = row.last_active_ms.max(active);
            if row.name.is_empty() {
                row.name = conversation_name(c, me, names).unwrap_or_default();
            }
            continue;
        }
        let kind = match c.kind {
            ConversationKind::System if is_notes(&c.id) => Kind::Notes,
            ConversationKind::System => continue,
            ConversationKind::Channel => Kind::Channel,
            ConversationKind::OneToOne => Kind::Chat,
            ConversationKind::Group => Kind::Group,
        };
        let name = if kind == Kind::Notes {
            Some("Notes".to_owned())
        } else {
            conversation_name(c, me, names)
        };
        push(
            ListRow {
                id: c.id.0.clone(),
                kind,
                name: name.unwrap_or_default(),
                hidden: c.hidden,
                last_active: None,
                last_active_ms: active,
            },
            &mut out,
            &mut index,
        );
    }

    for row in &mut out {
        if row.name.is_empty() {
            row.name.clone_from(&row.id);
        }
        row.last_active = row.last_active_ms.map(iso);
    }
    // Most recent first; never-active last, keeping their order.
    out.sort_by_key(|r| std::cmp::Reverse(r.last_active_ms.unwrap_or(Timestamp::MIN)));
    out
}

/// A name for a chat known only from the chat service's listing.
fn conversation_name(c: &Conversation, me: &Mri, names: &HashMap<Mri, String>) -> Option<String> {
    if c.kind == ConversationKind::Group
        && let Some(t) = c.topic.as_ref().filter(|t| !t.is_empty())
    {
        return Some(t.clone());
    }
    let from_members = c.members.as_ref().and_then(|ms| {
        join_names(
            ms.iter()
                .filter(|m| &m.mri != me)
                .filter_map(|m| name_of(&m.mri, m.friendly_name.as_deref(), names)),
        )
    });
    from_members.or_else(|| {
        // A 1:1's last message from the other person names them.
        let last = c.last_message.as_ref()?;
        (c.kind == ConversationKind::OneToOne && &last.from != me)
            .then(|| last.from_name.clone())
            .flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic3_teams::{
        Channel, ChatMember, ChatSummary, ConversationId, Member, MessageId, MessageSummary, Team,
    };

    fn me() -> Mri {
        Mri("8:orgid:me".into())
    }

    fn channel(id: &str, name: &str) -> Channel {
        Channel {
            id: ConversationId(id.into()),
            name: name.into(),
            channel_type: None,
            general: false,
            favorite: true,
            pinned: false,
            muted: false,
            archived: false,
            interest: None,
            member_role: Some(1),
        }
    }

    fn member(mri: &str, name: Option<&str>) -> ChatMember {
        ChatMember {
            mri: Mri(mri.into()),
            role: None,
            friendly_name: name.map(Into::into),
        }
    }

    fn chat(
        id: &str,
        kind: ChatKind,
        title: Option<&str>,
        members: Vec<ChatMember>,
        at: Option<i64>,
    ) -> ChatSummary {
        ChatSummary {
            id: ConversationId(id.into()),
            title: title.map(Into::into),
            kind,
            hidden: false,
            muted: false,
            members,
            last_message_at: at,
        }
    }

    fn conv(
        id: &str,
        kind: ConversationKind,
        last: Option<(&str, &str, Option<&str>)>,
    ) -> Conversation {
        Conversation {
            id: ConversationId(id.into()),
            kind,
            topic: None,
            members: None,
            last_message: last.map(|(mid, from, from_name)| MessageSummary {
                id: MessageId(mid.into()),
                from: Mri(from.into()),
                from_name: from_name.map(Into::into),
                message_type: "RichText/Html".into(),
            }),
            read_horizon: None,
            version: 0,
            hidden: false,
            extra: serde_json::Map::new(),
        }
    }

    fn state() -> UserState {
        UserState {
            folders: Vec::new(),
            folder_order: Vec::new(),
            chats: vec![
                chat(
                    "19:group@thread.v2",
                    ChatKind::Group,
                    Some("Launch plans"),
                    vec![],
                    Some(300),
                ),
                chat(
                    "19:untitled@thread.v2",
                    ChatKind::Group,
                    None,
                    vec![
                        member("8:orgid:me", None),
                        member("8:orgid:a", Some("Ann")),
                        member("8:orgid:b", None),
                    ],
                    Some(100),
                ),
                chat(
                    "19:one@unq.gbl.spaces",
                    ChatKind::OneOnOne,
                    None,
                    vec![member("8:orgid:me", None), member("8:orgid:b", None)],
                    Some(200),
                ),
            ],
            teams: vec![Team {
                id: ConversationId("19:general@thread.tacv2".into()),
                name: "Engineering".into(),
                favorite: false,
                collapsed: false,
                archived: false,
                channels: vec![channel("19:general@thread.tacv2", "General")],
            }],
        }
    }

    #[test]
    fn names_kinds_and_order() {
        let names = HashMap::from([(Mri("8:orgid:b".into()), "Bob".to_owned())]);
        let convs = vec![
            conv(
                "48:notes",
                ConversationKind::System,
                Some(("400", "8:orgid:me", None)),
            ),
            conv("48:calllogs", ConversationKind::System, None),
            conv(
                "19:general@thread.tacv2",
                ConversationKind::Channel,
                Some(("250", "8:orgid:a", None)),
            ),
        ];
        let rows = rows(Some(&state()), &convs, &me(), &names);
        let got: Vec<(&str, Kind, &str)> = rows
            .iter()
            .map(|r| (r.id.as_str(), r.kind, r.name.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("48:notes", Kind::Notes, "Notes"),
                ("19:group@thread.v2", Kind::Group, "Launch plans"),
                (
                    "19:general@thread.tacv2",
                    Kind::Channel,
                    "Engineering/General"
                ),
                ("19:one@unq.gbl.spaces", Kind::Chat, "Bob"),
                ("19:untitled@thread.v2", Kind::Group, "Ann, Bob"),
            ]
        );
        assert_eq!(
            rows[0].last_active.as_deref(),
            Some("1970-01-01T00:00:00.400Z")
        );
    }

    #[test]
    fn unnamed_members_skips_me_named_and_duplicates() {
        let got = unnamed_members(Some(&state()), &[], &me());
        assert_eq!(got, vec![Mri("8:orgid:b".into())]);
    }

    #[test]
    fn listing_only_chats_are_named_from_members_or_last_message() {
        let mut group = conv("19:g@thread.v2", ConversationKind::Group, None);
        group.members = Some(vec![Member {
            mri: Mri("8:orgid:c".into()),
            role: None,
            friendly_name: Some("Cat".into()),
        }]);
        let one = conv(
            "19:o@unq.gbl.spaces",
            ConversationKind::OneToOne,
            Some(("5", "8:orgid:d", Some("Dan"))),
        );
        let nameless = conv("19:n@thread.v2", ConversationKind::Group, None);
        let rows = rows(None, &[group, one, nameless], &me(), &HashMap::new());
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["Dan", "Cat", "19:n@thread.v2"]);
    }
}
