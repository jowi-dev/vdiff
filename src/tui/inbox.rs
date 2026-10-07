//! `vdiff --inbox`'s picker (issue #36): a full-screen list of the review
//! inbox (see [`crate::review::inbox`]) grouped by category, navigated
//! with `j`/`k`. `Enter` hands the selected PR to the caller's `open`
//! callback, which `main` implements by running a child `vdiff --pr <n>`
//! while this picker's terminal is suspended; when that session closes the
//! inbox is fetched again and redrawn, so it always reflects GitHub's
//! current state rather than anything remembered between runs.
//!
//! [`InboxPicker`] is the pure state and key handling, unit-tested without
//! a terminal; [`draw`] renders it; [`run`] owns the terminal and the
//! fetch/open loop.

use crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::review::inbox::{InboxCategory, InboxEntry, InboxReport};

/// What the event loop should do after a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerAction {
    None,
    Quit,
    Refresh,
    /// Open this PR number via `vdiff --pr`.
    Open(u64),
}

/// The picker's state: the last fetched report (or the error fetching it),
/// the selected entry, and a one-line status message.
#[derive(Debug, Clone)]
pub struct InboxPicker {
    report: Option<InboxReport>,
    error: Option<String>,
    selected: usize,
    current_repo: Option<String>,
    status: Option<String>,
}

impl InboxPicker {
    /// A picker with nothing fetched yet. `current_repo` is the `owner/name`
    /// `--pr` resolves PRs against, when known: entries from any other repo
    /// can't be opened from here.
    pub fn new(current_repo: Option<String>) -> Self {
        Self {
            report: None,
            error: None,
            selected: 0,
            current_repo,
            status: None,
        }
    }

    /// Install a freshly fetched report, or the error fetching it. The
    /// selection follows the previously selected PR when it is still in
    /// the inbox, and is clamped into range otherwise.
    pub fn set_report(&mut self, result: Result<InboxReport, String>) {
        let previous = self.selected_entry().map(|e| (e.repo.clone(), e.number));
        match result {
            Ok(report) => {
                self.selected = previous
                    .and_then(|(repo, number)| {
                        report
                            .entries
                            .iter()
                            .position(|r| r.entry.repo == repo && r.entry.number == number)
                    })
                    .unwrap_or(self.selected)
                    .min(report.entries.len().saturating_sub(1));
                self.report = Some(report);
                self.error = None;
            }
            Err(err) => {
                self.report = None;
                self.error = Some(err);
                self.selected = 0;
            }
        }
    }

    /// Replace the footer's status message.
    pub fn set_status(&mut self, status: Option<String>) {
        self.status = status;
    }

    fn entries(&self) -> &[crate::review::inbox::ReportEntry] {
        self.report.as_ref().map_or(&[], |r| r.entries.as_slice())
    }

    fn selected_entry(&self) -> Option<&InboxEntry> {
        self.entries().get(self.selected).map(|r| &r.entry)
    }

    /// Apply one key: `j`/`k`/arrows move, `g`/`G` jump to the ends,
    /// `Enter` opens, `r` refreshes, `q`/`Esc` quit.
    pub fn handle_key(&mut self, code: KeyCode) -> PickerAction {
        let last = self.entries().len().saturating_sub(1);
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return PickerAction::Quit,
            KeyCode::Char('r') => return PickerAction::Refresh,
            KeyCode::Char('j') | KeyCode::Down => self.selected = (self.selected + 1).min(last),
            KeyCode::Char('k') | KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Char('g') | KeyCode::Home => self.selected = 0,
            KeyCode::Char('G') | KeyCode::End => self.selected = last,
            KeyCode::Enter => return self.open_selected(),
            _ => {}
        }
        PickerAction::None
    }

    fn open_selected(&mut self) -> PickerAction {
        let Some(entry) = self.selected_entry() else {
            return PickerAction::None;
        };
        match &self.current_repo {
            Some(current) if *current != entry.repo => {
                let message = format!(
                    "{}#{} is not in {current}; run `vdiff --pr {}` from a checkout of {}",
                    entry.repo, entry.number, entry.number, entry.repo
                );
                self.status = Some(message);
                PickerAction::None
            }
            _ => PickerAction::Open(entry.number),
        }
    }
}

/// The header shown above each [`InboxCategory`]'s group.
fn category_heading(category: InboxCategory) -> &'static str {
    match category {
        InboxCategory::ReviewRequested => "Review requested",
        InboxCategory::NewCommits => "New commits since your review",
        InboxCategory::Replies => "Replies waiting",
    }
}

/// Draw the picker into the whole frame: a title line, the grouped list,
/// and a footer of key hints plus the status message.
pub fn draw(frame: &mut Frame, picker: &InboxPicker) {
    let [title_area, list_area, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let bold = Style::default().add_modifier(Modifier::BOLD);
    let dim = Style::default().add_modifier(Modifier::DIM);

    let scope = match &picker.report {
        Some(report) => format!(
            "{} ({})",
            report.scope.as_deref().unwrap_or("all repos"),
            report.total
        ),
        None => String::new(),
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" vdiff inbox ", bold),
            Span::raw(scope),
        ])),
        title_area,
    );

    let mut footer = vec![Span::styled(
        " j/k move  Enter open  r refresh  q quit",
        dim,
    )];
    if let Some(status) = &picker.status {
        footer.push(Span::raw(format!("  {status}")));
    }
    frame.render_widget(Paragraph::new(Line::from(footer)), footer_area);

    let Some(report) = &picker.report else {
        let message = match &picker.error {
            Some(err) => format!(" error: {err}  (r to retry)"),
            None => " Fetching inbox...".to_string(),
        };
        frame.render_widget(Paragraph::new(message), list_area);
        return;
    };
    if report.entries.is_empty() {
        frame.render_widget(Paragraph::new(" Nothing waiting on you."), list_area);
        return;
    }

    let mut items = Vec::new();
    let mut selected_row = 0;
    let mut current: Option<InboxCategory> = None;
    for (idx, row) in report.entries.iter().enumerate() {
        let entry = &row.entry;
        if current != Some(entry.category) {
            current = Some(entry.category);
            let count = report
                .entries
                .iter()
                .filter(|r| r.entry.category == entry.category)
                .count();
            if !items.is_empty() {
                items.push(ListItem::new(""));
            }
            items.push(ListItem::new(Line::styled(
                format!("{} ({count})", category_heading(entry.category)),
                bold,
            )));
        }
        if idx == picker.selected {
            selected_row = items.len();
        }
        items.push(ListItem::new(Line::from(vec![
            Span::styled(format!("{}#{}", entry.repo, entry.number), bold),
            Span::raw(format!("  {}  ", entry.title)),
            Span::styled(format!("@{}", entry.author), dim),
            Span::raw(format!("  {}", row.reason)),
        ])));
    }

    let mut state = ListState::default().with_selected(Some(selected_row));
    let list = List::new(items)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    frame.render_stateful_widget(list, list_area, &mut state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::inbox::{InboxCategory, InboxEntry};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn entry(repo: &str, number: u64, category: InboxCategory) -> InboxEntry {
        InboxEntry {
            repo: repo.into(),
            number,
            title: format!("Title {number}"),
            author: "alice".into(),
            url: String::new(),
            updated_at: String::new(),
            category,
            review_requested: category == InboxCategory::ReviewRequested,
            head_changed_since_review: category == InboxCategory::NewCommits,
            new_commits_since_review: (category == InboxCategory::NewCommits).then_some(3),
            unanswered_replies: if category == InboxCategory::Replies {
                2
            } else {
                0
            },
        }
    }

    fn report(entries: Vec<InboxEntry>) -> InboxReport {
        InboxReport::new(Some("o/r".into()), entries)
    }

    fn picker() -> InboxPicker {
        let mut p = InboxPicker::new(Some("o/r".into()));
        p.set_report(Ok(report(vec![
            entry("o/r", 1, InboxCategory::ReviewRequested),
            entry("o/r", 2, InboxCategory::NewCommits),
            entry("o/other", 3, InboxCategory::Replies),
        ])));
        p
    }

    fn screen(p: &InboxPicker) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|f| draw(f, p)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn enter_opens_the_selected_pr() {
        let mut p = picker();
        assert_eq!(p.handle_key(KeyCode::Enter), PickerAction::Open(1));
        p.handle_key(KeyCode::Char('j'));
        assert_eq!(p.handle_key(KeyCode::Enter), PickerAction::Open(2));
    }

    #[test]
    fn selection_stays_in_bounds() {
        let mut p = picker();
        p.handle_key(KeyCode::Char('k'));
        assert_eq!(p.handle_key(KeyCode::Enter), PickerAction::Open(1));
        for _ in 0..5 {
            p.handle_key(KeyCode::Down);
        }
        p.handle_key(KeyCode::Up);
        assert_eq!(p.handle_key(KeyCode::Enter), PickerAction::Open(2));
        p.handle_key(KeyCode::Char('g'));
        assert_eq!(p.handle_key(KeyCode::Enter), PickerAction::Open(1));
    }

    #[test]
    fn a_pr_from_another_repo_is_not_opened() {
        let mut p = picker();
        p.handle_key(KeyCode::Char('G'));
        assert_eq!(p.handle_key(KeyCode::Enter), PickerAction::None);
        assert!(screen(&p).contains("o/other"));
        assert!(
            p.status.as_deref().unwrap().contains("checkout of o/other"),
            "{:?}",
            p.status
        );
    }

    #[test]
    fn unknown_current_repo_lets_any_pr_open() {
        let mut p = picker();
        p.current_repo = None;
        p.handle_key(KeyCode::Char('G'));
        assert_eq!(p.handle_key(KeyCode::Enter), PickerAction::Open(3));
    }

    #[test]
    fn quit_refresh_and_empty_inbox() {
        let mut p = InboxPicker::new(None);
        assert_eq!(p.handle_key(KeyCode::Enter), PickerAction::None);
        assert_eq!(p.handle_key(KeyCode::Char('r')), PickerAction::Refresh);
        assert_eq!(p.handle_key(KeyCode::Char('q')), PickerAction::Quit);
        assert_eq!(p.handle_key(KeyCode::Esc), PickerAction::Quit);
    }

    #[test]
    fn refresh_keeps_the_same_pr_selected() {
        let mut p = picker();
        p.handle_key(KeyCode::Char('j'));
        p.set_report(Ok(report(vec![
            entry("o/r", 9, InboxCategory::ReviewRequested),
            entry("o/r", 1, InboxCategory::ReviewRequested),
            entry("o/r", 2, InboxCategory::NewCommits),
        ])));
        assert_eq!(p.handle_key(KeyCode::Enter), PickerAction::Open(2));
    }

    #[test]
    fn refresh_clamps_when_the_selected_pr_is_gone() {
        let mut p = picker();
        p.handle_key(KeyCode::Char('G'));
        p.set_report(Ok(report(vec![entry(
            "o/r",
            1,
            InboxCategory::ReviewRequested,
        )])));
        assert_eq!(p.handle_key(KeyCode::Enter), PickerAction::Open(1));
    }

    #[test]
    fn fetch_error_is_shown() {
        let mut p = picker();
        p.set_report(Err("gh failed: boom".into()));
        assert_eq!(p.handle_key(KeyCode::Enter), PickerAction::None);
        assert!(screen(&p).contains("gh failed: boom"));
    }

    #[test]
    fn draws_groups_with_counts_and_rows_with_reasons() {
        let s = screen(&picker());
        assert!(s.contains("Review requested (1)"), "{s}");
        assert!(s.contains("New commits since your review (1)"), "{s}");
        assert!(s.contains("Replies waiting (1)"), "{s}");
        assert!(s.contains("o/r#1"), "{s}");
        assert!(s.contains("Title 2"), "{s}");
        assert!(s.contains("@alice"), "{s}");
        assert!(s.contains("3 new commits since your review"), "{s}");
        assert!(s.contains("2 replies"), "{s}");
        assert!(s.contains("Enter open"), "{s}");
    }

    #[test]
    fn draws_an_empty_and_a_loading_inbox() {
        assert!(screen(&InboxPicker::new(None)).contains("Fetching"));
        let mut p = InboxPicker::new(None);
        p.set_report(Ok(report(vec![])));
        assert!(screen(&p).contains("Nothing waiting on you"));
    }
}
