use netstat2::{AddressFamilyFlags, ProtocolFlags, ProtocolSocketInfo, TcpState, get_sockets_info};
use std::collections::HashMap;
use sysinfo::{Pid, System};

/// Pre-computed map of PID → TCP listening port.
///
/// Built once per refresh tick by [`build_listener_map`] and shared across all
/// `discover_port` calls, avoiding redundant `/proc/net/tcp` and `/proc/*/fd`
/// scans (which are the most expensive part of the old per-pane approach).
pub type ListenerMap = HashMap<u32, u16>;

/// Scan the system socket table once and return a map from PID to its TCP
/// listening port. Only includes LISTEN-state TCP sockets.
pub fn build_listener_map() -> ListenerMap {
    let af_flags = AddressFamilyFlags::IPV4 | AddressFamilyFlags::IPV6;
    let proto_flags = ProtocolFlags::TCP;

    let Ok(sockets) = get_sockets_info(af_flags, proto_flags) else {
        return HashMap::new();
    };

    let mut map = HashMap::new();
    for socket in sockets {
        if let ProtocolSocketInfo::Tcp(tcp) = &socket.protocol_socket_info
            && tcp.state == TcpState::Listen
        {
            for &pid in &socket.associated_pids {
                // First listener wins — stable across ticks.
                map.entry(pid).or_insert(tcp.local_port);
            }
        }
    }
    map
}

/// How to reach an opencode instance's HTTP API.
#[derive(Debug, Clone, PartialEq)]
pub enum Endpoint {
    /// Standalone instance listening on a local port (started with `--port 0`).
    Local(u16),
    /// Attach client of a shared server; base URL parsed from its argv
    /// (`opencode attach <url> ...`).
    Attached(String),
}

impl Endpoint {
    /// Base URL for API requests, without a trailing slash.
    pub fn base_url(&self) -> String {
        match self {
            Endpoint::Local(port) => format!("http://127.0.0.1:{}", port),
            Endpoint::Attached(url) => url.trim_end_matches('/').to_string(),
        }
    }
}

/// Discover the HTTP endpoint for an opencode instance given the pane's PID.
///
/// Walks the process tree from the shell PID to find the opencode process.
/// If it is an `opencode attach <url>` client (no listener of its own), the
/// shared server URL is taken from its argv; otherwise it and its children
/// are checked for a TCP listener.
///
/// Accepts a pre-refreshed `&System` and pre-built `&ListenerMap` to avoid
/// redundant `/proc` scans — the caller is expected to build both once per tick.
pub fn discover_endpoint(sys: &System, listeners: &ListenerMap, pane_pid: u32) -> Option<Endpoint> {
    let opencode_pid = find_opencode_pid(sys, pane_pid)?;

    if let Some(url) = attach_url(sys, opencode_pid) {
        return Some(Endpoint::Attached(url));
    }

    // Collect opencode PID + all its children (the HTTP server may run in a child worker).
    let mut candidate_pids = vec![opencode_pid];
    candidate_pids.extend(find_child_pids(sys, opencode_pid));

    candidate_pids
        .iter()
        .find_map(|pid| listeners.get(pid).copied())
        .map(Endpoint::Local)
}

/// Extract the server URL from an `opencode attach <url>` client's argv.
fn attach_url(sys: &System, pid: u32) -> Option<String> {
    let proc_ = sys.process(Pid::from_u32(pid))?;
    let args: Vec<String> = proc_
        .cmd()
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    attach_url_from_args(&args)
}

/// Parse the server URL out of argv like
/// `["/path/to/opencode", "attach", "http://127.0.0.1:4599", "--dir", ...]`.
///
/// `attach` must be the subcommand (first non-flag argument), not just any
/// argv word — `opencode run attach http://x ...` is a prompt, not a client.
fn attach_url_from_args(args: &[String]) -> Option<String> {
    let mut rest = args.iter().skip(1).skip_while(|a| a.starts_with('-'));
    if rest.next().map(String::as_str) != Some("attach") {
        return None;
    }
    rest.find(|a| a.starts_with("http://") || a.starts_with("https://"))
        .cloned()
}

/// Find the opencode process in the tree rooted at `shell_pid`.
///
/// Checks: shell itself → direct children → grandchildren → fallback to first child.
fn find_opencode_pid(sys: &System, shell_pid: u32) -> Option<u32> {
    if is_opencode_process(sys, shell_pid) {
        return Some(shell_pid);
    }

    let children = find_child_pids(sys, shell_pid);
    for &child in &children {
        if is_opencode_process(sys, child) {
            return Some(child);
        }
    }

    // Grandchildren
    for &child in &children {
        for grandchild in find_child_pids(sys, child) {
            if is_opencode_process(sys, grandchild) {
                return Some(grandchild);
            }
        }
    }

    // Fallback: first child (might be opencode under a wrapper)
    children.first().copied()
}

/// Check if a PID corresponds to an opencode process by inspecting its command.
fn is_opencode_process(sys: &System, pid: u32) -> bool {
    sys.process(Pid::from_u32(pid))
        .map(|p| {
            let name = p.name().to_string_lossy();
            if name.contains("opencode") {
                return true;
            }
            // Also check argv[0] in case the binary name differs
            p.cmd()
                .first()
                .map(|arg| arg.to_string_lossy().contains("opencode"))
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

/// Find all direct child PIDs of a given parent.
fn find_child_pids(sys: &System, parent_pid: u32) -> Vec<u32> {
    let parent = Pid::from_u32(parent_pid);
    sys.processes()
        .iter()
        .filter_map(|(pid, proc_)| {
            if proc_.parent() == Some(parent) {
                Some(pid.as_u32())
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn attach_url_parsed_from_argv() {
        let a = args(&[
            "/home/u/.opencode/bin/opencode",
            "attach",
            "http://127.0.0.1:4599",
            "--dir",
            "/tmp/proj",
        ]);
        assert_eq!(
            attach_url_from_args(&a),
            Some("http://127.0.0.1:4599".to_string())
        );
    }

    #[test]
    fn attach_url_skips_non_url_flags() {
        let a = args(&["opencode", "attach", "--mini", "https://oc.example:4096"]);
        assert_eq!(
            attach_url_from_args(&a),
            Some("https://oc.example:4096".to_string())
        );
    }

    #[test]
    fn no_attach_subcommand() {
        assert_eq!(
            attach_url_from_args(&args(&["opencode", "--port", "0"])),
            None
        );
        assert_eq!(attach_url_from_args(&args(&["opencode"])), None);
        assert_eq!(attach_url_from_args(&args(&[])), None);
    }

    #[test]
    fn attach_without_url() {
        assert_eq!(attach_url_from_args(&args(&["opencode", "attach"])), None);
    }

    #[test]
    fn attach_as_prompt_word_not_confused() {
        // `attach` appearing as a prompt word must not classify the pane as
        // an attach client (and must not leak traffic to the URL).
        assert_eq!(
            attach_url_from_args(&args(&[
                "opencode",
                "run",
                "attach",
                "http://example.com",
                "and",
                "summarize"
            ])),
            None
        );
    }

    #[test]
    fn attach_after_global_flags() {
        assert_eq!(
            attach_url_from_args(&args(&[
                "opencode",
                "--print-logs",
                "attach",
                "http://127.0.0.1:4599"
            ])),
            Some("http://127.0.0.1:4599".to_string())
        );
    }

    #[test]
    fn attach_in_project_path_not_confused() {
        // A standalone TUI opened on a directory literally named "attach"
        // must not be treated as an attach client.
        assert_eq!(
            attach_url_from_args(&args(&["opencode", "/home/u/attach"])),
            None
        );
    }

    #[test]
    fn endpoint_base_url() {
        assert_eq!(Endpoint::Local(1234).base_url(), "http://127.0.0.1:1234");
        assert_eq!(
            Endpoint::Attached("http://127.0.0.1:4599/".into()).base_url(),
            "http://127.0.0.1:4599"
        );
    }
}
