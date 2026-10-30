//! Compact live view, drawn inline under the prompt (no full-screen takeover):
//! a temperature bar, a temperature-over-time chart and a line of stats.
//! It's only a window onto the background heater, so closing it can leave
//! the heater running.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Chart, Dataset, GraphType, Paragraph};
use ratatui::{DefaultTerminal, Frame, TerminalOptions, Viewport};

use crate::client::{self, Info, Status};

const HEIGHT: u16 = 11;
const ORANGE: Color = Color::Rgb(255, 150, 40);
const MENU: [&str; 3] = ["continue", "run in background", "quit"];

pub enum Exit {
    Background,
    Quit,
    /// The heater was stopped from elsewhere (e.g. `warm off`).
    Gone,
}

pub fn run(info: &Info) -> io::Result<Exit> {
    let mut term = ratatui::try_init_with_options(TerminalOptions {
        viewport: Viewport::Inline(HEIGHT),
    })?;
    let result = event_loop(&mut term, info);
    // leave the shell as we found it: wipe our lines, cursor back at the top.
    // Erasing to the end of the screen as well means the exit message can't
    // land on top of leftover text, even if the terminal placed our lines a
    // row off from where we expected.
    let top = term.get_frame().area().y;
    let _ = term.clear();
    let _ = term.set_cursor_position((0, top));
    let _ = term.show_cursor();
    ratatui::restore();
    let mut out = io::stdout();
    let _ = write!(out, "\r\x1b[J");
    let _ = out.flush();
    result
}

fn event_loop(term: &mut DefaultTerminal, info: &Info) -> io::Result<Exit> {
    let mut status = Status::default();
    let mut menu: Option<usize> = None;
    let mut next_poll = Instant::now();

    loop {
        if Instant::now() >= next_poll {
            match client::status(info) {
                Ok(s) => status = s,
                Err(_) => return Ok(Exit::Gone),
            }
            next_poll = Instant::now() + Duration::from_secs(1);
        }
        term.draw(|f| draw(f, &status, menu))?;

        if !event::poll(next_poll.saturating_duration_since(Instant::now()))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue; // resize etc: just redraw
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            client::stop(info);
            return Ok(Exit::Quit);
        }

        match menu {
            // the exit menu: continue / run in background / quit
            Some(sel) => {
                let choice = match key.code {
                    KeyCode::Left | KeyCode::Up | KeyCode::BackTab => {
                        menu = Some((sel + MENU.len() - 1) % MENU.len());
                        None
                    }
                    KeyCode::Right | KeyCode::Down | KeyCode::Tab => {
                        menu = Some((sel + 1) % MENU.len());
                        None
                    }
                    KeyCode::Enter => Some(sel),
                    KeyCode::Char('c') | KeyCode::Esc => Some(0),
                    KeyCode::Char('b') => Some(1),
                    KeyCode::Char('q') => Some(2),
                    _ => None,
                };
                match choice {
                    Some(0) => menu = None,
                    Some(1) => return Ok(Exit::Background),
                    Some(2) => {
                        client::stop(info);
                        return Ok(Exit::Quit);
                    }
                    _ => {}
                }
            }
            None => {
                let cmd = match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => {
                        menu = Some(0);
                        None
                    }
                    KeyCode::Up => Some(format!("target {}", status.target + 0.5)),
                    KeyCode::Down => Some(format!("target {}", status.target - 0.5)),
                    KeyCode::Right => Some(format!("cap {}", status.max_duty + 0.1)),
                    KeyCode::Left => Some(format!("cap {}", status.max_duty - 0.1)),
                    _ => None,
                };
                if let Some(cmd) = cmd {
                    let _ = client::request(info, &cmd);
                    next_poll = Instant::now(); // show the change right away
                }
            }
        }
    }
}

fn draw(f: &mut Frame, s: &Status, menu: Option<usize>) {
    let [head, bar, stats, chart, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(f.area());

    draw_header(f, head, s);
    draw_bar(f, bar, s);
    draw_stats(f, stats, s);
    draw_chart(f, chart, s);
    draw_footer(f, foot, menu);
}

fn draw_header(f: &mut Frame, area: Rect, s: &Status) {
    let (m, sec) = (s.uptime_s / 60, s.uptime_s % 60);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" warm ", Style::new().fg(ORANGE).bold()),
            Span::styled(" ● ", state_style(&s.state)),
            Span::styled(s.state.clone(), state_style(&s.state)),
            Span::raw(format!("   {}", eta(s))).fg(Color::White),
        ])),
        area,
    );
    f.render_widget(
        Paragraph::new(format!("{} sensor · {m}m {sec:02}s ", s.sensor))
            .dim()
            .right_aligned(),
        area,
    );
}

/// `30.8°C |▄▄▄▄▄▄▄▄▄▄▄▄▄      ┃     | 34.0°C`: the fill is the temperature
/// now, the marker the target, on a fixed scale (from just below the
/// coldest reading, at least 10°C wide). Changing the target moves only the
/// marker; the fill moves only when the temperature does.
fn draw_bar(f: &mut Frame, area: Rect, s: &Status) {
    let [now, bar, target] = Layout::horizontal([
        Constraint::Length(9),
        Constraint::Fill(1),
        Constraint::Length(10),
    ])
    .areas(area);

    let temp = s.temp.map_or("  --".into(), |t| format!("{t:>5.1}°C"));
    f.render_widget(Paragraph::new(format!(" {temp}")).bold(), now);
    f.render_widget(
        Paragraph::new(format!(" → {:.1}°C", s.target)).fg(Color::White),
        target,
    );

    let (lo, hi) = bar_scale(s);
    let width = bar.width.saturating_sub(2) as usize;
    let cell = |t: f32| (((t - lo) / (hi - lo)).clamp(0.0, 1.0) * width as f32).round() as usize;
    let filled = s.temp.map_or(0, cell);
    let mark = cell(s.target).min(width.saturating_sub(1));

    let mut spans = vec![Span::raw("|")];
    for i in 0..width {
        spans.push(if i == mark {
            Span::styled("┃", Style::new().fg(Color::White).bold())
        } else if i < filled {
            Span::styled("▄", Style::new().fg(heat(i as f32 / width.max(1) as f32)))
        } else {
            Span::raw(" ")
        });
    }
    spans.push(Span::raw("|"));
    f.render_widget(Paragraph::new(Line::from(spans)), bar);
}

/// Bar range in °C: starts a degree below the coldest reading so far, spans
/// at least 10°C, and only grows if the target or temperature goes past it.
fn bar_scale(s: &Status) -> (f32, f32) {
    let readings = s.history.iter().flatten().copied().chain(s.temp);
    let (cold, hot) = readings.fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), t| {
        (a.min(t), b.max(t))
    });
    let lo = (cold.min(s.target) - 1.0).floor();
    let hi = (lo + 10.0).max((hot.max(s.target) + 1.0).ceil());
    (lo, hi)
}

fn draw_stats(f: &mut Frame, area: Rect, s: &Status) {
    let sep = || Span::raw(" · ").dim();
    let mut spans = vec![
        Span::raw(" heater ").dim(),
        Span::raw(format!("{:.0}%", s.duty * 100.0)).fg(ORANGE),
        Span::raw(format!(" (max {:.0}%)", s.max_duty * 100.0)).dim(),
        sep(),
        Span::raw("cpu ").dim(),
        Span::raw(format!("{:.0}%", s.cpu_pct)),
        Span::raw(format!(" (warm {:.0}%)", s.ours_pct)).dim(),
    ];
    if let Some(g) = s.gpu.filter(|g| *g > 0.0) {
        spans.extend([
            sep(),
            Span::raw("gpu ").dim(),
            Span::raw(format!("{:.0}%", g * 100.0)).fg(ORANGE),
        ]);
    }
    if let Some(rate) = rate_per_min(s) {
        spans.extend([
            sep(),
            Span::raw(format!("{rate:+.1}°C/min")).fg(if rate > 0.05 {
                ORANGE
            } else {
                Color::Reset
            }),
        ]);
    }
    if let Some(c) = s.cpu_c {
        spans.extend([
            sep(),
            Span::raw("chip ").dim(),
            Span::raw(format!("{c:.0}°C")),
        ]);
    }
    if let Some(p) = s.charge_pct {
        let src = if s.on_battery { "" } else { " ⚡" };
        spans.extend([
            sep(),
            Span::raw("battery ").dim(),
            Span::raw(format!("{p:.0}%{src}")),
        ]);
    }
    if let (true, Some(w)) = (s.on_battery, s.watts) {
        spans.extend([sep(), Span::raw(format!("{w:.1} W"))]);
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// How fast the temperature is moving, in °C per minute: the slope of a
/// straight line fitted through the last five minutes. The battery sensor
/// moves in small steps, so comparing two single readings is mostly noise.
fn rate_per_min(s: &Status) -> Option<f32> {
    let n = s.history.len();
    let recent = s.history.iter().skip(n.saturating_sub(300));
    let points: Vec<(f32, f32)> = recent
        .enumerate()
        .filter_map(|(i, t)| t.map(|t| (i as f32, t)))
        .collect();
    if points.len() < 60 {
        return None; // under a minute: too little to go on
    }
    let k = points.len() as f32;
    let (mx, my) = points
        .iter()
        .fold((0.0, 0.0), |(a, b), (x, y)| (a + x / k, b + y / k));
    let (sxy, sxx) = points.iter().fold((0.0, 0.0), |(a, b), (x, y)| {
        (a + (x - mx) * (y - my), b + (x - mx) * (x - mx))
    });
    Some(sxy / sxx * 60.0)
}

/// Rough time to reach the target at the current pace, recomputed every
/// second as the pace changes. The battery warms slowly at first and eases
/// in at the end, so treat it as a guide.
fn eta(s: &Status) -> String {
    let Some(temp) = s.temp else {
        return String::new();
    };
    let gap = s.target - temp;
    if gap.abs() <= 0.2 {
        return String::new(); // there, or close enough to be holding
    }
    let rate = rate_per_min(s);
    let (verb, moving) = if gap > 0.0 {
        ("warm", rate.filter(|r| *r > 0.01))
    } else {
        ("cool", rate.filter(|r| *r < -0.01))
    };
    let Some(rate) = moving else {
        return match rate {
            None => "estimating time…".into(),
            Some(_) if gap > 0.0 => "not rising yet".into(),
            Some(_) => "not cooling yet".into(),
        };
    };
    let mins = gap / rate;
    let when = match mins {
        m if m < 1.0 => "under a minute".to_string(),
        m if m < 60.0 => format!("~{m:.0} min"),
        m if m < 180.0 => format!("~{}h {:02}m", m as u32 / 60, m as u32 % 60),
        _ => "over 3 hours".to_string(),
    };
    format!("{when} to {verb} to {:.1}°C", s.target)
}

/// Temperature over time, with the target as a faint line. Time runs left
/// to right: the line starts at the left edge and grows rightward, the axis
/// widening a minute at a time, and scrolls once the history is full.
fn draw_chart(f: &mut Frame, area: Rect, s: &Status) {
    let n = s.history.len();
    let points: Vec<(f64, f64)> = s
        .history
        .iter()
        .enumerate()
        .filter_map(|(i, t)| t.map(|t| (i as f64, t as f64)))
        .collect();
    let span = n.div_ceil(60).max(1) * 60;
    let target = s.target as f64;
    let target_line = [(0.0, target), (span as f64, target)];
    // seconds since start at the left edge (non-zero once scrolling)
    let start = s.uptime_s.saturating_sub(n.saturating_sub(1) as u64);

    let (mut lo, mut hi) = (target, target);
    for &(_, t) in &points {
        lo = lo.min(t);
        hi = hi.max(t);
    }
    // a little headroom, and at least a degree of range so noise stays flat
    let pad = ((1.0 - (hi - lo)) / 2.0).max(0.2);
    let (lo, hi) = (lo - pad, hi + pad);

    let clock = |t: u64| format!("{}:{:02}", t / 60, t % 60);
    let chart = Chart::new(vec![
        Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(Color::DarkGray))
            .data(&target_line),
        Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(ORANGE))
            .data(&points),
    ])
    .x_axis(
        Axis::default()
            .bounds([0.0, span as f64])
            .labels([clock(start), clock(start + span as u64)])
            .style(Style::new().dim()),
    )
    .y_axis(
        Axis::default()
            .bounds([lo, hi])
            .labels([format!("{lo:.1}°"), format!("{hi:.1}°")])
            .style(Style::new().dim()),
    );
    f.render_widget(chart, area);
}

fn draw_footer(f: &mut Frame, area: Rect, menu: Option<usize>) {
    let line = match menu {
        None => Line::from(" ↑↓ target temp ±0.5°C   ←→ max heater power ±10%   q exit").dim(),
        Some(sel) => {
            let mut spans = vec![Span::raw(" leave warm? ").bold()];
            for (i, item) in MENU.iter().enumerate() {
                let style = if i == sel {
                    Style::new().fg(Color::Black).bg(ORANGE).bold()
                } else {
                    Style::new().dim()
                };
                spans.push(Span::styled(format!(" {item} "), style));
                spans.push(Span::raw(" "));
            }
            spans.push(Span::raw("  ←→ choose · enter").dim());
            Line::from(spans)
        }
    };
    f.render_widget(Paragraph::new(line), area);
}

/// Cold-to-hot colour for a position 0..=1 along the bar.
fn heat(x: f32) -> Color {
    let stops = [
        (70.0, 150.0, 255.0),
        (255.0, 190.0, 40.0),
        (255.0, 60.0, 40.0),
    ];
    let x = x.clamp(0.0, 1.0) * 2.0;
    let (a, b, t) = if x < 1.0 {
        (stops[0], stops[1], x)
    } else {
        (stops[1], stops[2], x - 1.0)
    };
    let mix = |p: f32, q: f32| (p + (q - p) * t) as u8;
    Color::Rgb(mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

fn state_style(state: &str) -> Style {
    match state {
        s if s.starts_with("warming up") => Style::new().fg(Color::Rgb(255, 90, 40)).bold(),
        s if s.starts_with("heating") || s.starts_with("holding") => Style::new().fg(ORANGE),
        "at target" => Style::new().fg(Color::Green),
        _ => Style::new().fg(Color::Yellow),
    }
}
