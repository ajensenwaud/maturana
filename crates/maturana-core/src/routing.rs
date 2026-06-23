//! Multi-agent routing: decide which agent should handle an inbound message, by
//! channel, sender, and/or content — so one front-end can fan out to several
//! isolated agents (a personal-assistant fleet behind one inbox). A route only
//! decides WHICH agent's front door (`enqueue_turn`) an inbound goes to — the
//! agents stay isolated in their own VMs, secrets stay host-side, nothing about
//! isolation changes. It is a dispatch table, not a new trust boundary.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::state::MaturanaHome;

/// One routing rule. A rule matches an inbound message when ALL of its set
/// conditions match (an unset condition is a wildcard). More specific rules win.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct Route {
    /// Match only this channel (telegram/discord/slack/agentmail/…); unset = any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// Match only this sender / peer / chat id; unset = any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<String>,
    /// Match only if the message contains this (case-insensitive) text; unset = any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,
    /// The agent a matching message is routed to.
    pub agent: String,
}

impl Route {
    fn matches(&self, channel: &str, sender: &str, text: &str) -> bool {
        self.channel
            .as_deref()
            .map_or(true, |c| c.eq_ignore_ascii_case(channel))
            && self.sender.as_deref().map_or(true, |s| s == sender)
            && self
                .contains
                .as_deref()
                .map_or(true, |k| text.to_lowercase().contains(&k.to_lowercase()))
    }

    /// How many conditions the rule pins — used to prefer the most specific match.
    fn specificity(&self) -> u8 {
        self.channel.is_some() as u8 + self.sender.is_some() as u8 + self.contains.is_some() as u8
    }

    /// A human description, e.g. "telegram from 123 containing 'invoice'".
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(c) = &self.channel {
            parts.push(format!("channel={c}"));
        }
        if let Some(s) = &self.sender {
            parts.push(format!("from={s}"));
        }
        if let Some(k) = &self.contains {
            parts.push(format!("contains='{k}'"));
        }
        if parts.is_empty() {
            "any".to_string()
        } else {
            parts.join(" ")
        }
    }
}

/// The routing table: an ordered set of rules plus an optional default agent for
/// anything that matches no rule.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RoutingTable {
    /// Where to send a message that matches no rule. `None` = drop (no route).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default)]
    pub routes: Vec<Route>,
}

impl RoutingTable {
    pub fn path(home: &MaturanaHome) -> PathBuf {
        home.root().join("routing.json")
    }

    pub fn load(home: &MaturanaHome) -> anyhow::Result<Self> {
        let path = Self::path(home);
        if !path.exists() {
            return Ok(Self::default());
        }
        Ok(serde_json::from_str(&std::fs::read_to_string(&path)?)?)
    }

    pub fn save(&self, home: &MaturanaHome) -> anyhow::Result<()> {
        let path = Self::path(home);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Resolve the target agent for an inbound message: the MOST SPECIFIC matching
    /// rule wins (ties broken by order); if nothing matches, the default agent (if
    /// any). `None` means "no route" — the caller drops or ignores the message.
    pub fn resolve(&self, channel: &str, sender: &str, text: &str) -> Option<&str> {
        self.routes
            .iter()
            .filter(|r| r.matches(channel, sender, text))
            .max_by_key(|r| r.specificity())
            .map(|r| r.agent.as_str())
            .or(self.default.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(channel: Option<&str>, sender: Option<&str>, contains: Option<&str>, agent: &str) -> Route {
        Route {
            channel: channel.map(str::to_string),
            sender: sender.map(str::to_string),
            contains: contains.map(str::to_string),
            agent: agent.to_string(),
        }
    }

    #[test]
    fn most_specific_rule_wins_then_default() {
        let table = RoutingTable {
            default: Some("general".to_string()),
            routes: vec![
                route(Some("telegram"), None, None, "tg-agent"),
                route(Some("telegram"), Some("42"), None, "vip-agent"),
                route(None, None, Some("invoice"), "billing-agent"),
            ],
        };
        // sender 42 on telegram → the 2-condition rule beats the channel-only one.
        assert_eq!(table.resolve("telegram", "42", "hi"), Some("vip-agent"));
        // other telegram sender → channel-only rule.
        assert_eq!(table.resolve("telegram", "7", "hi"), Some("tg-agent"));
        // discord with 'invoice' → the contains rule.
        assert_eq!(table.resolve("discord", "9", "your INVOICE is ready"), Some("billing-agent"));
        // nothing matches → default.
        assert_eq!(table.resolve("discord", "9", "hello"), Some("general"));
    }

    #[test]
    fn no_match_and_no_default_is_none() {
        let table = RoutingTable {
            default: None,
            routes: vec![route(Some("slack"), None, None, "slack-agent")],
        };
        assert_eq!(table.resolve("telegram", "1", "hi"), None);
        assert_eq!(table.resolve("slack", "1", "hi"), Some("slack-agent"));
    }

    #[test]
    fn contains_is_case_insensitive_and_channel_ignores_case() {
        let table = RoutingTable {
            default: None,
            routes: vec![route(Some("Telegram"), None, Some("Urgent"), "oncall")],
        };
        assert_eq!(table.resolve("telegram", "x", "this is URGENT"), Some("oncall"));
        assert_eq!(table.resolve("telegram", "x", "calm"), None);
    }

    #[test]
    fn table_round_trips_through_json() {
        let table = RoutingTable {
            default: Some("g".to_string()),
            routes: vec![route(Some("telegram"), Some("42"), None, "vip")],
        };
        let json = serde_json::to_string(&table).unwrap();
        let back: RoutingTable = serde_json::from_str(&json).unwrap();
        assert_eq!(back.default.as_deref(), Some("g"));
        assert_eq!(back.routes.len(), 1);
        assert_eq!(back.routes[0].agent, "vip");
    }
}
