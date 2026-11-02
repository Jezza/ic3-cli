//! What `ic3` prints: its own JSON contract, independent of the library's
//! types so library changes don't break consumers.

use ic3_teams::{AttachmentKind, ConversationId, Event, Message, Mri, SendReceipt, Timestamp};
use serde::Serialize;

/// RFC 3339 in UTC with milliseconds, e.g. `2026-11-02T09:14:03.123Z`.
pub fn iso(ms: Timestamp) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}

/// One line of `ic3 listen`.
#[derive(Debug, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum EventLine {
    Message(MessageJson),
    Edit(MessageJson),
    Delete { conversation: String, id: String },
}

#[derive(Debug, Serialize, PartialEq)]
pub struct MessageJson {
    pub conversation: String,
    pub id: String,
    pub client_message_id: Option<String>,
    pub thread_root: Option<String>,
    pub from: String,
    pub from_name: Option<String>,
    #[serde(rename = "self")]
    pub is_self: bool,
    pub arrived_at: String,
    pub text: String,
    pub reply_to: Option<ReplyJson>,
    pub attachments: Vec<AttachmentJson>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct ReplyJson {
    pub id: String,
    pub from_name: Option<String>,
    pub text: String,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct AttachmentJson {
    pub name: String,
    pub kind: &'static str,
}

impl MessageJson {
    pub fn from_message(m: &Message, me: &Mri) -> MessageJson {
        MessageJson {
            conversation: m.conversation.0.clone(),
            id: m.id.0.clone(),
            client_message_id: m.client_message_id.clone(),
            thread_root: m.thread_root.as_ref().map(|t| t.0.clone()),
            from: m.from.0.clone(),
            from_name: m.from_name.clone(),
            is_self: &m.from == me,
            arrived_at: iso(m.arrived_at),
            text: m.body.plain_text(),
            reply_to: m.reply_to.as_ref().map(|q| ReplyJson {
                id: q.message_id.0.clone(),
                from_name: q.author_name.clone(),
                text: q.excerpt.plain_text(),
            }),
            attachments: m
                .attachments
                .iter()
                .map(|a| AttachmentJson {
                    name: a.name.clone(),
                    kind: match a.kind {
                        AttachmentKind::File => "file",
                        AttachmentKind::Image => "image",
                        AttachmentKind::Other => "other",
                    },
                })
                .collect(),
        }
    }
}

impl EventLine {
    /// The line for `ev`, when it is a new, edited or deleted message.
    pub fn from_event(ev: &Event, me: &Mri) -> Option<EventLine> {
        match ev {
            Event::NewMessage(m) => Some(EventLine::Message(MessageJson::from_message(m, me))),
            Event::MessageEdited(m) => Some(EventLine::Edit(MessageJson::from_message(m, me))),
            Event::MessageDeleted {
                conversation,
                message,
                ..
            } => Some(EventLine::Delete {
                conversation: conversation.0.clone(),
                id: message.0.clone(),
            }),
            _ => None,
        }
    }

    pub fn conversation(&self) -> &str {
        match self {
            EventLine::Message(m) | EventLine::Edit(m) => &m.conversation,
            EventLine::Delete { conversation, .. } => conversation,
        }
    }
}

/// `send --json`.
#[derive(Debug, Serialize, PartialEq)]
pub struct ReceiptJson {
    pub conversation: String,
    pub client_message_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arrived_at: Option<String>,
}

impl ReceiptJson {
    pub fn new(conv: &ConversationId, r: &SendReceipt) -> ReceiptJson {
        ReceiptJson {
            conversation: conv.0.clone(),
            client_message_id: r.client_message_id.clone(),
            arrived_at: r.arrival_time.map(iso),
        }
    }
}

/// One conversation of `ic3 list`.
#[derive(Debug, Serialize, PartialEq, Clone)]
pub struct ListRow {
    pub id: String,
    pub kind: Kind,
    pub name: String,
    pub hidden: bool,
    pub last_active: Option<String>,
    #[serde(skip)]
    pub last_active_ms: Option<Timestamp>,
}

#[derive(Debug, Serialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Chat,
    Group,
    Channel,
    Notes,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Chat => "chat",
            Kind::Group => "group",
            Kind::Channel => "channel",
            Kind::Notes => "notes",
        }
    }
}

impl ListRow {
    /// `id\tkind\tname`, with tabs and line breaks in the name made spaces.
    pub fn tsv(&self) -> String {
        let name: String = self
            .name
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        format!("{}\t{}\t{}", self.id, self.kind.as_str(), name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic3_teams::{MessageId, RichText};
    use serde_json::json;

    fn message(from: &str) -> Message {
        Message {
            id: MessageId("1730538843000".into()),
            client_message_id: Some("cm-1".into()),
            conversation: ConversationId("19:abc@thread.v2".into()),
            thread_root: None,
            from: Mri(from.into()),
            from_name: Some("Jane Smith".into()),
            arrived_at: 1_762_074_843_123,
            version: 1,
            edited: false,
            deleted: false,
            body: RichText::text("hello there"),
            reply_to: None,
            reactions: Vec::new(),
            attachments: Vec::new(),
            message_type: "RichText/Html".into(),
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn iso_is_utc_with_millis() {
        assert_eq!(iso(1_762_074_843_123), "2025-11-02T09:14:03.123Z");
    }

    #[test]
    fn new_message_line() {
        let me = Mri("8:orgid:me".into());
        let line = EventLine::from_event(&Event::NewMessage(message("8:orgid:jane")), &me).unwrap();
        assert_eq!(
            serde_json::to_value(&line).unwrap(),
            json!({
                "type": "message",
                "conversation": "19:abc@thread.v2",
                "id": "1730538843000",
                "client_message_id": "cm-1",
                "thread_root": null,
                "from": "8:orgid:jane",
                "from_name": "Jane Smith",
                "self": false,
                "arrived_at": "2025-11-02T09:14:03.123Z",
                "text": "hello there",
                "reply_to": null,
                "attachments": [],
            })
        );
    }

    #[test]
    fn own_message_is_self_and_edit_is_typed() {
        let me = Mri("8:orgid:me".into());
        let line =
            EventLine::from_event(&Event::MessageEdited(message("8:orgid:me")), &me).unwrap();
        let v = serde_json::to_value(&line).unwrap();
        assert_eq!(v["type"], "edit");
        assert_eq!(v["self"], true);
    }

    #[test]
    fn delete_line() {
        let me = Mri("8:orgid:me".into());
        let ev = Event::MessageDeleted {
            conversation: ConversationId("19:abc@thread.v2".into()),
            message: MessageId("42".into()),
            version: 3,
        };
        let line = EventLine::from_event(&ev, &me).unwrap();
        assert_eq!(line.conversation(), "19:abc@thread.v2");
        assert_eq!(
            serde_json::to_value(&line).unwrap(),
            json!({"type": "delete", "conversation": "19:abc@thread.v2", "id": "42"})
        );
    }

    #[test]
    fn other_events_are_not_printed() {
        let me = Mri("8:orgid:me".into());
        assert_eq!(EventLine::from_event(&Event::Connected, &me), None);
        assert_eq!(EventLine::from_event(&Event::ReauthRequired, &me), None);
    }

    #[test]
    fn receipt_omits_a_missing_arrival() {
        let conv = ConversationId("48:notes".into());
        let r = SendReceipt {
            client_message_id: "cm".into(),
            arrival_time: None,
        };
        assert_eq!(
            serde_json::to_value(ReceiptJson::new(&conv, &r)).unwrap(),
            json!({"conversation": "48:notes", "client_message_id": "cm"})
        );
    }

    #[test]
    fn tsv_flattens_control_characters() {
        let row = ListRow {
            id: "19:x".into(),
            kind: Kind::Group,
            name: "Launch\tplans\nQ4".into(),
            hidden: false,
            last_active: None,
            last_active_ms: None,
        };
        assert_eq!(row.tsv(), "19:x\tgroup\tLaunch plans Q4");
    }
}
