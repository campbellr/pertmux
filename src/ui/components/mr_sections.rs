use std::collections::HashMap;
use std::path::PathBuf;

use super::cards::{render_mr_card, render_worktree_card};
use crate::types::AgentPane;
use crate::ui::{ACCENT, ProjectRenderData};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
    },
};

/// Height of a single MR/worktree card, in rows.
const CARD_H: u16 = 4;
/// Border rows consumed by a section's surrounding block.
const CHROME_H: u16 = 2;

/// Split the list panel between the worktree and MR sections.
///
/// Each section gets what it needs up to half the height; whatever it
/// doesn't use goes to the other. Only when both overflow is the height
/// split evenly. Proportional sizing let 20+ worktrees squeeze 2 MRs into
/// ~8% of the panel.
fn section_heights(total: u16, wt_count: u16, mr_count: u16) -> (u16, u16) {
    let want = |n: u16| n.max(1).saturating_mul(CARD_H).saturating_add(CHROME_H);
    let (wt_want, mr_want) = (want(wt_count), want(mr_count));
    let half = total / 2;

    if wt_want.saturating_add(mr_want) <= total || mr_want <= half {
        (total - mr_want, mr_want)
    } else if wt_want <= half {
        (wt_want, total - wt_want)
    } else {
        // Round worktrees down to whole cards so the partial-card rows all
        // land in one section instead of being wasted in both.
        let cards = half.saturating_sub(CHROME_H) / CARD_H;
        let wt = (cards * CARD_H + CHROME_H).min(half);
        (wt, total - wt)
    }
}

pub(crate) fn draw_mr_sections_render(frame: &mut Frame, proj: &ProjectRenderData<'_>, area: Rect) {
    let (wt_h, mr_h) = section_heights(
        area.height,
        proj.cached_worktrees.len() as u16,
        proj.dashboard.linked_mrs.len() as u16,
    );

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(wt_h), Constraint::Length(mr_h)])
        .split(area);

    draw_worktree_block_render(
        frame,
        proj,
        chunks[0],
        proj.list_focused && !proj.mr_focused,
    );
    draw_mr_block_render(frame, proj, chunks[1], proj.list_focused && proj.mr_focused);
}

fn draw_mr_block_render(
    frame: &mut Frame,
    proj: &ProjectRenderData<'_>,
    area: Rect,
    focused: bool,
) {
    let border_color = if focused { ACCENT } else { Color::Indexed(238) };
    let mr_count = proj.dashboard.linked_mrs.len();

    let block = Block::default()
        .title(Line::from(vec![Span::styled(
            format!(" Merge Requests ({}) ", mr_count),
            Style::default()
                .fg(border_color)
                .add_modifier(Modifier::BOLD),
        )]))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color));

    let section_inner = block.inner(area);
    frame.render_widget(block, area);

    if section_inner.height == 0 || section_inner.width == 0 {
        return;
    }

    if mr_count == 0 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  No open MRs. Press 'r' to refresh.",
                Style::default().fg(Color::DarkGray),
            ))),
            section_inner,
        );
        return;
    }

    let card_h = CARD_H;
    let total_content = mr_count as u16 * card_h;
    let selected_y = proj.mr_selected as u16 * card_h;

    let scroll: u16 = if total_content <= section_inner.height {
        0
    } else {
        let max_scroll = total_content.saturating_sub(section_inner.height);
        let ideal = selected_y.saturating_sub(section_inner.height / 2);
        ideal.min(max_scroll)
    };

    for (i, linked) in proj.dashboard.linked_mrs.iter().enumerate() {
        let card_y = i as u16 * card_h;
        let sy = card_y as i32 - scroll as i32;
        if sy + card_h as i32 <= 0 || sy >= section_inner.height as i32 {
            continue;
        }
        if sy < 0 || sy as u16 + card_h > section_inner.height {
            continue;
        }
        let ay = section_inner.y + sy as u16;
        let is_selected = focused && i == proj.mr_selected;
        let rect = Rect::new(section_inner.x, ay, section_inner.width, card_h);
        render_mr_card(frame, linked, rect, is_selected);
    }

    if total_content > section_inner.height {
        let mut scrollbar_state = ScrollbarState::new(mr_count).position(proj.mr_selected);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None),
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut scrollbar_state,
        );
    }
}

pub(crate) fn build_pane_by_path(panes: &[AgentPane]) -> HashMap<PathBuf, &AgentPane> {
    panes
        .iter()
        .filter_map(|pane| {
            std::fs::canonicalize(&pane.pane_path)
                .ok()
                .map(|path| (path, pane))
        })
        .collect()
}

fn draw_worktree_block_render(
    frame: &mut Frame,
    proj: &ProjectRenderData<'_>,
    area: Rect,
    focused: bool,
) {
    let border_color = if focused { ACCENT } else { Color::Indexed(238) };
    let wt_count = proj.cached_worktrees.len();

    let block = Block::default()
        .title(Line::from(vec![Span::styled(
            format!(" Worktrees ({}) ", wt_count),
            Style::default()
                .fg(border_color)
                .add_modifier(Modifier::BOLD),
        )]))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color));

    let section_inner = block.inner(area);
    frame.render_widget(block, area);

    if section_inner.height == 0 || section_inner.width == 0 {
        return;
    }

    if wt_count == 0 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  Install worktrunk (wt) for worktree listing",
                Style::default().fg(Color::DarkGray),
            ))),
            section_inner,
        );
        return;
    }

    let pane_by_path = build_pane_by_path(proj.panes);

    let card_h = CARD_H;
    let total_content = wt_count as u16 * card_h;
    let selected_y = proj.worktree_selected as u16 * card_h;

    let scroll: u16 = if total_content <= section_inner.height {
        0
    } else {
        let max_scroll = total_content.saturating_sub(section_inner.height);
        let ideal = selected_y.saturating_sub(section_inner.height / 2);
        ideal.min(max_scroll)
    };

    for (i, wt) in proj.cached_worktrees.iter().enumerate() {
        let card_y = i as u16 * card_h;
        let sy = card_y as i32 - scroll as i32;
        if sy + card_h as i32 <= 0 || sy >= section_inner.height as i32 {
            continue;
        }
        if sy < 0 || sy as u16 + card_h > section_inner.height {
            continue;
        }
        let ay = section_inner.y + sy as u16;
        let is_selected = focused && i == proj.worktree_selected;
        let rect = Rect::new(section_inner.x, ay, section_inner.width, card_h);
        let matched_pane = wt
            .path
            .as_ref()
            .and_then(|p| std::fs::canonicalize(p).ok())
            .and_then(|canon| pane_by_path.get(&canon).copied());
        render_worktree_card(frame, wt, matched_pane, rect, is_selected);
    }

    if total_content > section_inner.height {
        let mut scrollbar_state = ScrollbarState::new(wt_count).position(proj.worktree_selected);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None),
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut scrollbar_state,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{CARD_H, CHROME_H, section_heights};

    #[test]
    fn both_sections_fit_exactly() {
        // 3 worktrees + 2 MRs = 14 + 10 rows.
        assert_eq!(section_heights(24, 3, 2), (14, 10));
    }

    #[test]
    fn surplus_goes_to_worktrees() {
        assert_eq!(section_heights(50, 3, 2), (40, 10));
    }

    #[test]
    fn small_mr_list_leaves_the_rest_to_worktrees() {
        // 22 worktrees vs 2 MRs: MRs show in full, worktrees get the rest.
        assert_eq!(section_heights(40, 22, 2), (30, 10));
    }

    #[test]
    fn small_worktree_list_leaves_the_rest_to_mrs() {
        assert_eq!(section_heights(40, 1, 30), (6, 34));
    }

    #[test]
    fn no_mrs_leaves_the_rest_to_worktrees() {
        assert_eq!(section_heights(40, 22, 0), (34, 6));
    }

    #[test]
    fn both_overflowing_split_evenly_in_whole_cards() {
        // half = 20 -> 18 inner rows -> 4 cards -> 18; MRs get the other 22.
        assert_eq!(section_heights(40, 22, 30), (18, 22));
        // Odd totals: the partial-card rows all go to the MR section.
        assert_eq!(section_heights(41, 22, 30), (18, 23));
    }

    #[test]
    fn empty_sections_still_get_a_row() {
        // max(1) in `want` keeps the "No open MRs" placeholder visible.
        assert_eq!(section_heights(24, 0, 0), (18, 6));
    }

    #[test]
    fn degrades_without_panicking_on_tiny_areas() {
        for total in 0..=(CARD_H + CHROME_H) * 2 {
            let (wt, mr) = section_heights(total, 22, 2);
            assert_eq!(wt + mr, total, "total {total}");
            let (wt, mr) = section_heights(total, 22, 30);
            assert_eq!(wt + mr, total, "total {total}");
        }
    }
}
