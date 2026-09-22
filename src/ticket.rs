use anyhow::Context;
use std::fmt;
use std::str::FromStr;

/// A ticket ID like AGT-123. Wraps the number so it can't
/// be confused with any other u32 in the program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TicketId(pub u32);

impl fmt::Display for TicketId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AGT-{}", self.0)
    }
}

impl FromStr for TicketId {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let num = s
            .strip_prefix("AGT-")
            .ok_or_else(|| anyhow::anyhow!("ticket id must start with AGT-: {s}"))?;
        let n: u32 = num.parse()?;
        Ok(TicketId(n))
    }
}

/// How urgent a ticket is. Variants are declared lowest first,
/// so the derived ordering gives Low < Medium < High.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Low,
    Medium,
    High,
}

impl fmt::Display for Priority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s: &str = match self {
            Priority::Low => "low",
            Priority::Medium => "medium",
            Priority::High => "high",
        };
        write!(f, "{s}")
    }
}

impl FromStr for Priority {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "low" => Ok(Priority::Low),
            "medium" => Ok(Priority::Medium),
            "high" => Ok(Priority::High),
            other => anyhow::bail!("unknown priority: {other}"),
        }
    }
}

/// Where a ticket is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Triage,
    InProgress,
    Done,
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            State::Triage => "triage",
            State::InProgress => "in-progress",
            State::Done => "done",
        };
        write!(f, "{s}")
    }
}

impl FromStr for State {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "triage" => Ok(State::Triage),
            "in-progress" => Ok(State::InProgress),
            "done" => Ok(State::Done),
            other => anyhow::bail!("unknown state: {other}"),
        }
    }
}

/// One ticket. Dates and blockers arrive with storage.
#[derive(Debug, Clone)]
pub struct Ticket {
    pub id: TicketId,
    pub title: String,
    pub state: State,
    pub priority: Priority,
    pub project: Option<String>,
}

/// The text between the opening and closing `---` lines.
fn frontmatter(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("---\n")?;
    let (fm, _body) = rest.split_once("\n---")?;
    Some(fm)
}

/// The value of one `key: value` line, if the key is present.
fn field<'a>(fm: &'a str, key: &str) -> Option<&'a str> {
    for line in fm.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        if k == key {
            return Some(v.trim());
        }
    }
    None
}

/// Like `field`, but a missing key is an error that names the key.
fn required<'a>(fm: &'a str, key: &str) -> anyhow::Result<&'a str> {
    field(fm, key).with_context(|| format!("missing field: {key}"))
}

impl Ticket {
    /// Build a ticket from the full text of a vault ticket file.
    pub fn parse(text: &str) -> anyhow::Result<Ticket> {
        let fm = frontmatter(text).context("no frontmatter block")?;
        let id: TicketId = required(fm, "id")?.parse()?;
        let state: State = required(fm, "state")?.parse()?;
        let priority: Priority = required(fm, "priority")?.parse()?;
        let title = required(fm, "title")?.trim_matches('"').to_string();
        let project = field(fm, "project")
            .filter(|p| !p.is_empty())
            .map(String::from);
        Ok(Ticket {
            id,
            title,
            state,
            priority,
            project,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"---
id: AGT-7
title: "Parse tickets: the real ones"
state: in-progress
priority: high
project:
---

## Problem Statement
"#;

    #[test]
    fn frontmatter_is_the_fenced_block() {
        let fm = frontmatter(SAMPLE).unwrap();
        assert!(fm.starts_with("id: AGT-7"));
        assert!(fm.ends_with("project:"));
        assert!(frontmatter("no fences here").is_none());
    }

    #[test]
    fn parses_a_whole_ticket() {
        let t = Ticket::parse(SAMPLE).unwrap();
        assert_eq!(t.id, TicketId(7));
        assert_eq!(t.title, "Parse tickets: the real ones");
        assert_eq!(t.state, State::InProgress);
        assert_eq!(t.priority, Priority::High);
        assert_eq!(t.project, None);
        assert!(Ticket::parse("---\nid: AGT-1\n---\n").is_err());
    }

    #[test]
    fn id_round_trips() {
        let id: TicketId = "AGT-42".parse().unwrap();
        assert_eq!(id, TicketId(42));
        assert_eq!(id.to_string(), "AGT-42");
    }

    #[test]
    fn id_rejects_bad_prefix() {
        assert!("BUG-42".parse::<TicketId>().is_err());
    }

    #[test]
    fn state_round_trips() {
        for s in [State::Triage, State::InProgress, State::Done] {
            assert_eq!(s.to_string().parse::<State>().unwrap(), s);
        }
        assert!("closed".parse::<State>().is_err());
    }

    #[test]
    fn priority_round_trips() {
        for p in [Priority::Low, Priority::Medium, Priority::High] {
            assert_eq!(p.to_string().parse::<Priority>().unwrap(), p);
        }
        assert!("ultra".parse::<Priority>().is_err());
    }

    #[test]
    fn priority_orders_by_urgency() {
        assert!(Priority::Low < Priority::Medium);
        assert!(Priority::Medium < Priority::High);
    }
}
