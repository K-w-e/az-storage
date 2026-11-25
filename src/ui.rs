//! Rendering ratatui.

use crate::app::*;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph, Wrap};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const ACCENT: Color = Color::Cyan;

pub fn draw(f: &mut Frame, app: &mut App) {
    let n_xfer = app.transfers.len().min(3) as u16;
    let [header, body, xfers, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(5),
        Constraint::Length(n_xfer),
        Constraint::Length(1),
    ])
    .areas(f.area());

    app.hit = HitAreas::default();
    draw_header(f, app, header);
    let [left, mid, right] = Layout::horizontal([
        Constraint::Percentage(30),
        Constraint::Percentage(32),
        Constraint::Percentage(38),
    ])
    .areas(body);
    app.hit.tree = left;
    app.hit.main = mid;
    app.hit.preview = right;
    draw_tree(f, app, left);
    draw_main(f, app, mid);
    draw_preview(f, app, right);
    draw_transfers(f, app, xfers);
    draw_footer(f, app, footer);
    draw_modal(f, app);
}

fn panel_block(title: String, focused: bool) -> Block<'static> {
    let style = if focused {
        Style::new().fg(ACCENT)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_type(if focused {
            BorderType::Thick
        } else {
            BorderType::Rounded
        })
        .border_style(style)
        .title(Span::styled(
            format!(" {title} "),
            style.add_modifier(Modifier::BOLD),
        ))
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let cli = match &app.cli {
        CliState::Checking => Span::styled("checking…", Style::new().fg(Color::Yellow)),
        CliState::Active(u) => Span::styled(format!("{u} [Active]"), Style::new().fg(Color::Green)),
        CliState::Unavailable(e) => Span::styled(
            format!("not logged in ({})", e.chars().take(40).collect::<String>()),
            Style::new().fg(Color::Red),
        ),
    };
    let mut spans = vec![
        Span::styled(
            " az-storage ",
            Style::new().bg(ACCENT).fg(Color::Black).bold(),
        ),
        Span::raw(" │ CLI User: "),
        cli,
        Span::raw(format!(
            " │ Attached Accounts: {}",
            app.config.attached_accounts.len()
        )),
    ];
    if app.is_busy() {
        spans.push(Span::styled(
            format!("  {} working…", SPINNER[app.spinner % SPINNER.len()]),
            Style::new().fg(Color::Yellow),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let hints: &[(&str, &str)] = match app.panel {
        _ if app.filter_editing => &[
            ("type", "Filter"),
            ("↑/↓", "Move"),
            ("Enter", "Apply"),
            ("Ctrl-U", "Clear"),
            ("Esc", "Cancel filter"),
        ],
        ActivePanel::SidebarTree => &[
            ("Tab", "Switch"),
            ("Enter", "Open"),
            ("←/→", "Collapse/Expand"),
            ("a", "Attach ConnStr"),
            ("d", "Detach"),
            ("s/u", "Down/Upload"),
            ("c", "Clear queue"),
            ("r", "Refresh"),
            ("q", "Quit"),
        ],
        ActivePanel::MainItemList => &[
            ("Tab", "Switch"),
            ("Enter", "Open/Preview"),
            ("⌫", "Up"),
            ("/", "Filter"),
            ("o", "Sort"),
            ("d", "Delete"),
            ("s", "Download"),
            ("u", "Upload"),
            ("c", "Clear queue"),
            ("r", "Refresh"),
            ("q", "Quit"),
        ],
        ActivePanel::PreviewInspector => &[
            ("Tab", "Switch"),
            ("↑/↓ PgUp/PgDn", "Scroll"),
            ("Esc", "Back"),
            ("d", "Delete"),
            ("r", "Refresh"),
            ("q", "Quit"),
        ],
    };
    let mut spans: Vec<Span> = Vec::new();
    for (k, v) in hints {
        spans.push(Span::styled(
            format!(" [{k}]"),
            Style::new().fg(ACCENT).bold(),
        ));
        spans.push(Span::raw(format!(" {v}")));
    }
    if !app.transfers.is_empty() {
        spans.push(Span::styled(" [x]", Style::new().fg(Color::Red).bold()));
        spans.push(Span::raw(" Cancel transfers"));
    }
    spans.push(Span::styled(" [m]", Style::new().fg(ACCENT).bold()));
    spans.push(Span::raw(if app.mouse_enabled {
        " Mouse:on"
    } else {
        " Mouse:off"
    }));
    if let Some(s) = app.status_text() {
        spans.push(Span::styled(
            format!("  │ {s}"),
            Style::new().fg(Color::Yellow),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_tree(f: &mut Frame, app: &mut App, area: Rect) {
    let rows = app.rows();
    let items: Vec<ListItem> = rows
        .iter()
        .map(|r| {
            let n = app.node_at(&r.path);
            let indent = "  ".repeat(r.depth);
            let arrow = if n.leaf {
                "  "
            } else if n.expanded {
                "▾ "
            } else {
                "▸ "
            };
            let (icon, style) = match &n.key {
                NodeKey::SubsRoot | NodeKey::AttachedRoot => ("", Style::new().bold()),
                NodeKey::Sub(_) => ("󰄦 ", Style::new().fg(Color::LightBlue)),
                NodeKey::Account(_) => ("󰋊 ", Style::new().fg(Color::LightGreen)),
                NodeKey::Blobs(_) | NodeKey::Queues(_) => ("", Style::new()),
                NodeKey::Container(..) => ("󰆧 ", Style::new()),
                NodeKey::Queue(..) => ("󰁕 ", Style::new()),
                NodeKey::Info(t) if t.starts_with('✗') => ("", Style::new().fg(Color::Red)),
                NodeKey::Info(_) => ("", Style::new().fg(Color::DarkGray).italic()),
            };
            let mut spans = vec![
                Span::raw(indent),
                Span::styled(arrow, Style::new().fg(Color::DarkGray)),
                Span::styled(format!("{icon}{}", n.label), style),
            ];
            if !n.detail.is_empty() {
                spans.push(Span::styled(
                    format!(" {}", n.detail),
                    Style::new().fg(Color::DarkGray),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let list = List::new(items)
        .block(panel_block(
            "Resources Tree".into(),
            app.panel == ActivePanel::SidebarTree,
        ))
        .highlight_style(highlight(app.panel == ActivePanel::SidebarTree));
    app.hit.tree_list = Block::bordered().inner(area);
    f.render_stateful_widget(list, area, &mut app.tree_state);
}

fn highlight(focused: bool) -> Style {
    if focused {
        Style::new()
            .bg(Color::Rgb(40, 60, 90))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new().bg(Color::Rgb(40, 40, 40))
    }
}

fn draw_main(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.panel == ActivePanel::MainItemList;
    let visible = app.visible();
    let (title, items, loading, error): (String, Vec<ListItem>, bool, Option<String>) = match &app
        .main
    {
        MainView::Empty => {
            let p =
                Paragraph::new("\n  Select a container or queue in the tree\n  and press [Enter].")
                    .fg(Color::DarkGray)
                    .block(panel_block("Items List".into(), focused));
            f.render_widget(p, area);
            return;
        }
        MainView::Blobs {
            container,
            prefix,
            entries,
            loading,
            error,
            ..
        } => {
            let width = area.width.saturating_sub(4) as usize;
            let items = visible
                .iter()
                .map(|&i| &entries[i])
                .map(|e| match e {
                    Entry::Parent => {
                        ListItem::new(Line::from(".. (Parent Directory)").fg(Color::DarkGray))
                    }
                    Entry::Dir(d) => ListItem::new(Line::from(vec![
                        Span::styled("󰉋 ", Style::new().fg(Color::Yellow)),
                        Span::styled(
                            d[prefix.len()..].to_string(),
                            Style::new().fg(Color::Yellow),
                        ),
                    ])),
                    Entry::Blob(b) => {
                        let name = &b.name[prefix.len().min(b.name.len())..];
                        let size = human_size(b.size);
                        let pad = width
                            .saturating_sub(2 + name.chars().count() + size.len())
                            .max(1);
                        ListItem::new(Line::from(vec![
                            Span::raw("󰈚 "),
                            Span::raw(name.to_string()),
                            Span::raw(" ".repeat(pad)),
                            Span::styled(size, Style::new().fg(Color::DarkGray)),
                        ]))
                    }
                })
                .collect();
            (
                format!("󰆧 {container}/{prefix} · sort: {}", app.sort.label()),
                items,
                *loading,
                error.clone(),
            )
        }
        MainView::Messages {
            queue,
            messages,
            loading,
            error,
            ..
        } => {
            let items = visible
                .iter()
                .map(|&i| (i, &messages[i]))
                .map(|(i, m)| {
                    let (body, _) = decode_message(&m.text);
                    let snippet: String = body
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .chars()
                        .take(80)
                        .collect();
                    ListItem::new(Line::from(vec![
                        Span::styled(format!("{:>2} ", i + 1), Style::new().fg(Color::DarkGray)),
                        Span::styled(
                            format!("{} ", &m.id[..m.id.len().min(8)]),
                            Style::new().fg(ACCENT),
                        ),
                        Span::raw(snippet),
                    ]))
                })
                .collect();
            (
                format!("󰁕 {queue} (peek · max 32)"),
                items,
                *loading,
                error.clone(),
            )
        }
    };

    let block = panel_block(title, focused);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut list_area = inner;
    if app.filter_editing || !app.filter.is_empty() {
        let [bar, rest] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
        let real = visible
            .iter()
            .filter(|&&i| match &app.main {
                MainView::Blobs { entries, .. } => !matches!(entries[i], Entry::Parent),
                _ => true,
            })
            .count();
        let total = match &app.main {
            MainView::Blobs { entries, .. } => entries
                .iter()
                .filter(|e| !matches!(e, Entry::Parent))
                .count(),
            _ => app.main_total(),
        };
        let mut spans = vec![
            Span::styled(" / ", Style::new().fg(ACCENT).bold()),
            Span::styled(app.filter.clone(), Style::new().fg(Color::White).bold()),
        ];
        if app.filter_editing {
            spans.push(Span::styled("█", Style::new().fg(ACCENT)));
        }
        spans.push(Span::styled(
            format!("  ({real}/{total})"),
            Style::new().fg(Color::DarkGray),
        ));
        f.render_widget(
            Paragraph::new(Line::from(spans)).bg(Color::Rgb(30, 30, 30)),
            bar,
        );
        list_area = rest;
    }
    let inner = list_area;
    if loading || error.is_some() {
        let [msg, rest] = Layout::vertical([
            Constraint::Length(if error.is_some() { 3 } else { 1 }),
            Constraint::Min(0),
        ])
        .areas(inner);
        let p = match &error {
            Some(e) => Paragraph::new(format!("✗ {e}"))
                .fg(Color::Red)
                .wrap(Wrap { trim: true }),
            None => Paragraph::new(format!("{} loading…", SPINNER[app.spinner % SPINNER.len()]))
                .fg(Color::Yellow),
        };
        f.render_widget(p, msg);
        list_area = rest;
    } else if items.is_empty() {
        let msg = if app.filter.is_empty() {
            "(empty)"
        } else {
            "(no matches)"
        };
        f.render_widget(Paragraph::new(msg).fg(Color::DarkGray), inner);
        return;
    }
    let list = List::new(items)
        .highlight_style(highlight(focused))
        .highlight_symbol("> ");
    app.hit.main_list = list_area;
    f.render_stateful_widget(list, list_area, &mut app.main_state);
}

fn kv(k: &str, v: impl Into<String>) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{k}: "), Style::new().fg(ACCENT).bold()),
        Span::raw(v.into()),
    ])
}

fn draw_preview(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.panel == ActivePanel::PreviewInspector;
    let block = panel_block("Content Preview / Details".into(), focused);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let sep = Line::from("─".repeat(inner.width as usize)).fg(Color::DarkGray);
    let mut header: Vec<Line> = Vec::new();
    let mut body: Vec<Line> = Vec::new();

    match &app.main {
        MainView::Blobs {
            account_id,
            container,
            ..
        } => match app.selected_entry() {
            Some(Entry::Blob(b)) => {
                header.push(kv("Name", b.name.clone()));
                header.push(kv(
                    "Size",
                    format!("{} ({} bytes)", human_size(b.size), b.size),
                ));
                header.push(kv("Content-Type", b.content_type.clone()));
                header.push(kv("Last-Modified", b.last_modified.clone()));
                header.push(kv("Blob-Type", b.blob_type.clone()));
                header.push(kv("ETag", b.etag.clone()));
                header.push(sep);
                let key = format!("{account_id}/{container}/{}", b.name);
                match (&app.preview_key, &app.preview) {
                    (Some(k), Some(p)) if *k == key => match p {
                        PreviewState::Loading => {
                            body.push(Line::from("loading content…").fg(Color::Yellow))
                        }
                        PreviewState::Failed(e) => {
                            body.push(Line::from(format!("✗ {e}")).fg(Color::Red))
                        }
                        PreviewState::Ready { text, truncated } => {
                            if *truncated {
                                body.push(
                                    Line::from("(preview limited to first 512 KB)")
                                        .fg(Color::Yellow),
                                );
                            }
                            body.extend(text.lines().map(|l| Line::from(l.to_string())));
                        }
                    },
                    _ => body.push(
                        Line::from("Press [Enter] to load content preview").fg(Color::DarkGray),
                    ),
                }
            }
            Some(Entry::Dir(d)) => {
                header.push(kv("Directory", d.clone()));
                let key = app.selected_dir_key().unwrap_or_default();
                match app.dir_cache.get(&key) {
                    None | Some(DirPreview::Loading) => body.push(
                        Line::from(format!(
                            "{} loading contents…",
                            SPINNER[app.spinner % SPINNER.len()]
                        ))
                        .fg(Color::Yellow),
                    ),
                    Some(DirPreview::Failed(e)) => {
                        header.push(sep);
                        body.push(Line::from(format!("✗ {e}")).fg(Color::Red));
                    }
                    Some(DirPreview::Ready(entries)) => {
                        let items = sorted_entries(entries, app.sort);
                        let (dirs, files): (Vec<&Entry>, Vec<&Entry>) =
                            items.iter().partition(|e| matches!(e, Entry::Dir(_)));
                        let total: u64 = files
                            .iter()
                            .map(|e| match e {
                                Entry::Blob(b) => b.size,
                                _ => 0,
                            })
                            .sum();
                        header.push(kv(
                            "Contents",
                            format!(
                                "{} folders · {} files · {}",
                                dirs.len(),
                                files.len(),
                                human_size(total)
                            ),
                        ));
                        header.push(sep);
                        if items.is_empty() {
                            body.push(Line::from("(empty)").fg(Color::DarkGray));
                        }
                        let width = inner.width as usize;
                        for e in items {
                            match e {
                                Entry::Dir(sub) => body.push(Line::from(vec![
                                    Span::styled("󰉋 ", Style::new().fg(Color::Yellow)),
                                    Span::styled(
                                        rel_name(sub, d).to_string(),
                                        Style::new().fg(Color::Yellow),
                                    ),
                                ])),
                                Entry::Blob(b) => {
                                    let name = rel_name(&b.name, d);
                                    let size = human_size(b.size);
                                    let pad = width
                                        .saturating_sub(2 + name.chars().count() + size.len())
                                        .max(1);
                                    body.push(Line::from(vec![
                                        Span::raw("󰈚 "),
                                        Span::raw(name.to_string()),
                                        Span::raw(" ".repeat(pad)),
                                        Span::styled(size, Style::new().fg(Color::DarkGray)),
                                    ]));
                                }
                                Entry::Parent => {}
                            }
                        }
                    }
                }
            }
            _ => {}
        },
        MainView::Messages { .. } => {
            if let Some(m) = app.selected_message() {
                let (text, was_b64) = decode_message(&m.text);
                header.push(kv("Message-Id", m.id.clone()));
                header.push(kv("Inserted", m.insertion_time.clone()));
                header.push(kv("Expires", m.expiration_time.clone()));
                header.push(kv("Dequeue-Count", m.dequeue_count.to_string()));
                header.push(kv(
                    "Encoding",
                    if was_b64 {
                        "base64 (decoded)"
                    } else {
                        "plain text"
                    },
                ));
                header.push(sep);
                body.extend(text.lines().map(|l| Line::from(l.to_string())));
            }
        }
        MainView::Empty => {}
    }

    if header.is_empty() && body.is_empty() {
        f.render_widget(
            Paragraph::new("Nothing selected").fg(Color::DarkGray),
            inner,
        );
        return;
    }

    // Scroll per righe logiche del body: si renderizza solo la porzione visibile.
    let max = body.len().saturating_sub(1) as u16;
    app.preview_scroll = app.preview_scroll.min(max);
    let avail = (inner.height as usize).saturating_sub(header.len());
    let mut lines = header;
    lines.extend(
        body.into_iter()
            .skip(app.preview_scroll as usize)
            .take(avail.max(1)),
    );
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn draw_transfers(f: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    let bar_w = 24usize;
    let lines: Vec<Line> = app
        .transfers
        .values()
        .take(area.height as usize)
        .map(|p| {
            let ratio = if p.total == 0 {
                1.0
            } else {
                (p.done as f64 / p.total as f64).min(1.0)
            };
            let filled = (ratio * bar_w as f64).round() as usize;
            let (arrow, color) = if p.upload {
                ("⇡ Upload", Color::Magenta)
            } else {
                ("⇣ Download", Color::Green)
            };
            let mut spans = vec![
                Span::styled(format!(" {arrow} "), Style::new().fg(color).bold()),
                Span::raw(format!("{} ", p.label)),
                Span::styled("█".repeat(filled), Style::new().fg(color)),
                Span::styled("░".repeat(bar_w - filled), Style::new().fg(Color::DarkGray)),
                Span::raw(format!(
                    " {:>3.0}%  {}/{}",
                    ratio * 100.0,
                    human_size(p.done),
                    human_size(p.total)
                )),
            ];
            if p.files_total > 1 {
                spans.push(Span::raw(format!(
                    "  ({}/{} files)",
                    p.files_done, p.files_total
                )));
            }
            if !p.current.is_empty() && p.files_total > 1 {
                spans.push(Span::styled(
                    format!("  {}", p.current),
                    Style::new().fg(Color::DarkGray),
                ));
            }
            Line::from(spans)
        })
        .collect();
    f.render_widget(Paragraph::new(lines), area);
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn draw_modal(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let mut hit = app.hit;
    match &app.modal {
        ModalState::None => {}
        ModalState::ErrorPopup(msg) => {
            let r = centered(area, 70, 10);
            hit.modal = r;
            f.render_widget(Clear, r);
            let p = Paragraph::new(vec![
                Line::from(msg.clone()),
                Line::from(""),
                Line::from("[Enter/Esc/Click] Close").fg(Color::DarkGray),
            ])
            .wrap(Wrap { trim: true })
            .block(
                Block::bordered()
                    .border_type(BorderType::Thick)
                    .border_style(Style::new().fg(Color::Red))
                    .title(" Error "),
            );
            f.render_widget(p, r);
        }
        ModalState::ConfirmDeleteDialog { target, .. } => {
            let r = centered(area, 70, 7);
            hit.modal = r;
            f.render_widget(Clear, r);
            let block = Block::bordered()
                .border_type(BorderType::Thick)
                .border_style(Style::new().fg(Color::Yellow))
                .title(" Confirm [y/N] ");
            let inner = block.inner(r);
            f.render_widget(block, r);
            let [text, buttons] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
            f.render_widget(
                Paragraph::new(Line::from(target.clone()).bold()).wrap(Wrap { trim: true }),
                text,
            );
            // Pulsanti cliccabili: " [y] Yes "  " [N] No "
            let yes = Rect {
                width: 9.min(buttons.width),
                ..buttons
            };
            let no = Rect {
                x: buttons.x + 11,
                width: 8.min(buttons.width.saturating_sub(11)),
                ..buttons
            };
            hit.confirm_yes = yes;
            hit.confirm_no = no;
            f.render_widget(
                Paragraph::new(" [y] Yes ")
                    .style(Style::new().bg(Color::Red).fg(Color::White).bold()),
                yes,
            );
            f.render_widget(
                Paragraph::new(" [N] No ").style(Style::new().bg(ACCENT).fg(Color::Black).bold()),
                no,
            );
        }
        ModalState::TransferDialog(form) => {
            let r = centered(area, 90, 11);
            hit.modal = r;
            f.render_widget(Clear, r);
            let (title, what, label) = match &form.kind {
                TransferKind::Download {
                    container, spec, ..
                } => (
                    " Download ",
                    match spec {
                        crate::worker::DownloadSpec::Blob(n) => format!("Blob: {container}/{n}"),
                        crate::worker::DownloadSpec::Prefix(p) => {
                            format!("Folder (recursive): {container}/{p}")
                        }
                    },
                    "Local destination",
                ),
                TransferKind::Upload {
                    container, prefix, ..
                } => (
                    " Upload ",
                    format!("Target: {container}/{prefix}"),
                    "Local file or folder (drag & drop from Finder works)",
                ),
            };
            let block = Block::bordered()
                .border_type(BorderType::Thick)
                .border_style(Style::new().fg(ACCENT))
                .title(title);
            let inner = block.inner(r);
            f.render_widget(block, r);
            let [l_what, l_input, l_opt, l_err, l_keys] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Length(3),
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .areas(inner);
            f.render_widget(Paragraph::new(what).bold(), l_what);
            input(f, l_input, label, &form.path, true, false);
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("[Ctrl-O]", Style::new().fg(ACCENT).bold()),
                    Span::raw(" Overwrite existing: "),
                    if form.overwrite {
                        Span::styled("[x] yes", Style::new().fg(Color::Red).bold())
                    } else {
                        Span::raw("[ ] no (skip existing)")
                    },
                ])),
                l_opt,
            );
            if let Some(e) = &form.error {
                f.render_widget(
                    Paragraph::new(e.clone())
                        .fg(Color::Red)
                        .wrap(Wrap { trim: true }),
                    l_err,
                );
            }
            f.render_widget(
                Paragraph::new("[Enter] Start  [Ctrl-U] Clear  [Esc] Cancel").fg(Color::DarkGray),
                l_keys,
            );
        }
        ModalState::AddConnectionStringDialog(form) => {
            let r = centered(area, 90, 12);
            hit.modal = r;
            f.render_widget(Clear, r);
            let block = Block::bordered()
                .border_type(BorderType::Thick)
                .border_style(Style::new().fg(ACCENT))
                .title(" Attach Storage Account (Connection String) ");
            let inner = block.inner(r);
            f.render_widget(block, r);
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Length(2),
                    Constraint::Min(1),
                ])
                .split(inner);
            hit.form_fields = [rows[0], rows[1]];
            input(
                f,
                rows[0],
                "Profile name",
                &form.name,
                form.field == 0,
                false,
            );
            input(
                f,
                rows[1],
                "Connection string or SAS URL",
                &form.conn,
                form.field == 1,
                true,
            );
            if let Some(e) = &form.error {
                f.render_widget(
                    Paragraph::new(e.clone())
                        .fg(Color::Red)
                        .wrap(Wrap { trim: true }),
                    rows[2],
                );
            } else {
                f.render_widget(
                    Paragraph::new("e.g. DefaultEndpointsProtocol=https;AccountName=…;AccountKey=…  ·  https://acct.blob.core.windows.net/container?sv=…&sig=…  ·  UseDevelopmentStorage=true")
                        .fg(Color::DarkGray)
                        .wrap(Wrap { trim: true }),
                    rows[2],
                );
            }
            f.render_widget(
                Paragraph::new(
                    "[Tab] Next field  [Enter] Save  [Ctrl-U] Clear field  [Esc] Cancel",
                )
                .fg(Color::DarkGray),
                rows[3],
            );
        }
    }
    app.hit = hit;
}

fn input(f: &mut Frame, area: Rect, label: &str, value: &str, active: bool, secret: bool) {
    let style = if active {
        Style::new().fg(ACCENT)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    let block = Block::bordered()
        .border_style(style)
        .title(format!(" {label} "));
    let w = block.inner(area).width.saturating_sub(1) as usize;
    // Maschera le chiavi: mostra solo la coda del valore se non è attivo.
    let shown: String = if secret && !active {
        mask_secret(value)
    } else {
        value.to_string()
    };
    let chars: Vec<char> = shown.chars().collect();
    let visible: String = chars[chars.len().saturating_sub(w)..].iter().collect();
    let mut spans = vec![Span::raw(visible)];
    if active {
        spans.push(Span::styled("█", Style::new().fg(ACCENT)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
}

fn mask_secret(cs: &str) -> String {
    // SAS URL: nasconde la firma.
    if let Some((base, q)) = cs.split_once('?')
        && cs.starts_with("http")
    {
        let q: Vec<String> = q
            .split('&')
            .map(|kv| {
                if kv.starts_with("sig=") {
                    "sig=••••••".to_string()
                } else {
                    kv.to_string()
                }
            })
            .collect();
        return format!("{base}?{}", q.join("&"));
    }
    cs.split(';')
        .map(|p| match p.split_once('=') {
            Some((k, _))
                if k.eq_ignore_ascii_case("AccountKey")
                    || k.eq_ignore_ascii_case("SharedAccessSignature") =>
            {
                format!("{k}=••••••")
            }
            _ => p.to_string(),
        })
        .collect::<Vec<_>>()
        .join(";")
}
