use super::CodingAgent;
use crate::discovery::{self, Endpoint, ListenerMap};
use crate::types::{AgentPane, PaneStatus, SessionDetail};
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

pub struct OpenCode {
    db_path: Option<String>,
    /// Reusable HTTP agent for status queries (short timeout).
    status_agent: ureq::Agent,
    /// Reusable HTTP agent for sending prompts (longer timeout).
    send_agent: ureq::Agent,
}

impl OpenCode {
    pub fn new(db_path: Option<String>) -> Self {
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
            db_path,
            status_agent,
            send_agent,
        }
    }
}

// ─── Opencode-specific API types ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SessionStatus {
    #[serde(rename = "type")]
    status_type: String,
    attempt: Option<u32>,
    message: Option<String>,
}

type SessionStatusMap = HashMap<String, SessionStatus>;

// ─── Trait implementation ────────────────────────────────────────────────────

const TIMEOUT: Duration = Duration::from_secs(1);
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

impl CodingAgent for OpenCode {
    fn name(&self) -> &str {
        "opencode"
    }

    fn process_name(&self) -> &str {
        "opencode"
    }

    fn query_status(&self, pane: &AgentPane, sys: &System, listeners: &ListenerMap) -> PaneStatus {
        let Some(endpoint) = discovery::discover_endpoint(sys, listeners, pane.pane_pid) else {
            return PaneStatus::Unknown;
        };

        let Some(map) =
            get_session_status(&self.status_agent, &endpoint.base_url(), &pane.pane_path)
        else {
            return PaneStatus::Unknown;
        };

        status_for_pane(&map, pane.db_session_id.as_deref(), &endpoint)
    }

    fn send_prompt(&self, pane_pid: u32, session_id: &str, prompt: &str) -> anyhow::Result<String> {
        // send_prompt is a rare user action, so fresh scans are acceptable.
        let mut sys = System::new();
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
        );
        let listeners = discovery::build_listener_map();
        let endpoint = discovery::discover_endpoint(&sys, &listeners, pane_pid)
            .ok_or_else(|| anyhow::anyhow!("Could not discover opencode endpoint"))?;

        let url = format!("{}/session/{}/message", endpoint.base_url(), session_id);
        let body = serde_json::json!({
            "parts": [{"type": "text", "text": prompt}]
        });

        let response = self
            .send_agent
            .post(&url)
            .send_json(&body)
            .map_err(|e| anyhow::anyhow!("Failed to send message: {}", e))?;

        if response.status().is_success() {
            Ok("Message sent to opencode".to_string())
        } else {
            let status = response.status();
            anyhow::bail!("opencode API error ({})", status)
        }
    }

    fn enrich_pane(&self, pane: &mut AgentPane) {
        crate::db::enrich_pane(pane, self.db_path.as_deref());
    }

    fn fetch_session_detail(&self, session_id: &str) -> Option<SessionDetail> {
        crate::db::fetch_session_detail(session_id, self.db_path.as_deref())
    }
}

// ─── Internal helpers ────────────────────────────────────────────────────────

/// Query `/session/status`, scoped to the pane's directory.
///
/// The `directory` query param is required on a shared server: without it
/// the server scopes the response to its own working directory's project
/// and returns an empty map regardless of activity. Standalone instances
/// resolve to the same scope either way.
fn get_session_status(
    agent: &ureq::Agent,
    base_url: &str,
    directory: &str,
) -> Option<SessionStatusMap> {
    let url = format!("{}/session/status", base_url);
    let mut response = agent.get(&url).query("directory", directory).call().ok()?;
    response.body_mut().read_json::<SessionStatusMap>().ok()
}

/// Determine the pane's status from the `/session/status` response.
///
/// The map only contains non-idle sessions within the pane's directory
/// scope. When the pane's session id is known, only that entry counts —
/// on a shared server, other panes' sessions can share the scope (e.g.
/// non-git directories all map to the "global" project). Without a
/// session id, a standalone instance can fall back to aggregating (its
/// scope only holds the pane's own sessions), but on a shared server the
/// aggregate could reflect a sibling pane, so report Unknown instead.
fn status_for_pane(
    map: &SessionStatusMap,
    session_id: Option<&str>,
    endpoint: &Endpoint,
) -> PaneStatus {
    match session_id {
        Some(id) => map.get(id).map_or(PaneStatus::Idle, status_from_entry),
        None => match endpoint {
            Endpoint::Local(_) => status_from_map(map),
            Endpoint::Attached(_) => PaneStatus::Unknown,
        },
    }
}

fn status_from_entry(status: &SessionStatus) -> PaneStatus {
    match status.status_type.as_str() {
        "busy" => PaneStatus::Busy,
        "retry" => PaneStatus::Retry {
            attempt: status.attempt.unwrap_or(0),
            message: status.message.clone().unwrap_or_default(),
        },
        _ => PaneStatus::Idle,
    }
}

/// Determine overall status from the opencode API response.
/// Priority: Busy > Retry > Idle.
fn status_from_map(map: &SessionStatusMap) -> PaneStatus {
    if map.is_empty() {
        return PaneStatus::Idle;
    }
    if map.values().any(|s| s.status_type == "busy") {
        return PaneStatus::Busy;
    }
    if let Some(status) = map.values().find(|s| s.status_type == "retry") {
        return PaneStatus::Retry {
            attempt: status.attempt.unwrap_or(0),
            message: status.message.clone().unwrap_or_default(),
        };
    }
    PaneStatus::Idle
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: &[(&str, &str)]) -> SessionStatusMap {
        entries
            .iter()
            .map(|(id, ty)| {
                (
                    id.to_string(),
                    SessionStatus {
                        status_type: ty.to_string(),
                        attempt: Some(2),
                        message: Some("rate limited".to_string()),
                    },
                )
            })
            .collect()
    }

    fn shared() -> Endpoint {
        Endpoint::Attached("http://127.0.0.1:4599".to_string())
    }

    #[test]
    fn known_session_id_selects_only_that_entry() {
        let m = map(&[("ses_a", "busy"), ("ses_b", "retry")]);
        assert_eq!(
            status_for_pane(&m, Some("ses_b"), &shared()),
            PaneStatus::Retry {
                attempt: 2,
                message: "rate limited".to_string()
            }
        );
    }

    #[test]
    fn session_absent_from_map_is_idle() {
        let m = map(&[("ses_other", "busy")]);
        assert_eq!(
            status_for_pane(&m, Some("ses_mine"), &shared()),
            PaneStatus::Idle
        );
    }

    #[test]
    fn no_session_id_on_shared_server_is_unknown() {
        // The scoped map can still contain sibling panes' sessions (shared
        // directory scope), so aggregating would misreport.
        let m = map(&[("ses_other", "busy")]);
        assert_eq!(status_for_pane(&m, None, &shared()), PaneStatus::Unknown);
    }

    #[test]
    fn no_session_id_on_local_instance_aggregates() {
        let m = map(&[("ses_a", "busy")]);
        assert_eq!(
            status_for_pane(&m, None, &Endpoint::Local(4096)),
            PaneStatus::Busy
        );
        let empty = map(&[]);
        assert_eq!(
            status_for_pane(&empty, None, &Endpoint::Local(4096)),
            PaneStatus::Idle
        );
    }
}
