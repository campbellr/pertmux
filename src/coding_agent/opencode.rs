use super::CodingAgent;
use crate::types::{AgentPane, MessageSummary, PaneStatus, SessionDetail};
use anyhow::Context;
use base64::Engine;
use jiff::Timestamp;
use serde::Deserialize;
use serde::de::{DeserializeOwned, IgnoredAny};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(1);
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
/// Shorter than the default 2s refresh interval.
const ACTIVE_TTL: Duration = Duration::from_secs(1);

/// Talks to the opencode background service that every opencode TUI on the
/// machine connects to.
pub struct OpenCode {
    /// Reusable HTTP agent for status queries (short timeout).
    status_agent: ureq::Agent,
    /// Reusable HTTP agent for sending prompts (longer timeout).
    send_agent: ureq::Agent,
    /// Latest assistant reply per session id, with the session's
    /// `time.updated` it was read at. Messages are only refetched after the
    /// session changes.
    last_responses: RefCell<HashMap<String, (i64, Option<String>)>>,
    /// Running session ids, or `None` if the service couldn't be reached,
    /// with when they were fetched. Shared by every pane in one refresh.
    active: RefCell<Option<(Instant, Option<HashSet<String>>)>>,
}

impl OpenCode {
    pub fn new() -> Self {
        // timeout_global is the catch-all: it bounds the entire request,
        // including waiting for the response status/headers. Without it, an
        // opencode that accepts the TCP connection but never replies leaves the
        // blocking call stuck in recvfrom forever. Because the App is !Send and
        // runs on the daemon's single task, that freezes the whole daemon.
        let status_agent = ureq::Agent::new_with_config(
            ureq::config::Config::builder()
                .timeout_global(Some(TIMEOUT))
                .timeout_connect(Some(TIMEOUT))
                .timeout_recv_response(Some(TIMEOUT))
                .timeout_recv_body(Some(TIMEOUT))
                .build(),
        );
        let send_agent = ureq::Agent::new_with_config(
            ureq::config::Config::builder()
                .timeout_global(Some(SEND_TIMEOUT))
                .timeout_connect(Some(SEND_TIMEOUT))
                .timeout_recv_response(Some(SEND_TIMEOUT))
                .timeout_recv_body(Some(SEND_TIMEOUT))
                .build(),
        );
        Self {
            status_agent,
            send_agent,
            last_responses: RefCell::new(HashMap::new()),
            active: RefCell::new(None),
        }
    }

    /// Whether the session is running, or `None` if unknown.
    fn is_active(&self, session_id: &str) -> Option<bool> {
        let fresh = matches!(&*self.active.borrow(), Some((at, _)) if at.elapsed() < ACTIVE_TTL);
        if !fresh {
            // Only running sessions are listed. opencode doesn't expose retries.
            let active = Service::discover()
                .and_then(|s| {
                    s.get::<HashMap<String, IgnoredAny>>(
                        &self.status_agent,
                        "/api/session/active",
                        &[],
                    )
                })
                .ok()
                .map(|map| map.into_keys().collect());
            *self.active.borrow_mut() = Some((Instant::now(), active));
        }
        let active = self.active.borrow();
        let (_, ids) = active.as_ref()?;
        ids.as_ref().map(|ids| ids.contains(session_id))
    }

    /// The text of the session's latest assistant reply, if any.
    ///
    /// A busy session's latest messages can all be tool calls, so the
    /// previous reply is kept until a newer one shows up rather than
    /// flickering to `None`.
    fn last_response(&self, service: &Service, session: &Session) -> Option<String> {
        let previous = match self.last_responses.borrow().get(&session.id) {
            Some((updated, text)) if *updated == session.time.updated => return text.clone(),
            Some((_, text)) => text.clone(),
            None => None,
        };
        let Ok(messages) = service.get::<Vec<Message>>(
            &self.status_agent,
            &format!("/api/session/{}/message", session.id),
            &[("order", "desc"), ("limit", "20")],
        ) else {
            return previous;
        };
        let text = messages
            .iter()
            .find_map(|m| match m {
                Message::Assistant { content, .. } => last_text(content),
                _ => None,
            })
            .map(|t| t.chars().take(200).collect())
            .or(previous);
        self.last_responses
            .borrow_mut()
            .insert(session.id.clone(), (session.time.updated, text.clone()));
        text
    }
}

impl CodingAgent for OpenCode {
    fn name(&self) -> &str {
        "opencode"
    }

    fn process_name(&self) -> &str {
        "opencode"
    }

    fn query_status(&self, pane: &AgentPane) -> PaneStatus {
        let Some(id) = pane.db_session_id.as_deref() else {
            return PaneStatus::Unknown;
        };
        match self.is_active(id) {
            Some(true) => PaneStatus::Busy,
            Some(false) => PaneStatus::Idle,
            None => PaneStatus::Unknown,
        }
    }

    fn send_prompt(
        &self,
        _pane_pid: u32,
        session_id: &str,
        prompt: &str,
    ) -> anyhow::Result<String> {
        let service = Service::discover()?;
        let mut request = self
            .send_agent
            .post(format!("{}/api/session/{}/prompt", service.url, session_id));
        if let Some(authorization) = &service.authorization {
            request = request.header("Authorization", authorization);
        }
        request
            .send_json(serde_json::json!({ "text": prompt }))
            .context("Failed to send message")?;
        Ok("Message sent to opencode".to_string())
    }

    /// Finds the pane's session by matching the TUI's terminal title against
    /// session titles. A new session has no title until opencode generates
    /// one, so it isn't found until then.
    fn enrich_pane(&self, pane: &mut AgentPane) {
        let Some(title) = PaneTitle::parse(&pane.pane_title) else {
            return;
        };
        let Ok(service) = Service::discover() else {
            return;
        };
        let Ok(sessions) = service.get::<Vec<Session>>(
            &self.status_agent,
            "/api/session",
            &[("search", title.text()), ("order", "desc"), ("limit", "20")],
        ) else {
            return;
        };
        let pane_path = pane.canonical_path.as_deref().unwrap_or(&pane.pane_path);
        let Some(session) = pick_session(sessions, &title, Path::new(pane_path)) else {
            return;
        };

        pane.last_response = self.last_response(&service, &session);
        pane.last_activity = timestamp(session.time.updated);
        pane.model = session.model.map(|m| m.id);
        pane.agent = session.agent;
        pane.db_session_title = Some(session.title);
        pane.db_session_id = Some(session.id);
    }

    fn fetch_session_detail(&self, session_id: &str) -> Option<SessionDetail> {
        let service = Service::discover().ok()?;
        let session: Session = service
            .get(
                &self.status_agent,
                &format!("/api/session/{session_id}"),
                &[],
            )
            .ok()?;
        let messages: Vec<Message> = service
            .get(
                &self.status_agent,
                &format!("/api/session/{session_id}/message"),
                &[("order", "desc"), ("limit", "50")],
            )
            .unwrap_or_default();
        let mut summaries: Vec<MessageSummary> =
            messages.iter().filter_map(summarize).take(20).collect();
        summaries.reverse();

        let tokens = &session.tokens;
        Some(SessionDetail {
            input_tokens: tokens.input + tokens.cache.read + tokens.cache.write,
            output_tokens: tokens.output,
            session_created: timestamp(session.time.created),
            session_updated: timestamp(session.time.updated),
            messages: summaries,
            session_id: session.id,
            title: session.title,
            directory: session.location.directory,
            ..SessionDetail::default()
        })
    }
}

/// The opencode background service, as registered in its service file.
struct Service {
    url: String,
    authorization: Option<String>,
}

impl Service {
    /// Reads the service file on every call, since the service can be
    /// replaced (eg: after an opencode upgrade) while pertmux runs.
    fn discover() -> anyhow::Result<Self> {
        #[derive(Deserialize)]
        struct Registration {
            url: String,
            password: Option<String>,
        }

        let path = service_file().context("Could not find home directory")?;
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("Could not read {}", path.display()))?;
        let registration: Registration = serde_json::from_str(&text)
            .with_context(|| format!("Could not parse {}", path.display()))?;
        let authorization = registration.password.map(|password| {
            let credentials =
                base64::engine::general_purpose::STANDARD.encode(format!("opencode:{password}"));
            format!("Basic {credentials}")
        });
        Ok(Self {
            url: registration.url.trim_end_matches('/').to_string(),
            authorization,
        })
    }

    fn get<T: DeserializeOwned>(
        &self,
        agent: &ureq::Agent,
        path: &str,
        query: &[(&str, &str)],
    ) -> anyhow::Result<T> {
        #[derive(Deserialize)]
        struct Data<T> {
            data: T,
        }

        let mut request = agent.get(format!("{}{}", self.url, path));
        if let Some(authorization) = &self.authorization {
            request = request.header("Authorization", authorization);
        }
        for (key, value) in query {
            request = request.query(*key, *value);
        }
        let body: Data<T> = request.call()?.body_mut().read_json()?;
        Ok(body.data)
    }
}

/// opencode uses `$XDG_STATE_HOME`, falling back to `~/.local/state` on every
/// platform.
fn service_file() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".local/state")))?;
    Some(state.join("opencode/service.json"))
}

/// The session title shown in an opencode TUI's terminal title, eg:
/// `OC | Fix the login bug`.
#[derive(Debug, PartialEq)]
enum PaneTitle<'a> {
    Full(&'a str),
    /// Titles over 40 characters are cut to 37 and end in an ellipsis.
    Truncated(&'a str),
}

impl<'a> PaneTitle<'a> {
    fn parse(pane_title: &'a str) -> Option<Self> {
        let title = pane_title.strip_prefix("OC | ")?;
        let parsed = match title
            .strip_suffix('\u{2026}')
            .or_else(|| title.strip_suffix("..."))
        {
            Some(prefix) => Self::Truncated(prefix),
            None => Self::Full(title),
        };
        (!parsed.text().is_empty()).then_some(parsed)
    }

    fn text(&self) -> &'a str {
        match self {
            Self::Full(text) | Self::Truncated(text) => text,
        }
    }

    fn matches(&self, session_title: &str) -> bool {
        match self {
            Self::Full(title) => session_title == *title,
            Self::Truncated(prefix) => session_title.starts_with(prefix),
        }
    }
}

/// The most recently updated top-level session matching `title`, preferring
/// one started in `pane_path`. A session can live in a different directory
/// from the TUI showing it (eg: one started in a worktree below the pane's
/// directory), so sessions in a parent or child directory also count.
/// `sessions` must be newest first.
fn pick_session(sessions: Vec<Session>, title: &PaneTitle, pane_path: &Path) -> Option<Session> {
    let mut related = None;
    for session in sessions {
        // `search` is a case-insensitive substring match, so recheck.
        if session.parent_id.is_some()
            || session.time.archived.is_some()
            || !title.matches(&session.title)
        {
            continue;
        }
        let directory = Path::new(&session.location.directory);
        if directory == pane_path {
            return Some(session);
        }
        if related.is_none()
            && (directory.starts_with(pane_path) || pane_path.starts_with(directory))
        {
            related = Some(session);
        }
    }
    related
}

fn timestamp(millis: i64) -> Option<Timestamp> {
    Timestamp::from_millisecond(millis).ok()
}

fn last_text(content: &[Content]) -> Option<&str> {
    content.iter().rev().find_map(|c| match c {
        Content::Text { text } if !text.trim().is_empty() => Some(text.trim()),
        _ => None,
    })
}

fn summarize(message: &Message) -> Option<MessageSummary> {
    let preview = |text: &str| Some(text.chars().take(120).collect::<String>());
    match message {
        Message::User { text, time } => Some(MessageSummary {
            role: "user".to_string(),
            agent: None,
            model: None,
            output_tokens: 0,
            timestamp: timestamp(time.created)?,
            text_preview: Some(text.trim())
                .filter(|t| !t.is_empty())
                .and_then(preview),
        }),
        // Each tool-calling step is its own message. Skip the ones with no
        // text.
        Message::Assistant {
            agent,
            model,
            tokens,
            time,
            content,
        } => Some(MessageSummary {
            role: "assistant".to_string(),
            agent: agent.clone(),
            model: model.as_ref().map(|m| m.id.clone()),
            output_tokens: tokens.as_ref().map_or(0, |t| t.output),
            timestamp: timestamp(time.created)?,
            text_preview: preview(last_text(content)?),
        }),
        Message::Other => None,
    }
}

// opencode API types. Only the fields pertmux reads are listed.

#[derive(Deserialize)]
struct Session {
    id: String,
    #[serde(rename = "parentID")]
    parent_id: Option<String>,
    agent: Option<String>,
    model: Option<ModelRef>,
    #[serde(default)]
    tokens: Tokens,
    time: SessionTime,
    title: String,
    location: Location,
}

#[derive(Deserialize)]
struct ModelRef {
    id: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Tokens {
    input: u64,
    output: u64,
    cache: CacheTokens,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CacheTokens {
    read: u64,
    write: u64,
}

/// Milliseconds since the Unix epoch.
#[derive(Deserialize)]
struct SessionTime {
    created: i64,
    updated: i64,
    archived: Option<i64>,
}

#[derive(Deserialize)]
struct Location {
    directory: String,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Message {
    User {
        #[serde(default)]
        text: String,
        time: MessageTime,
    },
    Assistant {
        agent: Option<String>,
        model: Option<ModelRef>,
        tokens: Option<Tokens>,
        time: MessageTime,
        #[serde(default)]
        content: Vec<Content>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct MessageTime {
    created: i64,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Content {
    Text {
        text: String,
    },
    #[serde(other)]
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_title_strips_tui_decoration() {
        assert_eq!(
            PaneTitle::parse("OC | Fix the login bug"),
            Some(PaneTitle::Full("Fix the login bug"))
        );
        assert_eq!(
            PaneTitle::parse("OC | Optimizing GET /v1/transactions/ \u{2026}"),
            Some(PaneTitle::Truncated("Optimizing GET /v1/transactions/ "))
        );
        assert_eq!(
            PaneTitle::parse("OC | Old style..."),
            Some(PaneTitle::Truncated("Old style"))
        );
    }

    #[test]
    fn pane_title_rejects_other_titles() {
        assert_eq!(PaneTitle::parse("zsh"), None);
        assert_eq!(PaneTitle::parse("OpenCode"), None);
        assert_eq!(PaneTitle::parse("OC | "), None);
        assert_eq!(PaneTitle::parse("OC | \u{2026}"), None);
    }

    fn session(id: &str, title: &str, directory: &str) -> Session {
        Session {
            id: id.to_string(),
            parent_id: None,
            agent: None,
            model: None,
            tokens: Tokens::default(),
            time: SessionTime {
                created: 0,
                updated: 0,
                archived: None,
            },
            title: title.to_string(),
            location: Location {
                directory: directory.to_string(),
            },
        }
    }

    fn pick(sessions: Vec<Session>, title: &str, pane_path: &str) -> Option<String> {
        let title = PaneTitle::parse(title).unwrap();
        pick_session(sessions, &title, Path::new(pane_path)).map(|s| s.id)
    }

    #[test]
    fn pick_session_prefers_pane_directory() {
        let sessions = vec![
            session("worktree", "Fix bug", "/repo/.worktrees/fix"),
            session("exact", "Fix bug", "/repo"),
        ];
        assert_eq!(
            pick(sessions, "OC | Fix bug", "/repo").as_deref(),
            Some("exact")
        );
    }

    #[test]
    fn pick_session_falls_back_to_related_directory() {
        let sessions = vec![
            session("other", "Fix bug", "/elsewhere"),
            session("worktree", "Fix bug", "/repo/.worktrees/fix"),
        ];
        assert_eq!(
            pick(sessions, "OC | Fix bug", "/repo").as_deref(),
            Some("worktree")
        );
        let sessions = vec![session("parent", "Fix bug", "/home")];
        assert_eq!(
            pick(sessions, "OC | Fix bug", "/home/repo").as_deref(),
            Some("parent")
        );
    }

    #[test]
    fn pick_session_matches_full_titles_exactly() {
        let sessions = vec![
            session("longer", "Fix bug properly", "/repo"),
            session("exact", "Fix bug", "/repo"),
        ];
        assert_eq!(
            pick(sessions, "OC | Fix bug", "/repo").as_deref(),
            Some("exact")
        );
        let sessions = vec![session("longer", "Fix bug properly", "/repo")];
        assert_eq!(
            pick(sessions, "OC | Fix bug\u{2026}", "/repo").as_deref(),
            Some("longer")
        );
    }

    #[test]
    fn pick_session_skips_non_matching() {
        let mut child = session("child", "Fix bug", "/repo");
        child.parent_id = Some("ses_root".to_string());
        let mut archived = session("archived", "Fix bug", "/repo");
        archived.time.archived = Some(1);
        let sessions = vec![
            child,
            archived,
            session("case", "fix bug", "/repo"),
            session("sibling", "Fix bug", "/repo2"),
        ];
        assert_eq!(pick(sessions, "OC | Fix bug", "/repo"), None);
    }

    #[test]
    fn decodes_messages() {
        let json = r#"[
            {"type":"assistant","agent":"build","model":{"id":"opus","providerID":"p"},
             "time":{"created":1791036602813},
             "content":[{"type":"text","text":"first"},{"type":"tool","name":"read"},
                        {"type":"text","text":" done "}]},
            {"type":"assistant","agent":"build","model":{"id":"opus"},"tokens":null,
             "time":{"created":1791036602000},
             "content":[{"type":"reasoning","text":"hmm"},{"type":"tool","name":"read"}]},
            {"type":"idle","outcome":"completed"},
            {"type":"user","time":{"created":1791036601000},"text":"hello"}
        ]"#;
        let messages: Vec<Message> = serde_json::from_str(json).unwrap();
        let summaries: Vec<MessageSummary> = messages.iter().filter_map(summarize).collect();

        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].role, "assistant");
        assert_eq!(summaries[0].text_preview.as_deref(), Some("done"));
        assert_eq!(summaries[1].role, "user");
        assert_eq!(summaries[1].text_preview.as_deref(), Some("hello"));
    }
}
