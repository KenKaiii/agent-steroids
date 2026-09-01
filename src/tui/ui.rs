//! Drawing. Pure: state in, frame out.
//!
//! Deliberately restrained: one accent colour, named colours only so the app
//! stays readable on light and dark terminals, and selection by reversed video
//! rather than a background colour that may clash with the user's theme.

use ratatui::prelude::*;
use ratatui::widgets::{
    Block, BorderType, Clear, List, ListItem, Padding, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};

use super::app::{App, Modal, Screen};
use crate::search::{Diagnosis, Facts};

const ACCENT: Color = Color::Cyan;

/// One line explaining why a search found nothing, for the results pane.
pub fn short_diagnosis(facts: &Facts) -> String {
    match &facts.diagnosis {
        Diagnosis::EmptyCorpus => "No repositories indexed yet, press a to add one.".into(),
        Diagnosis::NearMiss { nearest, .. } => {
            format!("No matches. The corpus does contain '{nearest}', try that.")
        }
        Diagnosis::TopicAbsent { missing } => {
            format!("No matches. '{missing}' appears in no indexed repository.")
        }
        Diagnosis::SpellingMismatch { .. } => {
            "No matches for this exact pattern. Try a shorter one.".into()
        }
        Diagnosis::TooBroad => "Add a literal of 3+ characters to search on.".into(),
        Diagnosis::CrossLine => {
            "Matching runs one line at a time, so a newline never matches.".into()
        }
        Diagnosis::FilterExcludesAll { advice } => advice.clone(),
    }
}

fn panel(title: &str) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
}

fn human(bytes: f64) -> String {
    for (unit, scale) in [("GB", 1e9), ("MB", 1e6), ("KB", 1e3)] {
        if bytes >= scale {
            return format!("{:.1}{unit}", bytes / scale);
        }
    }
    format!("{bytes:.0}B")
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let areas = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .split(frame.area());

    draw_header(frame, areas[0], app);
    match app.screen {
        Screen::Repos => draw_repos(frame, areas[1], app),
        Screen::Files => draw_files(frame, areas[1], app),
        Screen::Search => draw_search(frame, areas[1], app),
        Screen::Preview => draw_preview(frame, areas[1], app),
    }
    draw_footer(frame, areas[2], app);
    draw_modal(frame, app);
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let files: i64 = app.repos.iter().map(|summary| summary.files).sum();
    // Spell out the split: the per-repository column shows stored code, and
    // the difference from the total is the search index.
    let sizes = format!(
        "{} code + {} index = {}",
        human(app.disk_bytes.saturating_sub(app.index_bytes) as f64),
        human(app.index_bytes as f64),
        human(app.disk_bytes as f64),
    );
    let mut summary = format!("{} repositories · {files} files · {sizes}", app.repos.len());
    // The path is the least useful part, so append it only if it fits whole
    // rather than letting it trail off mid-word.
    let path = app.root.display().to_string();
    if summary.chars().count() + path.chars().count() + 13 <= area.width as usize {
        summary.push_str(&format!(" · {path}"));
    }
    let line = Line::from(vec![
        Span::styled(
            " steroids ",
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(summary, Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    if !app.status.is_empty() {
        let style = if app.status.starts_with("failed") {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::Green)
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(format!(" {}", app.status), style))),
            area,
        );
        return;
    }

    let keys: &[(&str, &str)] = match app.screen {
        Screen::Repos => &[
            ("↑↓", "select"),
            ("↵", "files"),
            ("/", "search"),
            ("a", "add"),
            ("d", "remove"),
            ("u", "update"),
            ("q", "quit"),
        ],
        Screen::Files => &[
            ("↑↓", "select"),
            ("↵", "open"),
            ("/", "search"),
            ("esc", "back"),
        ],
        Screen::Search => &[
            ("type", "to search"),
            ("↑↓", "results"),
            ("↵", "open"),
            ("esc", "back"),
        ],
        Screen::Preview => &[("↑↓", "scroll"), ("space", "page"), ("esc", "back")],
    };

    let mut spans = vec![Span::raw(" ")];
    for (key, label) in keys {
        spans.push(Span::styled(
            *key,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {label}   "),
            Style::default().fg(Color::DarkGray),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_repos(frame: &mut Frame, area: Rect, app: &mut App) {
    if app.repos.is_empty() {
        let message = Paragraph::new(vec![
            Line::raw(""),
            Line::from(Span::styled(
                "No repositories indexed yet.",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::raw(""),
            Line::from(Span::styled(
                "Press  a  to add one, e.g. openai/openai-agents-python",
                Style::default().fg(Color::DarkGray),
            )),
        ])
        .block(panel("Repositories"));
        frame.render_widget(message, area);
        return;
    }

    // Reserve the fixed columns first so the date is never the thing that
    // falls off the right edge; the name absorbs whatever is left.
    // 12 = language, 14 = files, 10 = size, 20 = sha + gap + date.
    const TRAILING: usize = 12 + 14 + 10 + 20;
    // 4 = two borders plus one column of padding each side.
    let width = (area.width as usize).saturating_sub(TRAILING + 4).max(12);
    let items: Vec<ListItem> = app
        .repos
        .iter()
        .map(|summary| {
            let short: String = summary.commit_sha.chars().take(8).collect();
            ListItem::new(Line::from(vec![
                Span::raw(format!("{:<width$}", truncate(&summary.name, width))),
                Span::styled(
                    format!("{:<12}", summary.language),
                    Style::default().fg(Color::Magenta),
                ),
                Span::styled(
                    format!("{:>6} files  ", summary.files),
                    Style::default().fg(ACCENT),
                ),
                Span::styled(
                    format!("{:>8}  ", human(summary.disk_bytes as f64)),
                    Style::default().fg(ACCENT),
                ),
                Span::styled(
                    format!(
                        "{short}  {}",
                        summary.indexed_at.split(' ').next().unwrap_or("")
                    ),
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(panel("Repositories"))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, area, &mut app.repos_state);
}

fn draw_files(frame: &mut Frame, area: Rect, app: &mut App) {
    let width = area.width.saturating_sub(24) as usize;
    let items: Vec<ListItem> = app
        .files
        .iter()
        .map(|(path, language, size)| {
            let shown: String = if path.chars().count() > width {
                // Keep the filename visible; the leading directories matter less.
                let tail: String = path
                    .chars()
                    .skip(path.chars().count().saturating_sub(width - 1))
                    .collect();
                format!("…{tail}")
            } else {
                path.clone()
            };
            ListItem::new(Line::from(vec![
                Span::raw(format!("{shown:<width$}")),
                Span::styled(format!("{language:<12}"), Style::default().fg(ACCENT)),
                Span::styled(
                    format!("{:>8}", human(*size as f64)),
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        })
        .collect();

    let title = format!("{}  ({} files)", app.files_repo, app.files.len());
    let list = List::new(items)
        .block(panel(&title))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, area, &mut app.files_state);
}

fn draw_search(frame: &mut Frame, area: Rect, app: &mut App) {
    let rows = Layout::vertical([Constraint::Length(3), Constraint::Min(3)]).split(area);

    // Scroll the text horizontally once it outgrows the box, so the caret stays
    // visible instead of running off the right edge.
    let inner = rows[0].width.saturating_sub(4) as usize;
    let scroll = app.query.visual_scroll(inner);
    let input = Paragraph::new(app.query.value())
        .scroll((0, scroll as u16))
        .block(panel("Search"));
    frame.render_widget(input, rows[0]);
    // A visible cursor tells the user typing goes here.
    frame.set_cursor_position((
        rows[0].x + 2 + (app.query.visual_cursor().saturating_sub(scroll)) as u16,
        rows[0].y + 1,
    ));

    if app.hits.is_empty() {
        let message = app
            .searching_message
            .clone()
            .unwrap_or_else(|| "Type to search across every indexed repository.".into());
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                message,
                Style::default().fg(Color::DarkGray),
            )))
            .wrap(Wrap { trim: true })
            .block(panel("Results")),
            rows[1],
        );
        return;
    }

    let panes =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).split(rows[1]);

    let items: Vec<ListItem> = app
        .hits
        .iter()
        .map(|hit| {
            let file = hit.path.rsplit('/').next().unwrap_or(&hit.path);
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(
                        hit.repo.split('/').next_back().unwrap_or(&hit.repo),
                        Style::default().fg(ACCENT),
                    ),
                    Span::raw(format!("  {file}:{}", hit.line_number)),
                ]),
                Line::from(Span::styled(
                    format!("  {}", truncate(&hit.scope, panes[0].width as usize - 4)),
                    Style::default().fg(Color::DarkGray),
                )),
            ])
        })
        .collect();

    let results_title = format!("{} results", app.hits.len());
    let list = List::new(items)
        .block(panel(&results_title))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, panes[0], &mut app.hits_state);

    if let Some(hit) = app.hits_state.selected().and_then(|i| app.hits.get(i)) {
        let mut lines = vec![
            Line::from(Span::styled(
                truncate(
                    &format!("{}/{}", hit.repo, hit.path),
                    panes[1].width as usize - 4,
                ),
                Style::default().fg(Color::DarkGray),
            )),
            Line::raw(""),
        ];
        // Mark the line that actually matched; without it the reader has to
        // guess which of seven lines of context is the hit.
        let context = app.highlighter.lines(&hit.path, &hit.context);
        lines.extend(context.into_iter().enumerate().map(|(offset, mut line)| {
            if offset == hit.context_offset {
                line.spans
                    .insert(0, Span::styled("▌", Style::default().fg(ACCENT)));
                line.style(Style::default().add_modifier(Modifier::BOLD))
            } else {
                line.spans.insert(0, Span::raw(" "));
                line
            }
        }));
        frame.render_widget(Paragraph::new(lines).block(panel("Preview")), panes[1]);
    }
}

fn draw_preview(frame: &mut Frame, area: Rect, app: &mut App) {
    let height = area.height.saturating_sub(2) as usize;
    let total = app.preview_lines.len();
    let start = app.preview_scroll.min(total.saturating_sub(1));
    let end = (start + height).min(total);

    let number_width = total.to_string().len();
    let lines: Vec<Line> = app.preview_lines[start..end]
        .iter()
        .enumerate()
        .map(|(offset, code)| {
            let mut line = code.clone();
            line.spans.insert(
                0,
                Span::styled(
                    format!("{:>number_width$}  ", start + offset + 1),
                    Style::default().fg(Color::DarkGray),
                ),
            );
            line
        })
        .collect();

    let title = format!("{}  ({total} lines)", app.preview_title);
    frame.render_widget(Paragraph::new(lines).block(panel(&title)), area);

    let mut scrollbar_state = ScrollbarState::new(total.saturating_sub(height)).position(start);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight),
        area,
        &mut scrollbar_state,
    );
}

fn draw_modal(frame: &mut Frame, app: &App) {
    let (title, body, hint) = match &app.modal {
        Modal::None => return,
        Modal::AddRepo(input) => (
            "Add repositories",
            input.value().to_string(),
            "space-separated owner/name   ↵ add   esc cancel",
        ),
        Modal::ConfirmRemove(name) => (
            "Remove repository",
            name.clone(),
            "y remove   any other key cancel",
        ),
        Modal::Working(progress) => ("Working", progress.clone(), "please wait…"),
    };

    let area = centered(frame.area(), 62, 7);
    frame.render_widget(Clear, area);
    let inner = area.width.saturating_sub(4) as usize;
    // The caret must line up with the text, so the input scrolls rather than
    // wrapping; other modals have short bodies and can wrap freely.
    let scroll = match &app.modal {
        Modal::AddRepo(input) => input.visual_scroll(inner),
        _ => 0,
    };
    let text = vec![
        Line::raw(""),
        Line::from(Span::styled(
            body,
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray))),
    ];
    let mut paragraph =
        Paragraph::new(text).block(panel(title).border_style(Style::default().fg(ACCENT)));
    paragraph = match &app.modal {
        Modal::AddRepo(_) => paragraph.scroll((0, scroll as u16)),
        _ => paragraph.wrap(Wrap { trim: true }),
    };
    frame.render_widget(paragraph, area);

    if let Modal::AddRepo(input) = &app.modal {
        frame.set_cursor_position((
            area.x + 2 + (input.visual_cursor().saturating_sub(scroll)) as u16,
            area.y + 2,
        ));
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    text.chars()
        .take(width.saturating_sub(1))
        .collect::<String>()
        + "…"
}
