use crate::app::{PopupState, SelectionSection};
use crate::banner::{DIM, GRAY, GREEN, ORANGE, RESET, WHITE};
use crate::daemon;
use crate::project_sort::{ProjectSort, load_last_sort, save_last_sort};
use crate::project_stats::build_sorted_project_stats;
use crate::protocol::{ClientMsg, DaemonMsg, DashboardSnapshot, PROTOCOL_VERSION, RefreshStep};
use crate::tmux;
use crate::types::AgentPane;
use crate::ui;
use crate::ui::helpers::truncate;
use anyhow::Result;
use bytes::Bytes;
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures::{SinkExt, StreamExt};
use ratatui::prelude::*;
use std::cell::Cell;
use std::io;
use std::path::PathBuf;
use std::time::Instant;
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

fn last_project_path() -> Option<PathBuf> {
    let data_dir = dirs::data_dir()?;
    Some(data_dir.join("pertmux").join("last_project"))
}

fn save_last_project(name: &str) {
    if let Some(path) = last_project_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&path, name);
    }
}

fn load_last_project() -> Option<String> {
    let path = last_project_path()?;
    std::fs::read_to_string(&path)
        .ok()
        .map(|s| s.trim().to_string())
}

fn open_url_in_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let _ = std::process::Command::new("open").arg(url).spawn();
}

/// Stores everything needed to open a tmux pane for a newly-created worktree.
/// Set when `CreateWorktreeWithPrompt` ActionResult comes back ok; fulfilled on
/// the next snapshot that contains the new worktree path.
struct WorktreeOpenRequest {
    project_idx: usize,
    branch: String,
    /// Full command to send to the pane (template already filled).
    command: String,
    project_name: String,
}

pub struct ClientState {
    pub snapshot: DashboardSnapshot,
    pub active_project: usize,
    pub mr_selected: Vec<usize>,
    pub worktree_selected: Vec<usize>,
    pub selection_section: Vec<SelectionSection>,
    pub selected: usize,
    pub popup: PopupState,
    pub notification: Option<(String, Instant)>,
    /// Live progress steps sent by the daemon while a refresh is in flight.
    /// Cleared when the daemon sends `Progress(vec![])`.
    pub refresh_steps: Vec<RefreshStep>,
    pub running: bool,
    /// Applied sort column (persisted).
    pub project_sort_col: ProjectSort,
    /// Applied sort direction (persisted). True = descending.
    pub project_sort_desc: bool,
    /// Whether the Projects pane is the focused section (Tab cycles into it).
    pub project_focused: bool,
    /// Cursor row within the sorted projects list (display index, not canonical).
    pub project_cursor_row: usize,
    /// Cursor column within the projects table while focused (ephemeral).
    pub project_cursor_col: ProjectSort,
    /// Scroll offset (top row) for the projects table.
    pub project_scroll: usize,
    /// Last rendered viewport row count (excluding chrome) — written by render,
    /// read by key handler to clamp scrolling.
    pub overview_height: Cell<u16>,
    /// Set when `CreateWorktreeWithPrompt` is submitted, cleared on ActionResult.
    pending_create_with_prompt: Option<WorktreeOpenRequest>,
    /// Set when ActionResult ok is received for a `CreateWorktreeWithPrompt`.
    /// On the next snapshot update we find the worktree path and open the pane.
    pending_open_worktree: Option<WorktreeOpenRequest>,
}

impl ClientState {
    fn from_snapshot(mut snapshot: DashboardSnapshot) -> Self {
        let n = snapshot.projects.len();
        let active_project = if snapshot.auto_switch_project {
            crate::tmux::get_own_session().and_then(|session| {
                let session_lower = session.to_lowercase();
                snapshot
                    .projects
                    .iter()
                    .position(|p| p.name.to_lowercase() == session_lower)
            })
        } else {
            None
        }
        .or_else(|| {
            load_last_project()
                .and_then(|name| snapshot.projects.iter().position(|p| p.name == name))
        })
        .unwrap_or(0);
        let popup = if !snapshot.pending_changes.is_empty() {
            let changes = std::mem::take(&mut snapshot.pending_changes);
            PopupState::ChangeSummary {
                changes,
                selected: 0,
            }
        } else {
            PopupState::None
        };
        let (project_sort_col, project_sort_desc) =
            load_last_sort().unwrap_or((ProjectSort::Mrs, true));
        Self {
            snapshot,
            active_project,
            mr_selected: vec![0; n],
            worktree_selected: vec![0; n],
            selection_section: (0..n).map(|_| SelectionSection::Worktrees).collect(),
            selected: 0,
            popup,
            notification: None,
            running: true,
            project_sort_col,
            project_sort_desc,
            project_focused: false,
            project_cursor_row: 0,
            project_cursor_col: project_sort_col,
            project_scroll: 0,
            overview_height: Cell::new(0),
            refresh_steps: vec![],
            pending_create_with_prompt: None,
            pending_open_worktree: None,
        }
    }

    fn update_snapshot(&mut self, mut snapshot: DashboardSnapshot) {
        // Show a toast notification for any MR changes that arrived while connected.
        // (The activity feed itself is managed entirely by the daemon and arrives
        // pre-populated in snapshot.activity_feed — no client-side conversion needed.)
        if !snapshot.pending_changes.is_empty() {
            let changes = std::mem::take(&mut snapshot.pending_changes);
            let summary: String = changes
                .iter()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            self.notification = Some((summary, Instant::now()));
        }

        while self.mr_selected.len() < snapshot.projects.len() {
            self.mr_selected.push(0);
            self.worktree_selected.push(0);
            self.selection_section.push(SelectionSection::Worktrees);
        }

        for (i, proj) in snapshot.projects.iter().enumerate() {
            if self.mr_selected[i] >= proj.dashboard.linked_mrs.len()
                && !proj.dashboard.linked_mrs.is_empty()
            {
                self.mr_selected[i] = proj.dashboard.linked_mrs.len() - 1;
            }
            if self.worktree_selected[i] >= proj.cached_worktrees.len()
                && !proj.cached_worktrees.is_empty()
            {
                self.worktree_selected[i] = proj.cached_worktrees.len() - 1;
            }
        }

        if self.active_project >= snapshot.projects.len() && !snapshot.projects.is_empty() {
            self.active_project = snapshot.projects.len() - 1;
        }
        if self.selected >= snapshot.panes.len() && !snapshot.panes.is_empty() {
            self.selected = snapshot.panes.len() - 1;
        }

        // Clamp project cursor/scroll if project list shrunk or is empty.
        if snapshot.projects.is_empty() {
            self.project_focused = false;
            self.project_cursor_row = 0;
            self.project_scroll = 0;
        } else {
            let max_idx = snapshot.projects.len() - 1;
            self.project_cursor_row = self.project_cursor_row.min(max_idx);
            self.project_scroll = self.project_scroll.min(max_idx);
        }

        // Resolve the anchor against the outgoing pane list: `filtered` still
        // indexes into it.
        let session_anchor = self.session_search_anchor();

        self.snapshot = snapshot;

        // Worktree lists may have changed; refresh the global search results so
        // (project_idx, worktree_idx) pairs never go stale.
        if matches!(self.popup, PopupState::WorktreeSearch { .. }) {
            self.recompute_worktree_search();
        }

        // Panes churn on every 2s tick, so the same staleness applies to the
        // session search indices.
        if matches!(self.popup, PopupState::SessionSearch { .. }) {
            self.refilter_session_search(session_anchor);
        }

        // After updating the snapshot, try to fulfil a pending "open worktree pane"
        // request. The worktree path arrives in the snapshot broadcast that follows
        // the ActionResult ok message, so we check here rather than in ActionResult.
        if let Some(pending) = self.pending_open_worktree.take() {
            let result = self
                .snapshot
                .projects
                .get(pending.project_idx)
                .and_then(|proj| {
                    proj.cached_worktrees
                        .iter()
                        .find(|wt| wt.branch.as_deref() == Some(pending.branch.as_str()))
                        .and_then(|wt| wt.path.clone())
                        .map(|path| (path, pending.project_name.clone(), pending.command.clone()))
                });
            match result {
                Some((path, project_name, command)) => {
                    if let Err(e) = tmux::find_or_create_pane(&path, &project_name, Some(&command))
                    {
                        self.notify(format!("Failed to open pane: {}", e));
                    }
                }
                None => {
                    // Worktree not in snapshot yet — keep pending for next update.
                    self.pending_open_worktree = Some(pending);
                }
            }
        }
    }

    fn has_projects(&self) -> bool {
        !self.snapshot.projects.is_empty()
    }

    fn active_project(&self) -> Option<&crate::protocol::ProjectSnapshot> {
        self.snapshot.projects.get(self.active_project)
    }

    fn has_popup(&self) -> bool {
        !matches!(self.popup, PopupState::None)
    }

    pub fn notify(&mut self, msg: impl Into<String>) {
        self.notification = Some((msg.into(), Instant::now()));
    }

    fn move_up(&mut self) {
        if let Some(proj) = self.snapshot.projects.get(self.active_project) {
            match self
                .selection_section
                .get(self.active_project)
                .unwrap_or(&SelectionSection::Worktrees)
            {
                SelectionSection::MergeRequests => {
                    if self.mr_selected[self.active_project] > 0 {
                        self.mr_selected[self.active_project] -= 1;
                    }
                }
                SelectionSection::Worktrees => {
                    if self.worktree_selected[self.active_project] > 0 {
                        self.worktree_selected[self.active_project] -= 1;
                    }
                }
            }

            if self.mr_selected[self.active_project] >= proj.dashboard.linked_mrs.len()
                && !proj.dashboard.linked_mrs.is_empty()
            {
                self.mr_selected[self.active_project] = proj.dashboard.linked_mrs.len() - 1;
            }
        } else if !self.snapshot.panes.is_empty() && self.selected > 0 {
            self.selected -= 1;
        }
    }

    fn move_down(&mut self) {
        if let Some(proj) = self.snapshot.projects.get(self.active_project) {
            match self
                .selection_section
                .get(self.active_project)
                .unwrap_or(&SelectionSection::Worktrees)
            {
                SelectionSection::MergeRequests => {
                    if !proj.dashboard.linked_mrs.is_empty()
                        && self.mr_selected[self.active_project]
                            < proj.dashboard.linked_mrs.len() - 1
                    {
                        self.mr_selected[self.active_project] += 1;
                    }
                }
                SelectionSection::Worktrees => {
                    if !proj.cached_worktrees.is_empty()
                        && self.worktree_selected[self.active_project]
                            < proj.cached_worktrees.len() - 1
                    {
                        self.worktree_selected[self.active_project] += 1;
                    }
                }
            }
        } else if !self.snapshot.panes.is_empty() && self.selected < self.snapshot.panes.len() - 1 {
            self.selected += 1;
        }
    }

    /// Tab cycles MergeRequests → Worktrees → Projects(focused) → MergeRequests.
    fn toggle_section(&mut self) {
        if self.snapshot.projects.is_empty() {
            return;
        }
        if self.project_focused {
            // Exit focus, land on MergeRequests.
            self.project_focused = false;
            if let Some(section) = self.selection_section.get_mut(self.active_project) {
                *section = SelectionSection::MergeRequests;
            }
            return;
        }
        let section = self
            .selection_section
            .get_mut(self.active_project)
            .expect("selection section exists for project");
        match section {
            SelectionSection::MergeRequests => {
                *section = SelectionSection::Worktrees;
            }
            SelectionSection::Worktrees => {
                // Enter Projects focus mode.
                self.project_focused = true;
                let display_idx = self.active_project_display_index();
                self.project_cursor_row = display_idx;
                self.project_cursor_col = self.project_sort_col;
                self.ensure_cursor_visible();
            }
        }
    }

    /// Build canonical→display order for the projects list under current sort.
    /// Thin wrapper around [`project_stats::build_sorted_project_stats`].
    pub fn sorted_project_canonical_indices(&self) -> Vec<usize> {
        build_sorted_project_stats(
            &self.snapshot,
            self.project_sort_col,
            self.project_sort_desc,
        )
        .into_iter()
        .map(|s| s.canonical_idx)
        .collect()
    }

    fn active_project_display_index(&self) -> usize {
        self.sorted_project_canonical_indices()
            .iter()
            .position(|&i| i == self.active_project)
            .unwrap_or(0)
    }

    fn ensure_cursor_visible(&mut self) {
        let h = self.overview_height.get() as usize;
        if h == 0 {
            return;
        }
        if self.project_cursor_row < self.project_scroll {
            self.project_scroll = self.project_cursor_row;
        } else if self.project_cursor_row >= self.project_scroll + h {
            self.project_scroll = self.project_cursor_row + 1 - h;
        }
    }

    fn current_mr_iid(&self) -> Option<u64> {
        let proj = self.active_project()?;
        if !matches!(
            self.selection_section.get(self.active_project),
            Some(SelectionSection::MergeRequests)
        ) {
            return None;
        }
        proj.dashboard
            .linked_mrs
            .get(*self.mr_selected.get(self.active_project).unwrap_or(&0))
            .map(|l| l.mr.iid)
    }

    fn open_selected_mr_in_browser(&self) {
        if let Some(proj) = self.snapshot.projects.get(self.active_project)
            && let Some(linked) = proj
                .dashboard
                .linked_mrs
                .get(*self.mr_selected.get(self.active_project).unwrap_or(&0))
        {
            open_url_in_browser(&linked.mr.web_url);
        }
    }

    fn open_mr_overview(&mut self) {
        if self.snapshot.global_mrs.is_empty() {
            self.notify("No open MRs found");
            return;
        }
        self.popup = PopupState::MrOverview { selected: 0 };
    }

    fn open_activity_feed(&mut self) {
        if self.snapshot.activity_feed.is_empty() {
            self.notify("No activity yet");
            return;
        }
        self.popup = PopupState::ActivityFeed { selected: 0 };
    }

    fn open_keybindings_help(&mut self) {
        self.popup = PopupState::KeybindingsHelp;
    }

    fn copy_selected_branch(&mut self) {
        let branch = if let Some(proj) = self.snapshot.projects.get(self.active_project) {
            match self
                .selection_section
                .get(self.active_project)
                .unwrap_or(&SelectionSection::Worktrees)
            {
                SelectionSection::MergeRequests => proj
                    .dashboard
                    .linked_mrs
                    .get(*self.mr_selected.get(self.active_project).unwrap_or(&0))
                    .map(|l| l.mr.source_branch.clone()),
                SelectionSection::Worktrees => proj
                    .cached_worktrees
                    .get(
                        *self
                            .worktree_selected
                            .get(self.active_project)
                            .unwrap_or(&0),
                    )
                    .and_then(|wt| wt.branch.clone()),
            }
        } else {
            None
        };

        if let Some(branch) = branch {
            let ok = std::process::Command::new("pbcopy")
                .stdin(std::process::Stdio::piped())
                .spawn()
                .and_then(|mut child| {
                    use std::io::Write;
                    if let Some(ref mut stdin) = child.stdin {
                        stdin.write_all(branch.as_bytes())?;
                    }
                    child.wait()
                })
                .is_ok();
            if ok {
                self.notify(format!("Copied: {}", branch));
            }
        }
    }

    fn open_create_popup(&mut self) {
        if let Some(_proj) = self.snapshot.projects.get(self.active_project)
            && matches!(
                self.selection_section.get(self.active_project),
                Some(SelectionSection::Worktrees)
            )
        {
            self.popup = PopupState::CreateWorktree {
                input: String::new(),
            };
        }
    }

    fn open_create_with_prompt_popup(&mut self) {
        if self.snapshot.default_worktree_with_prompt.is_none() {
            self.notify(
                "No worktree prompt template configured (set default_worktree_with_prompt)",
            );
            return;
        }
        if let Some(_proj) = self.snapshot.projects.get(self.active_project)
            && matches!(
                self.selection_section.get(self.active_project),
                Some(SelectionSection::Worktrees)
            )
        {
            self.popup = PopupState::CreateWorktreeWithPrompt {
                branch_input: String::new(),
                prompt_input: String::new(),
                focused_field: 0,
            };
        }
    }

    fn open_remove_popup(&mut self) {
        if let Some(proj) = self.snapshot.projects.get(self.active_project)
            && matches!(
                self.selection_section.get(self.active_project),
                Some(SelectionSection::Worktrees)
            )
            && let Some(wt) = proj.cached_worktrees.get(
                *self
                    .worktree_selected
                    .get(self.active_project)
                    .unwrap_or(&0),
            )
        {
            if wt.is_main {
                self.notify("Cannot remove main worktree");
                return;
            }
            if let Some(ref branch) = wt.branch {
                // Look up the linked tmux pane NOW, while the worktree still exists on
                // disk. canonicalize (used inside find_window_for_path) requires the path
                // to exist, and by the time ActionResult arrives the directory is gone.
                let linked_pane_id = wt.path.as_deref().and_then(tmux::find_window_for_path);
                self.popup = PopupState::ConfirmRemove {
                    branch: branch.clone(),
                    linked_pane_id,
                };
            }
        }
    }

    fn open_merge_popup(&mut self) {
        if let Some(proj) = self.snapshot.projects.get(self.active_project)
            && matches!(
                self.selection_section.get(self.active_project),
                Some(SelectionSection::Worktrees)
            )
            && let Some(wt) = proj.cached_worktrees.get(
                *self
                    .worktree_selected
                    .get(self.active_project)
                    .unwrap_or(&0),
            )
        {
            if wt.is_main {
                self.notify("Cannot merge main worktree");
                return;
            }
            if let (Some(branch), Some(path)) = (&wt.branch, &wt.path) {
                self.popup = PopupState::ConfirmMerge {
                    branch: branch.clone(),
                    worktree_path: path.clone(),
                };
            }
        }
    }

    fn popup_input_push(&mut self, ch: char) {
        if let PopupState::CreateWorktree { ref mut input } = self.popup {
            input.push(ch);
        }
    }

    fn popup_input_pop(&mut self) {
        if let PopupState::CreateWorktree { ref mut input } = self.popup {
            input.pop();
        }
    }

    fn popup_with_prompt_push(&mut self, ch: char) {
        if let PopupState::CreateWorktreeWithPrompt {
            ref mut branch_input,
            ref mut prompt_input,
            focused_field,
        } = self.popup
        {
            match focused_field {
                0 => branch_input.push(ch),
                _ => prompt_input.push(ch),
            }
        }
    }

    fn popup_with_prompt_pop(&mut self) {
        if let PopupState::CreateWorktreeWithPrompt {
            ref mut branch_input,
            ref mut prompt_input,
            focused_field,
        } = self.popup
        {
            match focused_field {
                0 => {
                    branch_input.pop();
                }
                _ => {
                    prompt_input.pop();
                }
            }
        }
    }

    fn popup_with_prompt_toggle_field(&mut self) {
        if let PopupState::CreateWorktreeWithPrompt {
            ref mut focused_field,
            ..
        } = self.popup
        {
            *focused_field = if *focused_field == 0 { 1 } else { 0 };
        }
    }

    fn close_popup(&mut self) {
        self.popup = PopupState::None;
    }

    fn open_agent_actions(&mut self) {
        let proj = match self.snapshot.projects.get(self.active_project) {
            Some(p) => p,
            None => {
                self.notify("No active project");
                return;
            }
        };

        let wt_idx = *self
            .worktree_selected
            .get(self.active_project)
            .unwrap_or(&0);
        let wt = match proj.cached_worktrees.get(wt_idx) {
            Some(wt) => wt,
            None => {
                self.notify("No worktree selected");
                return;
            }
        };

        let wt_path = match &wt.path {
            Some(p) => p.clone(),
            None => {
                self.notify("Worktree has no path");
                return;
            }
        };

        let canonical_wt = std::fs::canonicalize(&wt_path).ok();
        let pane = self.snapshot.panes.iter().find(|p| {
            canonical_wt
                .as_ref()
                .and_then(|cwt| {
                    std::fs::canonicalize(&p.pane_path)
                        .ok()
                        .map(|cp| cp == *cwt)
                })
                .unwrap_or(false)
        });

        let pane = match pane {
            Some(p) => p,
            None => {
                self.notify("No agent instance for this worktree");
                return;
            }
        };

        let session_id = match &pane.db_session_id {
            Some(id) => id.clone(),
            None => {
                self.notify("No active agent session");
                return;
            }
        };

        self.popup = PopupState::AgentActions {
            selected: 0,
            pane_pid: pane.pane_pid,
            session_id,
            worktree_branch: wt.branch.clone(),
        };
    }

    fn open_project_filter(&mut self) {
        if self.snapshot.projects.len() < 2 {
            return;
        }
        let mut all: Vec<(usize, String)> = self
            .snapshot
            .projects
            .iter()
            .enumerate()
            .map(|(i, p)| (i, p.name.clone()))
            .collect();
        all.sort_by_key(|(_, name)| name.to_lowercase());
        self.popup = PopupState::ProjectFilter {
            input: String::new(),
            filtered: all,
            selected: 0,
        };
    }

    fn recompute_project_filter(&mut self) {
        if let PopupState::ProjectFilter {
            input,
            filtered,
            selected,
        } = &mut self.popup
        {
            let projects: Vec<(usize, &str)> = self
                .snapshot
                .projects
                .iter()
                .enumerate()
                .map(|(i, p)| (i, p.name.as_str()))
                .collect();

            if input.is_empty() {
                let mut all: Vec<(usize, String)> =
                    projects.iter().map(|(i, n)| (*i, n.to_string())).collect();
                all.sort_by_key(|(_, name)| name.to_lowercase());
                *filtered = all;
            } else {
                use nucleo_matcher::Matcher;
                use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};

                let mut matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);
                let pattern = Pattern::parse(input, CaseMatching::Ignore, Normalization::Smart);
                let names: Vec<&str> = projects.iter().map(|(_, n)| *n).collect();
                let matches = pattern.match_list(names, &mut matcher);

                *filtered = matches
                    .into_iter()
                    .filter_map(|(name, _score)| {
                        projects
                            .iter()
                            .find(|(_, n)| *n == name)
                            .map(|(i, _)| (*i, name.to_string()))
                    })
                    .collect();
            }

            if *selected >= filtered.len() {
                *selected = filtered.len().saturating_sub(1);
            }
        }
    }

    /// Flatten all worktrees across all projects into
    /// ((project_idx, worktree_idx), "project/branch") entries.
    fn worktree_search_entries(&self) -> Vec<((usize, usize), String)> {
        self.snapshot
            .projects
            .iter()
            .enumerate()
            .flat_map(|(pi, proj)| {
                proj.cached_worktrees
                    .iter()
                    .enumerate()
                    .map(move |(wi, wt)| {
                        let branch = wt.branch.as_deref().unwrap_or("(detached)");
                        ((pi, wi), format!("{}/{}", proj.name, branch))
                    })
            })
            .collect()
    }

    fn open_worktree_search(&mut self) {
        let entries = self.worktree_search_entries();
        if entries.is_empty() {
            self.notify("No worktrees found");
            return;
        }
        self.popup = PopupState::WorktreeSearch {
            input: String::new(),
            filtered: entries.into_iter().map(|(idx, _)| idx).collect(),
            selected: 0,
        };
    }

    fn recompute_worktree_search(&mut self) {
        let entries = self.worktree_search_entries();
        if let PopupState::WorktreeSearch {
            input,
            filtered,
            selected,
        } = &mut self.popup
        {
            *filtered = fuzzy_filter(&entries, input);
            if *selected >= filtered.len() {
                *selected = filtered.len().saturating_sub(1);
            }
        }
    }

    /// Flatten every agent pane into (pane_idx, haystack) entries. The haystack
    /// includes the tmux session name and the pane directory so a session can be
    /// found by title, by tmux session, or by worktree name.
    fn session_search_entries(&self) -> Vec<(usize, String)> {
        self.snapshot
            .panes
            .iter()
            .enumerate()
            .map(|(i, pane)| {
                let dir = std::path::Path::new(&pane.pane_path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                (
                    i,
                    format!("{}/{} {}", pane.session_name, pane.display_title(), dir),
                )
            })
            .collect()
    }

    fn open_session_search(&mut self) {
        let entries = self.session_search_entries();
        if entries.is_empty() {
            self.notify("No agent sessions found");
            return;
        }
        self.popup = PopupState::SessionSearch {
            input: String::new(),
            filtered: entries.into_iter().map(|(i, _)| i).collect(),
            selected: 0,
        };
    }

    /// Recompute after the user edits the query. The highlight keeps its
    /// position in the list, as in the worktree search.
    fn recompute_session_search(&mut self) {
        self.refilter_session_search(None);
    }

    /// The pane the highlight currently points at. Panes churn every 2s, so a
    /// snapshot refresh re-anchors on this rather than on the cursor position —
    /// otherwise a session exiting above the cursor silently retargets Enter.
    ///
    /// Must be called before the new snapshot is installed: `filtered` indexes
    /// into the pane list the popup was last rendered against.
    fn session_search_anchor(&self) -> Option<String> {
        let PopupState::SessionSearch {
            filtered, selected, ..
        } = &self.popup
        else {
            return None;
        };
        filtered
            .get(*selected)
            .and_then(|i| self.snapshot.panes.get(*i))
            .map(|p| p.pane_id.clone())
    }

    fn refilter_session_search(&mut self, anchor: Option<String>) {
        let entries = self.session_search_entries();
        let panes = &self.snapshot.panes;
        if let PopupState::SessionSearch {
            input,
            filtered,
            selected,
        } = &mut self.popup
        {
            *filtered = fuzzy_filter(&entries, input);
            let reanchored = anchor.and_then(|id| {
                filtered
                    .iter()
                    .position(|i| panes.get(*i).is_some_and(|p| p.pane_id == id))
            });
            *selected = reanchored
                .unwrap_or(*selected)
                .min(filtered.len().saturating_sub(1));
        }
    }

    /// Drop the highlighted row after its pane is killed, so the list updates
    /// immediately rather than waiting up to 2s for the next snapshot.
    fn drop_session_search_row(&mut self) {
        if let PopupState::SessionSearch {
            filtered, selected, ..
        } = &mut self.popup
            && *selected < filtered.len()
        {
            filtered.remove(*selected);
            *selected = (*selected).min(filtered.len().saturating_sub(1));
        }
    }

    /// Point the dashboard at whichever card owns `pane`: the MR whose linked
    /// pane it is, else the worktree living at its path.
    fn focus_pane_in_dashboard(&mut self, pane_idx: usize) {
        let Some(pane) = self.snapshot.panes.get(pane_idx) else {
            return;
        };
        let Some((project_idx, section, idx)) = locate_pane(&self.snapshot, pane) else {
            return;
        };
        self.active_project = project_idx;
        if let Some(proj) = self.snapshot.projects.get(project_idx) {
            save_last_project(&proj.name);
        }
        if let Some(slot) = self.selection_section.get_mut(project_idx) {
            *slot = section;
        }
        let selection = match section {
            SelectionSection::MergeRequests => self.mr_selected.get_mut(project_idx),
            SelectionSection::Worktrees => self.worktree_selected.get_mut(project_idx),
        };
        if let Some(slot) = selection {
            *slot = idx;
        }
    }
}

/// Resolve which project card a pane belongs to. An MR link (by pane id) wins
/// over a path match, since the MR card carries strictly more context.
fn locate_pane(
    snapshot: &DashboardSnapshot,
    pane: &AgentPane,
) -> Option<(usize, SelectionSection, usize)> {
    for (pi, proj) in snapshot.projects.iter().enumerate() {
        if let Some(mi) = proj.dashboard.linked_mrs.iter().position(|mr| {
            mr.tmux_pane
                .as_ref()
                .is_some_and(|p| p.pane_id == pane.pane_id)
        }) {
            return Some((pi, SelectionSection::MergeRequests, mi));
        }
    }

    let pane_path = pane
        .canonical_path
        .clone()
        .map(std::path::PathBuf::from)
        .or_else(|| std::fs::canonicalize(&pane.pane_path).ok())?;

    snapshot.projects.iter().enumerate().find_map(|(pi, proj)| {
        proj.cached_worktrees
            .iter()
            .position(|wt| {
                wt.path
                    .as_deref()
                    .and_then(|p| std::fs::canonicalize(p).ok())
                    .is_some_and(|p| p == pane_path)
            })
            .map(|wi| (pi, SelectionSection::Worktrees, wi))
    })
}

/// Fuzzy-filter `(payload, label)` entries by label, ranked by match score.
/// An empty input returns all payloads in original order.
fn fuzzy_filter<T: Copy>(entries: &[(T, String)], input: &str) -> Vec<T> {
    if input.is_empty() {
        return entries.iter().map(|(payload, _)| *payload).collect();
    }

    use nucleo_matcher::Matcher;
    use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};

    let mut matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);
    let pattern = Pattern::parse(input, CaseMatching::Ignore, Normalization::Smart);

    let mut buf = Vec::new();
    let mut scored: Vec<(T, u32)> = entries
        .iter()
        .filter_map(|(payload, label)| {
            let haystack = nucleo_matcher::Utf32Str::new(label, &mut buf);
            pattern
                .score(haystack, &mut matcher)
                .map(|score| (*payload, score))
        })
        .collect();
    scored.sort_by_key(|&(_, score)| std::cmp::Reverse(score));
    scored.into_iter().map(|(payload, _)| payload).collect()
}

pub async fn run() -> Result<()> {
    let sock_path = daemon::socket_path();
    let stream = match UnixStream::connect(&sock_path).await {
        Ok(s) => s,
        Err(_) => {
            show_connection_error(&sock_path);
            return Ok(());
        }
    };
    let mut framed = Framed::new(stream, LengthDelimitedCodec::new());

    let handshake = ClientMsg::Handshake {
        version: PROTOCOL_VERSION,
    };
    framed
        .send(Bytes::from(serde_json::to_vec(&handshake)?))
        .await?;

    let initial_snapshot = wait_for_initial_snapshot(&mut framed).await?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut state = ClientState::from_snapshot(initial_snapshot);
    let result = run_client_loop(&mut terminal, &mut state, &mut framed).await;

    if let Some(proj) = state.snapshot.projects.get(state.active_project) {
        save_last_project(&proj.name);
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    result
}

pub async fn stop() -> Result<()> {
    let sock_path = daemon::socket_path();
    let stream = UnixStream::connect(&sock_path)
        .await
        .map_err(|_| anyhow::anyhow!("no daemon running at {}", sock_path.display()))?;
    let mut framed = Framed::new(stream, LengthDelimitedCodec::new());

    // Drain the initial snapshot the daemon sends on connect,
    // otherwise our Stop message is never read.
    let _ = framed.next().await;

    let msg = ClientMsg::Stop;
    framed.send(Bytes::from(serde_json::to_vec(&msg)?)).await?;

    // Drop the connection so the daemon's client-handler task can exit cleanly.
    drop(framed);

    // Wait up to 5 s for the daemon to remove its socket file before declaring success.
    // Without this there is a race: the caller may try `pertmux serve` before the daemon
    // has processed the Stop command and run its DaemonShutdown cleanup.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while sock_path.exists() {
        if std::time::Instant::now() >= deadline {
            eprintln!("warning: daemon did not shut down within 5 s");
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    crate::banner::print();
    println!("  {GRAY}daemon stopped{RESET}");
    println!();
    Ok(())
}

/// Returns the path to the most recent pertmux daemon log file, if any.
fn latest_log_path() -> Option<std::path::PathBuf> {
    let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir("/tmp")
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let s = name.to_string_lossy();
            s.starts_with("pertmux-daemon-") && s.ends_with(".log")
        })
        .map(|e| e.path())
        .collect();
    entries.sort();
    entries.pop()
}

pub fn status() {
    let sock_path = daemon::socket_path();
    let log = latest_log_path()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/tmp/pertmux-daemon-*.log".to_string());

    crate::banner::print();

    if !sock_path.exists() {
        println!("  {GRAY}daemon{RESET}  {GRAY}○{RESET}  not running  {DIM}(no socket){RESET}");
        println!("  {GRAY}socket{RESET}  {DIM}{}{RESET}", sock_path.display());
        println!("  {GRAY}log   {RESET}  {DIM}{}{RESET}", log);
        println!();
        println!("  {DIM}start with{RESET}  {ORANGE}pertmux serve{RESET}");
    } else {
        let probe = std::os::unix::net::UnixStream::connect(&sock_path);
        match probe {
            Ok(_) => {
                println!("  {GRAY}daemon{RESET}  {GREEN}●{RESET}  {WHITE}running{RESET}");
            }
            Err(_) => {
                println!(
                    "  {GRAY}daemon{RESET}  {GRAY}◐{RESET}  stale socket  {DIM}(not responding){RESET}"
                );
                println!();
                println!("  {DIM}clean up with{RESET}  {ORANGE}pertmux cleanup{RESET}");
            }
        }
        println!("  {GRAY}socket{RESET}  {}", sock_path.display());
        println!("  {GRAY}log   {RESET}  {DIM}{}{RESET}", log);
    }
    println!();
}

pub fn cleanup() -> anyhow::Result<()> {
    crate::banner::print();

    let sock_path = daemon::socket_path();
    if sock_path.exists() {
        let is_stale = std::os::unix::net::UnixStream::connect(&sock_path).is_err();
        if is_stale {
            std::fs::remove_file(&sock_path)?;
            println!(
                "  {GREEN}✓{RESET}  stale socket removed  {DIM}{}{RESET}",
                sock_path.display()
            );
        } else {
            println!("  {GRAY}─{RESET}  socket is live (daemon running), skipping");
        }
    } else {
        println!("  {GRAY}─{RESET}  no socket found");
    }

    if let Some(data_dir) = dirs::data_dir() {
        let pertmux_dir = data_dir.join("pertmux");

        let read_state_path = pertmux_dir.join("read_state.db");
        if read_state_path.exists() {
            std::fs::remove_file(&read_state_path)?;
            println!(
                "  {GREEN}✓{RESET}  read state removed    {DIM}{}{RESET}",
                read_state_path.display()
            );
        }

        let last_project_path = pertmux_dir.join("last_project");
        if last_project_path.exists() {
            std::fs::remove_file(&last_project_path)?;
            println!(
                "  {GREEN}✓{RESET}  last project removed  {DIM}{}{RESET}",
                last_project_path.display()
            );
        }
    }

    println!();
    println!("  {GRAY}done{RESET}");
    println!();
    Ok(())
}

async fn wait_for_initial_snapshot(
    framed: &mut Framed<UnixStream, LengthDelimitedCodec>,
) -> Result<DashboardSnapshot> {
    while let Some(frame) = framed.next().await {
        let bytes = frame?;
        let msg: DaemonMsg = serde_json::from_slice(&bytes)?;
        match msg {
            DaemonMsg::Snapshot(snap) => return Ok(*snap),
            DaemonMsg::HandshakeAck { .. } => {}
            DaemonMsg::Progress(_) => {} // ignore progress during initial connect
            DaemonMsg::ActionResult { ok, message } => {
                if !ok {
                    anyhow::bail!(message);
                }
            }
        }
    }
    anyhow::bail!("daemon disconnected before initial snapshot")
}

async fn run_client_loop<B>(
    terminal: &mut Terminal<B>,
    state: &mut ClientState,
    framed: &mut Framed<UnixStream, LengthDelimitedCodec>,
) -> Result<()>
where
    B: Backend,
    B::Error: Send + Sync + 'static,
{
    let mut event_stream = EventStream::new();

    while state.running {
        terminal.draw(|frame| ui::draw_client(frame, state))?;

        tokio::select! {
            maybe_event = event_stream.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        handle_key(state, framed, key.code).await?;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) => {}
                    None => break,
                }
            }
            msg = framed.next() => {
                match msg {
                    Some(Ok(bytes)) => {
                        let daemon_msg: DaemonMsg = serde_json::from_slice(&bytes)?;
                        match daemon_msg {
                            DaemonMsg::Snapshot(snap) => {
                                state.update_snapshot(*snap);
                            }
                            DaemonMsg::Progress(steps) => {
                                state.refresh_steps = steps;
                            }
                            DaemonMsg::ActionResult { ok, message } => {
                                state.notify(message);
                                if ok {
                                    // If a CreateWorktreeWithPrompt was submitted, move the
                                    // pending open request so it fires on the next snapshot.
                                    if let Some(pending) =
                                        state.pending_create_with_prompt.take()
                                    {
                                        state.pending_open_worktree = Some(pending);
                                    }

                                    // After a successful worktree removal, offer to kill
                                    // the linked tmux window using the pane_id that was
                                    // captured when the popup was opened (before deletion).
                                    if let PopupState::ConfirmRemove {
                                        branch,
                                        linked_pane_id: Some(pane_id),
                                    } = &state.popup
                                    {
                                        let branch = branch.clone();
                                        let pane_id = pane_id.clone();
                                        state.popup =
                                            PopupState::ConfirmKillTmuxWindow { branch, pane_id };
                                    } else {
                                        state.popup = PopupState::None;
                                    }
                                } else {
                                    // Creation failed — discard any pending open request.
                                    state.pending_create_with_prompt = None;
                                }
                            }
                            DaemonMsg::HandshakeAck { .. } => {}
                        }
                    }
                    Some(Err(_)) => break,
                    None => {
                        anyhow::bail!("daemon disconnected");
                    }
                }
            }
        }
    }

    Ok(())
}

/// Whether a focus-mode key was consumed or should fall through to the global
/// dispatcher (Tab, Esc, q, configurable chars, etc.).
enum KeyOutcome {
    Handled,
    Fallthrough,
}

/// Handle keys while the Projects overview pane has focus.
///
/// Consumes navigation (j/k/h/l), the sort-apply binding (Space), and Enter
/// (select project). Returns `Fallthrough` for anything else so the global
/// dispatcher still sees Tab/Esc/q/etc.
async fn handle_project_focus_key(
    state: &mut ClientState,
    framed: &mut Framed<UnixStream, LengthDelimitedCodec>,
    code: KeyCode,
) -> Result<KeyOutcome> {
    match code {
        KeyCode::Char('j') | KeyCode::Down => {
            let len = state.snapshot.projects.len();
            if len > 0 && state.project_cursor_row + 1 < len {
                state.project_cursor_row += 1;
                state.ensure_cursor_visible();
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if state.project_cursor_row > 0 {
                state.project_cursor_row -= 1;
                state.ensure_cursor_visible();
            }
        }
        KeyCode::Char('l') | KeyCode::Right => {
            state.project_cursor_col = state.project_cursor_col.next_col();
        }
        KeyCode::Char('h') | KeyCode::Left => {
            state.project_cursor_col = state.project_cursor_col.prev_col();
        }
        KeyCode::Char(' ') => {
            if state.project_cursor_col == state.project_sort_col {
                state.project_sort_desc = !state.project_sort_desc;
            } else {
                state.project_sort_col = state.project_cursor_col;
                state.project_sort_desc = state.project_cursor_col.default_desc();
            }
            save_last_sort(state.project_sort_col, state.project_sort_desc);
            let arrow = if state.project_sort_desc {
                "↓"
            } else {
                "↑"
            };
            state.notify(format!(
                "Sort: {} {}",
                arrow,
                state.project_sort_col.label()
            ));
        }
        KeyCode::Enter => {
            let order = state.sorted_project_canonical_indices();
            if let Some(&canonical) = order.get(state.project_cursor_row) {
                state.active_project = canonical;
                if let Some(proj) = state.snapshot.projects.get(canonical) {
                    save_last_project(&proj.name);
                }
                state.project_focused = false;
                if let Some(section) = state.selection_section.get_mut(canonical) {
                    *section = SelectionSection::MergeRequests;
                }
                if let Some(mr_iid) = state.current_mr_iid() {
                    send_msg(
                        framed,
                        ClientMsg::SelectMr {
                            project_idx: canonical,
                            mr_iid,
                        },
                    )
                    .await?;
                }
            }
        }
        _ => return Ok(KeyOutcome::Fallthrough),
    }
    Ok(KeyOutcome::Handled)
}

async fn handle_key(
    state: &mut ClientState,
    framed: &mut Framed<UnixStream, LengthDelimitedCodec>,
    code: KeyCode,
) -> Result<()> {
    if matches!(state.popup, PopupState::ProjectFilter { .. }) {
        match code {
            KeyCode::Esc => state.close_popup(),
            KeyCode::Enter => {
                if let PopupState::ProjectFilter {
                    filtered, selected, ..
                } = &state.popup
                    && let Some(&(idx, _)) = filtered.get(*selected)
                {
                    state.active_project = idx;
                    if let Some(proj) = state.snapshot.projects.get(idx) {
                        save_last_project(&proj.name);
                    }
                }
                state.close_popup();
                if let Some(mr_iid) = state.current_mr_iid() {
                    send_msg(
                        framed,
                        ClientMsg::SelectMr {
                            project_idx: state.active_project,
                            mr_iid,
                        },
                    )
                    .await?;
                }
            }
            KeyCode::Down => {
                if let PopupState::ProjectFilter {
                    filtered, selected, ..
                } = &mut state.popup
                    && *selected + 1 < filtered.len()
                {
                    *selected += 1;
                }
            }
            KeyCode::Up => {
                if let PopupState::ProjectFilter { selected, .. } = &mut state.popup
                    && *selected > 0
                {
                    *selected -= 1;
                }
            }
            KeyCode::Backspace => {
                if let PopupState::ProjectFilter { input, .. } = &mut state.popup {
                    input.pop();
                }
                state.recompute_project_filter();
            }
            KeyCode::Char(ch) => {
                if let PopupState::ProjectFilter { input, .. } = &mut state.popup {
                    input.push(ch);
                }
                state.recompute_project_filter();
            }
            _ => {}
        }
        return Ok(());
    }

    if matches!(state.popup, PopupState::WorktreeSearch { .. }) {
        match code {
            KeyCode::Esc => state.close_popup(),
            KeyCode::Enter => {
                let target = if let PopupState::WorktreeSearch {
                    filtered, selected, ..
                } = &state.popup
                {
                    filtered.get(*selected).copied()
                } else {
                    None
                };
                state.close_popup();
                if let Some((pi, wi)) = target
                    && let Some(proj) = state.snapshot.projects.get(pi)
                {
                    state.active_project = pi;
                    save_last_project(&proj.name);
                    if let Some(section) = state.selection_section.get_mut(pi) {
                        *section = SelectionSection::Worktrees;
                    }
                    if let Some(sel) = state.worktree_selected.get_mut(pi) {
                        *sel = wi.min(proj.cached_worktrees.len().saturating_sub(1));
                    }
                    if let Some(wt) = proj.cached_worktrees.get(wi)
                        && let Some(ref path) = wt.path
                        && let Err(e) = tmux::find_or_create_pane(
                            path,
                            &proj.name,
                            state.snapshot.default_agent_command.as_deref(),
                        )
                    {
                        state.notify(format!("Focus failed: {}", e));
                    }
                }
            }
            KeyCode::Down => {
                if let PopupState::WorktreeSearch {
                    filtered, selected, ..
                } = &mut state.popup
                    && *selected + 1 < filtered.len()
                {
                    *selected += 1;
                }
            }
            KeyCode::Up => {
                if let PopupState::WorktreeSearch { selected, .. } = &mut state.popup
                    && *selected > 0
                {
                    *selected -= 1;
                }
            }
            KeyCode::Backspace => {
                if let PopupState::WorktreeSearch { input, .. } = &mut state.popup {
                    input.pop();
                }
                state.recompute_worktree_search();
            }
            KeyCode::Char(ch) => {
                if let PopupState::WorktreeSearch { input, .. } = &mut state.popup {
                    input.push(ch);
                }
                state.recompute_worktree_search();
            }
            _ => {}
        }
        return Ok(());
    }

    if matches!(state.popup, PopupState::SessionSearch { .. }) {
        match code {
            KeyCode::Esc => state.close_popup(),
            KeyCode::Enter => {
                let target = if let PopupState::SessionSearch {
                    filtered, selected, ..
                } = &state.popup
                {
                    filtered.get(*selected).copied()
                } else {
                    None
                };
                state.close_popup();
                if let Some(idx) = target {
                    let pane_id = state.snapshot.panes.get(idx).map(|p| p.pane_id.clone());
                    let before = state.current_mr_iid();
                    state.focus_pane_in_dashboard(idx);
                    maybe_send_select_mr(state, framed, before).await?;
                    if let Some(pane_id) = pane_id
                        && let Err(e) = tmux::switch_to_pane(&pane_id)
                    {
                        state.notify(format!("Focus failed: {}", e));
                    }
                }
            }
            KeyCode::Down => {
                if let PopupState::SessionSearch {
                    filtered, selected, ..
                } = &mut state.popup
                    && *selected + 1 < filtered.len()
                {
                    *selected += 1;
                }
            }
            KeyCode::Up => {
                if let PopupState::SessionSearch { selected, .. } = &mut state.popup
                    && *selected > 0
                {
                    *selected -= 1;
                }
            }
            KeyCode::Delete => {
                let target = if let PopupState::SessionSearch {
                    filtered, selected, ..
                } = &state.popup
                {
                    filtered
                        .get(*selected)
                        .and_then(|i| state.snapshot.panes.get(*i))
                        .map(|p| (p.pane_id.clone(), p.display_title().to_string()))
                } else {
                    None
                };
                if let Some((pane_id, title)) = target {
                    match tmux::kill_pane(&pane_id) {
                        Ok(()) => {
                            state.drop_session_search_row();
                            state.notify(format!("Killed {}", truncate(&title, 40)));
                        }
                        Err(e) => state.notify(format!("Kill failed: {}", e)),
                    }
                }
            }
            KeyCode::Backspace => {
                if let PopupState::SessionSearch { input, .. } = &mut state.popup {
                    input.pop();
                }
                state.recompute_session_search();
            }
            KeyCode::Char(ch) => {
                if let PopupState::SessionSearch { input, .. } = &mut state.popup {
                    input.push(ch);
                }
                state.recompute_session_search();
            }
            _ => {}
        }
        return Ok(());
    }

    if matches!(state.popup, PopupState::ChangeSummary { .. }) {
        match code {
            KeyCode::Esc => state.close_popup(),
            KeyCode::Up | KeyCode::Char('k') => {
                if let PopupState::ChangeSummary { selected, .. } = &mut state.popup
                    && *selected > 0
                {
                    *selected -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let PopupState::ChangeSummary {
                    changes, selected, ..
                } = &mut state.popup
                    && *selected + 1 < changes.len()
                {
                    *selected += 1;
                }
            }
            KeyCode::Enter => {
                if let PopupState::ChangeSummary { changes, selected } =
                    std::mem::replace(&mut state.popup, PopupState::None)
                    && let Some(change) = changes.get(selected)
                    && let Some(idx) = state
                        .snapshot
                        .projects
                        .iter()
                        .position(|p| p.name == change.project_name)
                {
                    state.active_project = idx;
                    if let Some(proj) = state.snapshot.projects.get(idx) {
                        save_last_project(&proj.name);
                        if let Some(mr_idx) = proj
                            .dashboard
                            .linked_mrs
                            .iter()
                            .position(|l| l.mr.iid == change.mr_iid)
                        {
                            state.mr_selected[idx] = mr_idx;
                            state.selection_section[idx] = SelectionSection::MergeRequests;
                            send_msg(
                                framed,
                                ClientMsg::SelectMr {
                                    project_idx: idx,
                                    mr_iid: change.mr_iid,
                                },
                            )
                            .await?;
                        }
                    }
                }
            }
            _ => {}
        }
        return Ok(());
    }

    if matches!(state.popup, PopupState::MrOverview { .. }) {
        match code {
            KeyCode::Esc => state.close_popup(),
            KeyCode::Up | KeyCode::Char('k') => {
                if let PopupState::MrOverview { selected } = &mut state.popup
                    && *selected > 0
                {
                    *selected -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let PopupState::MrOverview { selected } = &mut state.popup
                    && *selected + 1 < state.snapshot.global_mrs.len()
                {
                    *selected += 1;
                }
            }
            KeyCode::Enter => {
                if let PopupState::MrOverview { selected } =
                    std::mem::replace(&mut state.popup, PopupState::None)
                    && let Some(entry) = state.snapshot.global_mrs.get(selected)
                {
                    if let Some(ref proj_name) = entry.configured_project {
                        // Navigate to the configured project
                        if let Some(idx) = state
                            .snapshot
                            .projects
                            .iter()
                            .position(|p| &p.name == proj_name)
                        {
                            state.active_project = idx;
                            save_last_project(proj_name);
                            // Select MR section
                            if let Some(section) = state.selection_section.get_mut(idx) {
                                *section = SelectionSection::MergeRequests;
                            }
                            // Try to find the MR in linked_mrs and select it
                            let iid = entry.mr.iid;
                            if let Some(proj) = state.snapshot.projects.get(idx)
                                && let Some(mr_idx) = proj
                                    .dashboard
                                    .linked_mrs
                                    .iter()
                                    .position(|l| l.mr.iid == iid)
                            {
                                state.mr_selected[idx] = mr_idx;
                                send_msg(
                                    framed,
                                    ClientMsg::SelectMr {
                                        project_idx: idx,
                                        mr_iid: iid,
                                    },
                                )
                                .await?;
                            }
                        }
                    } else {
                        // Not a configured project — open in browser
                        open_url_in_browser(&entry.mr.web_url);
                        state.notify("Opened in browser (project not configured)");
                    }
                }
            }
            _ => {}
        }
        return Ok(());
    }

    if matches!(state.popup, PopupState::ActivityFeed { .. }) {
        match code {
            KeyCode::Esc => state.close_popup(),
            KeyCode::Up | KeyCode::Char('k') => {
                if let PopupState::ActivityFeed { selected } = &mut state.popup
                    && *selected > 0
                {
                    *selected -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let PopupState::ActivityFeed { selected } = &mut state.popup
                    && *selected + 1 < state.snapshot.activity_feed.len()
                {
                    *selected += 1;
                }
            }
            KeyCode::Enter => {
                if let PopupState::ActivityFeed { selected } =
                    std::mem::replace(&mut state.popup, PopupState::None)
                    && let Some(entry) = state.snapshot.activity_feed.get(selected).cloned()
                {
                    navigate_to_activity(state, framed, &entry).await?;
                }
            }
            _ => {}
        }
        return Ok(());
    }

    if matches!(state.popup, PopupState::AgentActions { .. }) {
        match code {
            KeyCode::Esc => state.close_popup(),
            KeyCode::Up | KeyCode::Char('k') => {
                if let PopupState::AgentActions { selected, .. } = &mut state.popup
                    && *selected > 0
                {
                    *selected -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let PopupState::AgentActions { selected, .. } = &mut state.popup
                    && *selected + 1 < state.snapshot.agent_actions.len()
                {
                    *selected += 1;
                }
            }
            KeyCode::Enter => {
                if let Some(msg) = build_agent_action_msg(state) {
                    state.notify("Sending to agent...");
                    send_msg(framed, msg).await?;
                }
                state.close_popup();
            }
            _ => {}
        }
        return Ok(());
    }

    if matches!(state.popup, PopupState::ConfirmKillTmuxWindow { .. }) {
        match code {
            KeyCode::Esc => state.close_popup(),
            KeyCode::Enter => {
                let pane_id =
                    if let PopupState::ConfirmKillTmuxWindow { ref pane_id, .. } = state.popup {
                        Some(pane_id.clone())
                    } else {
                        None
                    };
                if let Some(pane_id) = pane_id {
                    if let Err(e) = tmux::kill_window(&pane_id) {
                        state.notify(format!("Kill window failed: {}", e));
                    } else {
                        state.notify("Tmux window closed");
                    }
                }
                state.close_popup();
            }
            _ => {}
        }
        return Ok(());
    }

    if matches!(state.popup, PopupState::CreateWorktreeWithPrompt { .. }) {
        match code {
            KeyCode::Esc => state.close_popup(),
            KeyCode::Tab => state.popup_with_prompt_toggle_field(),
            KeyCode::Backspace => state.popup_with_prompt_pop(),
            KeyCode::Enter => {
                if let PopupState::CreateWorktreeWithPrompt {
                    ref branch_input,
                    ref prompt_input,
                    ..
                } = state.popup
                {
                    let branch = branch_input.trim().to_string();
                    let prompt = prompt_input.trim().to_string();
                    if !branch.is_empty() && !prompt.is_empty() {
                        // Build the filled command from the template.
                        let command = state
                            .snapshot
                            .default_worktree_with_prompt
                            .as_deref()
                            .unwrap_or("")
                            .replace("{{msg}}", &prompt);

                        let project_idx = state.active_project;
                        let project_name = state
                            .snapshot
                            .projects
                            .get(project_idx)
                            .map(|p| p.name.clone())
                            .unwrap_or_default();

                        state.pending_create_with_prompt = Some(WorktreeOpenRequest {
                            project_idx,
                            branch: branch.clone(),
                            command,
                            project_name,
                        });

                        state.notify("Creating worktree...");
                        let msg = ClientMsg::CreateWorktreeWithPrompt {
                            project_idx,
                            branch,
                            prompt,
                        };
                        state.close_popup();
                        send_msg(framed, msg).await?;
                    }
                }
            }
            KeyCode::Char(ch) => state.popup_with_prompt_push(ch),
            _ => {}
        }
        return Ok(());
    }

    if state.has_popup() {
        match code {
            KeyCode::Esc => state.close_popup(),
            KeyCode::Enter => {
                if let Some(msg) = popup_action_msg(state) {
                    let toast = match &state.popup {
                        PopupState::CreateWorktree { .. } => "Creating worktree...",
                        PopupState::ConfirmRemove { .. } => "Removing worktree...",
                        PopupState::ConfirmMerge { .. } => "Merging worktree...",
                        _ => "",
                    };
                    if !toast.is_empty() {
                        state.notify(toast);
                    }
                    send_msg(framed, msg).await?;
                }
            }
            KeyCode::Backspace => state.popup_input_pop(),
            KeyCode::Char(ch) => {
                if matches!(state.popup, PopupState::CreateWorktree { .. }) {
                    state.popup_input_push(ch);
                }
            }
            _ => {}
        }
        return Ok(());
    }

    // Project focus mode: intercept j/k/h/l/Space/Enter before global handler.
    // q/Esc/Tab and other chars (r/f/etc.) fall through.
    if state.project_focused
        && matches!(
            handle_project_focus_key(state, framed, code).await?,
            KeyOutcome::Handled
        )
    {
        return Ok(());
    }

    match code {
        KeyCode::Char('q') | KeyCode::Esc => state.running = false,
        KeyCode::Up | KeyCode::Char('k') => {
            let before = state.current_mr_iid();
            state.move_up();
            maybe_send_select_mr(state, framed, before).await?;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let before = state.current_mr_iid();
            state.move_down();
            maybe_send_select_mr(state, framed, before).await?;
        }
        KeyCode::Tab => {
            let before = state.current_mr_iid();
            state.toggle_section();
            maybe_send_select_mr(state, framed, before).await?;
        }
        KeyCode::Enter => {
            if let Err(e) = focus_selected(state) {
                state.notify(format!("Focus failed: {}", e));
            }
        }
        KeyCode::Char(ch) => {
            let kb = &state.snapshot.keybindings;
            if ch == kb.refresh {
                state.notify("Refreshing...");
                send_msg(framed, ClientMsg::Refresh).await?;
            } else if ch == kb.open_browser {
                if state.has_projects() {
                    state.open_selected_mr_in_browser();
                }
            } else if ch == kb.copy_branch {
                if state.has_projects() {
                    state.copy_selected_branch();
                }
            } else if ch == kb.filter_projects {
                state.open_project_filter();
            } else if ch == kb.create_worktree {
                state.open_create_popup();
            } else if ch == kb.open_worktree_with_prompt {
                state.open_create_with_prompt_popup();
            } else if ch == kb.delete_worktree {
                state.open_remove_popup();
            } else if ch == kb.merge_worktree {
                state.open_merge_popup();
            } else if ch == kb.agent_actions {
                state.open_agent_actions();
            } else if ch == kb.mr_overview {
                state.open_mr_overview();
            } else if ch == kb.activity_feed {
                state.open_activity_feed();
            } else if ch == kb.worktree_search {
                state.open_worktree_search();
            } else if ch == kb.session_search {
                state.open_session_search();
            } else if ch == 'K' {
                state.open_keybindings_help();
            }
        }
        _ => {}
    }

    Ok(())
}

fn popup_action_msg(state: &ClientState) -> Option<ClientMsg> {
    let project_idx = state.active_project;
    match &state.popup {
        PopupState::CreateWorktree { input } => {
            let branch = input.trim().to_string();
            if branch.is_empty() {
                return None;
            }
            Some(ClientMsg::CreateWorktree {
                project_idx,
                branch,
            })
        }
        PopupState::ConfirmRemove { branch, .. } => Some(ClientMsg::RemoveWorktree {
            project_idx,
            branch: branch.clone(),
        }),
        PopupState::ConfirmMerge { worktree_path, .. } => Some(ClientMsg::MergeWorktree {
            project_idx,
            worktree_path: worktree_path.clone(),
        }),
        PopupState::ProjectFilter { .. }
        | PopupState::WorktreeSearch { .. }
        | PopupState::SessionSearch { .. }
        | PopupState::ChangeSummary { .. }
        | PopupState::AgentActions { .. }
        | PopupState::MrOverview { .. }
        | PopupState::ActivityFeed { .. }
        | PopupState::ConfirmKillTmuxWindow { .. }
        | PopupState::KeybindingsHelp
        // CreateWorktreeWithPrompt is handled in its own if block above — it never
        // reaches popup_action_msg.
        | PopupState::CreateWorktreeWithPrompt { .. }
        | PopupState::None => None,
    }
}

fn build_agent_action_msg(state: &ClientState) -> Option<ClientMsg> {
    let PopupState::AgentActions {
        selected,
        pane_pid,
        session_id,
        worktree_branch,
    } = &state.popup
    else {
        return None;
    };

    let action = state.snapshot.agent_actions.get(*selected)?;
    let proj = state.snapshot.projects.get(state.active_project)?;

    let linked_mr = worktree_branch.as_ref().and_then(|branch| {
        proj.dashboard
            .linked_mrs
            .iter()
            .find(|l| &l.mr.source_branch == branch)
    });

    // If the action requires an MR but none is linked, bail out
    if action.requires_mr && linked_mr.is_none() {
        return None;
    }

    let prompt = substitute_template(&action.prompt, linked_mr.map(|l| &l.mr), &proj.name);

    Some(ClientMsg::AgentAction {
        pane_pid: *pane_pid,
        session_id: session_id.clone(),
        prompt,
    })
}

fn substitute_template(
    template: &str,
    mr: Option<&crate::forge_clients::types::MergeRequestSummary>,
    project_name: &str,
) -> String {
    let mut result = template.to_string();
    result = result.replace("{project_name}", project_name);

    if let Some(mr) = mr {
        result = result.replace("{target_branch}", &mr.target_branch);
        result = result.replace("{source_branch}", &mr.source_branch);
        result = result.replace("{mr_url}", &mr.web_url);
        result = result.replace("{mr_iid}", &mr.iid.to_string());
    } else {
        // Provide sensible defaults when no MR is linked
        result = result.replace("{target_branch}", "main");
        result = result.replace("{source_branch}", "");
        result = result.replace("{mr_url}", "");
        result = result.replace("{mr_iid}", "");
    }

    result
}

async fn maybe_send_select_mr(
    state: &ClientState,
    framed: &mut Framed<UnixStream, LengthDelimitedCodec>,
    before: Option<u64>,
) -> Result<()> {
    let after = state.current_mr_iid();
    if after != before
        && let Some(mr_iid) = after
    {
        send_msg(
            framed,
            ClientMsg::SelectMr {
                project_idx: state.active_project,
                mr_iid,
            },
        )
        .await?;
    }
    Ok(())
}

fn focus_selected(state: &ClientState) -> Result<()> {
    if let Some(proj) = state.snapshot.projects.get(state.active_project) {
        match state
            .selection_section
            .get(state.active_project)
            .unwrap_or(&SelectionSection::Worktrees)
        {
            SelectionSection::MergeRequests => {
                if let Some(linked) = proj
                    .dashboard
                    .linked_mrs
                    .get(*state.mr_selected.get(state.active_project).unwrap_or(&0))
                    && let Some(pane) = linked.tmux_pane.as_ref()
                {
                    tmux::switch_to_pane(&pane.pane_id)?;
                }
            }
            SelectionSection::Worktrees => {
                if let Some(wt) = proj.cached_worktrees.get(
                    *state
                        .worktree_selected
                        .get(state.active_project)
                        .unwrap_or(&0),
                ) && let Some(ref path) = wt.path
                {
                    tmux::find_or_create_pane(
                        path,
                        &proj.name,
                        state.snapshot.default_agent_command.as_deref(),
                    )?;
                }
            }
        }
    } else if let Some(pane) = state.snapshot.panes.get(state.selected) {
        tmux::switch_to_pane(&pane.pane_id)?;
    }
    Ok(())
}

/// Navigate pertmux to the item referenced by an activity entry.
///
/// Agent activities switch the tmux client to the recorded pane.
/// MR activities select the matching project + MR in the main view.
async fn navigate_to_activity(
    state: &mut ClientState,
    framed: &mut Framed<UnixStream, LengthDelimitedCodec>,
    entry: &crate::protocol::ActivityEntry,
) -> Result<()> {
    use crate::protocol::ActivityTarget;
    match &entry.target {
        Some(ActivityTarget::Pane { pane_id, .. }) => {
            if let Err(e) = tmux::switch_to_pane(pane_id) {
                state.notify(format!("Pane no longer active: {}", e));
            }
        }
        Some(ActivityTarget::MergeRequest { project_name, iid }) => {
            if let Some(idx) = state
                .snapshot
                .projects
                .iter()
                .position(|p| &p.name == project_name)
            {
                state.active_project = idx;
                save_last_project(project_name);
                if let Some(section) = state.selection_section.get_mut(idx) {
                    *section = SelectionSection::MergeRequests;
                }
                let iid = *iid;
                if let Some(proj) = state.snapshot.projects.get(idx)
                    && let Some(mr_idx) = proj
                        .dashboard
                        .linked_mrs
                        .iter()
                        .position(|l| l.mr.iid == iid)
                {
                    state.mr_selected[idx] = mr_idx;
                    send_msg(
                        framed,
                        ClientMsg::SelectMr {
                            project_idx: idx,
                            mr_iid: iid,
                        },
                    )
                    .await?;
                }
            } else {
                state.notify(format!("Project '{}' not in config", project_name));
            }
        }
        None => {
            state.notify("No navigation target for this activity");
        }
    }
    Ok(())
}

async fn send_msg(
    framed: &mut Framed<UnixStream, LengthDelimitedCodec>,
    msg: ClientMsg,
) -> Result<()> {
    framed.send(Bytes::from(serde_json::to_vec(&msg)?)).await?;
    Ok(())
}

fn show_connection_error(sock_path: &std::path::Path) {
    use ratatui::layout::{Alignment, Constraint, Layout};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

    let _ = enable_raw_mode();
    let mut stdout = io::stdout();
    let _ = execute!(stdout, EnterAlternateScreen);
    let backend = CrosstermBackend::new(stdout);
    let Ok(mut terminal) = Terminal::new(backend) else {
        return;
    };

    let _ = terminal.draw(|frame| {
        let area = frame.area();
        let vertical = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(9),
            Constraint::Fill(1),
        ])
        .split(area);
        let horizontal = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(52),
            Constraint::Fill(1),
        ])
        .split(vertical[1]);
        let rect = horizontal[1];

        let accent = Color::Rgb(255, 140, 0);
        let block = Block::default()
            .title(Line::from(Span::styled(
                " pertmux ",
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            )))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(accent));

        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "daemon is not running",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("start with: ", Style::default().fg(Color::DarkGray)),
                Span::styled("pertmux serve", Style::default().fg(accent)),
            ]),
            Line::from(Span::styled(
                format!("socket:     {}", sock_path.display()),
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "press any key to close",
                Style::default().fg(Color::DarkGray),
            )),
        ];

        let paragraph = Paragraph::new(lines)
            .block(block)
            .alignment(Alignment::Center);
        frame.render_widget(paragraph, rect);
    });

    let _ = crossterm::event::read();

    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
}

#[cfg(test)]
mod tests {
    use super::{AgentPane, DashboardSnapshot, SelectionSection, fuzzy_filter, locate_pane};
    use crate::config::ProjectForge;
    use crate::forge_clients::types::{ForgeUser, MergeRequestSummary};
    use crate::git::WorktreeInfo;
    use crate::linking::{DashboardState, LinkedMergeRequest};
    use crate::protocol::ProjectSnapshot;
    use crate::types::PaneStatus;
    use crate::worktrunk::{WtCommit, WtWorktree};

    fn entries() -> Vec<((usize, usize), String)> {
        vec![
            ((0, 0), "pertmux/main".to_string()),
            ((0, 1), "pertmux/feat-search".to_string()),
            ((1, 0), "mainapi/main".to_string()),
            ((1, 1), "mainapi/fix-auth".to_string()),
        ]
    }

    #[test]
    fn empty_input_returns_all_in_order() {
        let result = fuzzy_filter(&entries(), "");
        assert_eq!(result, vec![(0, 0), (0, 1), (1, 0), (1, 1)]);
    }

    #[test]
    fn matches_branch_name() {
        let result = fuzzy_filter(&entries(), "search");
        assert_eq!(result, vec![(0, 1)]);
    }

    #[test]
    fn matches_project_name() {
        let result = fuzzy_filter(&entries(), "mainapi");
        assert_eq!(result, vec![(1, 0), (1, 1)]);
    }

    #[test]
    fn matches_project_slash_branch() {
        let result = fuzzy_filter(&entries(), "pertmux/main");
        assert!(result.contains(&(0, 0)));
        assert_eq!(result[0], (0, 0));
    }

    #[test]
    fn no_match_returns_empty() {
        let result: Vec<(usize, usize)> = fuzzy_filter(&entries(), "zzzqqq");
        assert!(result.is_empty());
    }
    fn pane(pane_id: &str, path: &str) -> AgentPane {
        AgentPane {
            pane_id: pane_id.to_string(),
            session_name: "sess".to_string(),
            window_index: 0,
            pane_index: 0,
            pane_title: "OC | work".to_string(),
            pane_path: path.to_string(),
            canonical_path: std::fs::canonicalize(path)
                .ok()
                .and_then(|p| p.to_str().map(String::from)),
            pane_pid: 1,
            pane_command: "opencode".to_string(),
            status: PaneStatus::Idle,
            db_session_title: Some("Work".to_string()),
            agent: Some("opencode".to_string()),
            model: None,
            last_activity: None,
            status_changed_at: None,
            db_session_id: None,
            last_response: None,
        }
    }

    fn worktree(path: &str, branch: &str) -> WtWorktree {
        WtWorktree {
            branch: Some(branch.to_string()),
            path: Some(path.to_string()),
            kind: "worktree".to_string(),
            commit: WtCommit {
                sha: "abc".to_string(),
                short_sha: "abc".to_string(),
                message: "m".to_string(),
                timestamp: 0,
            },
            working_tree: None,
            main_state: None,
            main: None,
            remote: None,
            worktree: None,
            is_main: false,
            is_current: false,
            is_previous: false,
            symbols: None,
        }
    }

    fn linked_mr(branch: &str, tmux_pane: Option<AgentPane>) -> LinkedMergeRequest {
        LinkedMergeRequest {
            mr: MergeRequestSummary {
                iid: 1,
                title: "feat: x".to_string(),
                state: "opened".to_string(),
                source_branch: branch.to_string(),
                target_branch: "main".to_string(),
                author: ForgeUser {
                    id: 1,
                    username: "u".to_string(),
                    name: "U".to_string(),
                },
                draft: false,
                user_notes_count: 0,
                web_url: "https://example.com/1".to_string(),
                created_at: "2026-03-01T00:00:00.000Z".parse().unwrap(),
                updated_at: "2026-03-01T00:00:00.000Z".parse().unwrap(),
                detailed_merge_status: None,
                has_conflicts: None,
                approved: None,
            },
            worktree: Some(WorktreeInfo {
                path: "/tmp".to_string(),
                branch: Some(branch.to_string()),
                head_commit: "abc".to_string(),
                is_main: false,
                is_bare: false,
            }),
            tmux_pane,
            has_new_activity: false,
        }
    }

    fn snapshot(projects: Vec<ProjectSnapshot>, panes: Vec<AgentPane>) -> DashboardSnapshot {
        DashboardSnapshot {
            projects,
            panes,
            groups: vec![],
            detail: None,
            error: None,
            seconds_since_refresh: 0,
            default_agent_command: None,
            default_worktree_with_prompt: None,
            keybindings: Default::default(),
            pending_changes: vec![],
            agent_actions: vec![],
            pending_agent_changes: vec![],
            global_mrs: vec![],
            activity_feed: vec![],
            auto_switch_project: false,
        }
    }

    fn project(name: &str, mrs: Vec<LinkedMergeRequest>, wts: Vec<WtWorktree>) -> ProjectSnapshot {
        ProjectSnapshot {
            name: name.to_string(),
            source: ProjectForge::Gitlab,
            project_path: format!("team/{}", name),
            local_path: "/tmp".to_string(),
            dashboard: DashboardState { linked_mrs: mrs },
            cached_worktrees: wts,
            cached_mr_detail: None,
            cached_pipeline_jobs: vec![],
            cached_threads: vec![],
            cached_threads_iid: None,
        }
    }

    #[test]
    fn locate_pane_prefers_mr_link_over_path() {
        let dir = std::env::temp_dir();
        let dir = dir.to_str().unwrap();
        let p = pane("%7", dir);
        let snap = snapshot(
            vec![project(
                "pertmux",
                vec![linked_mr("other", None), linked_mr("feat", Some(p.clone()))],
                vec![worktree(dir, "feat")],
            )],
            vec![p.clone()],
        );
        assert_eq!(
            locate_pane(&snap, &p),
            Some((0, SelectionSection::MergeRequests, 1))
        );
    }

    #[test]
    fn locate_pane_falls_back_to_worktree_path() {
        let dir = std::env::temp_dir();
        let dir = dir.to_str().unwrap();
        let p = pane("%7", dir);
        let snap = snapshot(
            vec![
                project("other", vec![], vec![]),
                project(
                    "pertmux",
                    vec![],
                    vec![worktree("/nonexistent-xyz", "a"), worktree(dir, "feat")],
                ),
            ],
            vec![p.clone()],
        );
        assert_eq!(
            locate_pane(&snap, &p),
            Some((1, SelectionSection::Worktrees, 1))
        );
    }

    #[test]
    fn locate_pane_returns_none_when_unknown() {
        let p = pane("%7", "/nonexistent-abc");
        let snap = snapshot(vec![project("pertmux", vec![], vec![])], vec![p.clone()]);
        assert_eq!(locate_pane(&snap, &p), None);
    }

    fn session_search_state(n: usize, selected: usize) -> super::ClientState {
        let panes: Vec<AgentPane> = (0..n)
            .map(|i| pane(&format!("%{}", i), &format!("/tmp/wt{}", i)))
            .collect();
        let mut state = super::ClientState::from_snapshot(snapshot(vec![], panes));
        state.popup = crate::app::PopupState::SessionSearch {
            input: String::new(),
            filtered: (0..n).collect(),
            selected,
        };
        state
    }

    fn session_search_rows(state: &super::ClientState) -> (Vec<usize>, usize) {
        match &state.popup {
            crate::app::PopupState::SessionSearch {
                filtered, selected, ..
            } => (filtered.clone(), *selected),
            _ => panic!("expected a SessionSearch popup"),
        }
    }

    #[test]
    fn drop_session_search_row_removes_the_highlighted_row() {
        let mut state = session_search_state(3, 1);
        state.drop_session_search_row();
        assert_eq!(session_search_rows(&state), (vec![0, 2], 1));
    }

    #[test]
    fn drop_session_search_row_clamps_on_the_last_row() {
        let mut state = session_search_state(3, 2);
        state.drop_session_search_row();
        assert_eq!(session_search_rows(&state), (vec![0, 1], 1));
    }

    /// The anchor must be resolved against the pane list `filtered` indexes
    /// into. Resolving it after the swap re-anchors onto whichever pane
    /// inherited the old index, silently moving the cursor one row.
    #[test]
    fn snapshot_refresh_keeps_the_cursor_on_its_pane_after_a_kill() {
        let mut state = session_search_state(4, 1);
        // Kill B: the cursor now points at C (old index 2).
        state.drop_session_search_row();
        assert_eq!(session_search_rows(&state), (vec![0, 2, 3], 1));

        // The daemon catches up and drops B from the pane list.
        let panes: Vec<AgentPane> = ["%0", "%2", "%3"]
            .iter()
            .map(|id| pane(id, &format!("/tmp/wt{}", &id[1..])))
            .collect();
        state.update_snapshot(snapshot(vec![], panes));

        let (filtered, selected) = session_search_rows(&state);
        assert_eq!(filtered, vec![0, 1, 2]);
        assert_eq!(state.snapshot.panes[filtered[selected]].pane_id, "%2");
    }

    #[test]
    fn drop_session_search_row_handles_the_final_row() {
        let mut state = session_search_state(1, 0);
        state.drop_session_search_row();
        assert_eq!(session_search_rows(&state), (vec![], 0));

        // Killing again is a no-op rather than a panic.
        state.drop_session_search_row();
        assert_eq!(session_search_rows(&state), (vec![], 0));
    }
}
