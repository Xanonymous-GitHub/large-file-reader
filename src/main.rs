use std::{
    env,
    error::Error,
    fs,
    io::{self, Stdout},
    path::PathBuf,
    sync::mpsc::TryRecvError,
    time::Duration,
};

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use large_json_reader::{
    FormatHandle, FormatUpdate, LargeFile, SearchHandle, SearchMatch, SearchUpdate, TokenKind,
    VisualLine, Window, highlight_json_line, start_format, start_search, syntax_for_path,
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, Style as SyntectStyle, Theme, ThemeSet},
    parsing::SyntaxSet,
};

type Tui = Terminal<CrosstermBackend<Stdout>>;

fn main() -> Result<(), Box<dyn Error>> {
    let path = file_argument()?;
    run(path)
}

fn file_argument() -> Result<PathBuf, Box<dyn Error>> {
    let mut args = env::args_os();
    let program = args.next().unwrap_or_default();
    let Some(path) = args.next() else {
        eprintln!("Usage: {} <file.json>", PathBuf::from(program).display());
        std::process::exit(2);
    };

    if args.next().is_some() {
        eprintln!("Usage: {} <file.json>", PathBuf::from(program).display());
        std::process::exit(2);
    }

    Ok(PathBuf::from(path))
}

fn run(path: PathBuf) -> Result<(), Box<dyn Error>> {
    let mut app = App::new(path)?;
    let mut terminal = enter_tui()?;

    let app_result = run_app(&mut terminal, &mut app);
    let restore_result = restore_tui(&mut terminal);

    app_result?;
    restore_result?;
    Ok(())
}

fn enter_tui() -> io::Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.hide_cursor()?;
    Ok(terminal)
}

fn restore_tui(terminal: &mut Tui) -> io::Result<()> {
    let raw_result = disable_raw_mode();
    let screen_result = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let cursor_result = terminal.show_cursor();

    raw_result?;
    screen_result?;
    cursor_result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Normal,
    Search,
    Command,
}

struct App {
    path: PathBuf,
    active_path: PathBuf,
    reader: LargeFile,
    offset: u64,
    line_number: u64,
    mode: InputMode,
    search_input: String,
    command_input: String,
    pending_count: String,
    search_query: String,
    search_handle: Option<SearchHandle>,
    search_matches: Vec<SearchMatch>,
    current_match: Option<usize>,
    format_requested: bool,
    format_handle: Option<FormatHandle>,
    formatted_path: Option<PathBuf>,
    syntax_highlighter: SyntaxHighlighter,
    message: String,
}

impl App {
    fn new(path: PathBuf) -> io::Result<Self> {
        let reader = LargeFile::open(&path)?;
        Ok(Self {
            active_path: path.clone(),
            path,
            reader,
            offset: 0,
            line_number: 1,
            mode: InputMode::Normal,
            search_input: String::new(),
            command_input: String::new(),
            pending_count: String::new(),
            search_query: String::new(),
            search_handle: None,
            search_matches: Vec::new(),
            current_match: None,
            format_requested: false,
            format_handle: None,
            formatted_path: None,
            syntax_highlighter: SyntaxHighlighter::new(),
            message: "ready".to_owned(),
        })
    }

    fn poll_background(&mut self) -> io::Result<()> {
        self.poll_search();
        self.poll_format()
    }

    fn poll_search(&mut self) {
        let mut updates = Vec::new();
        let mut finished = false;

        if let Some(handle) = &self.search_handle {
            loop {
                match handle.try_recv() {
                    Ok(update) => updates.push(update),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        finished = true;
                        break;
                    }
                }
            }
        }

        for update in updates {
            match update {
                SearchUpdate::Found(found) => {
                    self.search_matches.push(found);
                    if self.current_match.is_none() {
                        self.current_match = Some(0);
                        self.offset = self.search_matches[0].offset;
                        self.line_number = self
                            .reader
                            .line_number_at_offset(self.offset)
                            .unwrap_or(self.line_number);
                    }
                    self.message = format!(
                        "match {}/{} for /{}",
                        self.current_match.map_or(1, |index| index + 1),
                        self.search_matches.len(),
                        self.search_query
                    );
                }
                SearchUpdate::Progress { bytes_scanned } => {
                    self.message = format!(
                        "searching /{}; scanned {}",
                        self.search_query,
                        human_bytes(bytes_scanned)
                    );
                }
                SearchUpdate::Finished { matches, .. } => {
                    finished = true;
                    self.message = if matches == 0 {
                        format!("no match for /{}", self.search_query)
                    } else {
                        format!("search done: {matches} matches for /{}", self.search_query)
                    };
                }
                SearchUpdate::Failed(message) => {
                    finished = true;
                    self.message = format!("search failed: {message}");
                }
            }
        }

        if finished {
            self.search_handle = None;
        }
    }

    fn poll_format(&mut self) -> io::Result<()> {
        let mut updates = Vec::new();
        let mut finished = false;

        if let Some(handle) = &self.format_handle {
            loop {
                match handle.try_recv() {
                    Ok(update) => updates.push(update),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        finished = true;
                        break;
                    }
                }
            }
        }

        for update in updates {
            match update {
                FormatUpdate::Progress { bytes_read } => {
                    self.message = format!(
                        "formatting in background; read {}; raw view active",
                        human_bytes(bytes_read)
                    );
                }
                FormatUpdate::Finished { path, bytes_read } => {
                    finished = true;
                    self.formatted_path = Some(path.clone());
                    if self.format_requested {
                        self.switch_to_path(
                            path,
                            format!("formatted view ready; read {}", human_bytes(bytes_read)),
                        )?;
                    } else {
                        self.message = "formatted view ready; press f to open".to_owned();
                    }
                }
                FormatUpdate::Failed(message) => {
                    finished = true;
                    self.format_requested = false;
                    self.message = format!("format failed: {message}");
                }
            }
        }

        if finished {
            self.format_handle = None;
        }

        Ok(())
    }

    fn move_down(&mut self, window: &Window) {
        if let Some(line) = window.lines.get(1) {
            self.offset = line.start_offset;
            self.line_number = line.line_number;
        } else if let Some(line) = window.lines.first() {
            self.offset = line.next_offset.min(window.file_len);
            self.line_number = line.next_line_number;
        }
    }

    fn move_up(&mut self, width: usize) -> io::Result<()> {
        self.offset = self.reader.previous_visual_offset(self.offset, width)?;
        self.line_number = self.reader.line_number_at_offset(self.offset)?;
        Ok(())
    }

    fn page_down(&mut self, window: &Window) {
        if let Some(line) = window.lines.last() {
            self.offset = line.next_offset.min(window.file_len);
            self.line_number = line.next_line_number;
        }
    }

    fn page_up(&mut self, width: usize, height: usize) -> io::Result<()> {
        for _ in 0..height {
            let previous = self.reader.previous_visual_offset(self.offset, width)?;
            self.offset = previous;
            if self.offset == 0 {
                break;
            }
        }
        self.line_number = self.reader.line_number_at_offset(self.offset)?;
        Ok(())
    }

    fn half_down(&mut self, window: &Window) {
        let target = window.lines.len().saturating_div(2).max(1);
        if let Some(line) = window.lines.get(target) {
            self.offset = line.start_offset;
            self.line_number = line.line_number;
        } else {
            self.page_down(window);
        }
    }

    fn half_up(&mut self, width: usize, height: usize) -> io::Result<()> {
        self.page_up(width, height.saturating_div(2).max(1))
    }

    fn go_top(&mut self) {
        self.offset = 0;
        self.line_number = 1;
    }

    fn go_end(&mut self, width: usize, height: usize) -> io::Result<()> {
        self.offset = self.reader.near_end_offset(width, height)?;
        self.line_number = self.reader.line_number_at_offset(self.offset)?;
        Ok(())
    }

    fn jump_to_line(&mut self, line_number: u64) -> io::Result<()> {
        let target = line_number.max(1);
        self.offset = self.reader.line_offset(target)?;
        self.line_number = self.reader.line_number_at_offset(self.offset)?;
        self.message = format!("line {}", self.line_number);
        Ok(())
    }

    fn enter_search(&mut self) {
        self.mode = InputMode::Search;
        self.search_input.clear();
        self.pending_count.clear();
        self.message = "enter search query".to_owned();
    }

    fn enter_command(&mut self) {
        self.mode = InputMode::Command;
        self.command_input.clear();
        self.pending_count.clear();
        self.message = "enter command".to_owned();
    }

    fn finish_command(&mut self) -> io::Result<bool> {
        let command = self.command_input.trim().to_owned();
        self.mode = InputMode::Normal;
        if command == "q" {
            return Ok(true);
        }
        if let Ok(line_number) = command.parse::<u64>() {
            self.jump_to_line(line_number)?;
        } else if command.is_empty() {
            self.message = "empty command ignored".to_owned();
        } else {
            self.message = format!("unknown command: {command}");
        }
        Ok(false)
    }

    fn begin_search(&mut self) -> io::Result<()> {
        self.mode = InputMode::Normal;
        if self.search_input.is_empty() {
            self.message = "empty search ignored".to_owned();
            return Ok(());
        }

        self.search_query.clone_from(&self.search_input);
        self.search_matches.clear();
        self.current_match = None;
        self.search_handle = Some(start_search(
            self.active_path.clone(),
            self.search_query.clone(),
            self.offset,
        )?);
        self.message = format!("searching /{} in background", self.search_query);
        Ok(())
    }

    fn next_match(&mut self) {
        self.jump_match(1);
    }

    fn previous_match(&mut self) {
        self.jump_match(-1);
    }

    fn jump_match(&mut self, direction: isize) {
        if self.search_matches.is_empty() {
            self.message = if self.search_handle.is_some() {
                "search running; no match yet".to_owned()
            } else if self.search_query.is_empty() {
                "no active search".to_owned()
            } else {
                format!("no match for /{}", self.search_query)
            };
            return;
        }

        let len = self.search_matches.len();
        let current = self.current_match.unwrap_or(0);
        let next = if direction.is_negative() {
            current.checked_sub(1).unwrap_or(len - 1)
        } else {
            (current + 1) % len
        };
        self.current_match = Some(next);
        self.offset = self.search_matches[next].offset;
        self.line_number = self
            .reader
            .line_number_at_offset(self.offset)
            .unwrap_or(self.line_number);
        self.message = format!("match {}/{} for /{}", next + 1, len, self.search_query);
    }

    fn toggle_format(&mut self) -> io::Result<()> {
        if self.format_requested {
            self.format_requested = false;
            return self.switch_to_path(self.path.clone(), "raw view".to_owned());
        }

        self.format_requested = true;
        if let Some(path) = self.formatted_path.clone() {
            return self.switch_to_path(path, "formatted view".to_owned());
        }

        if self.format_handle.is_none() {
            self.format_handle = Some(start_format(self.path.clone())?);
        }
        self.message = "formatting in background; raw view active".to_owned();
        Ok(())
    }

    fn switch_to_path(&mut self, path: PathBuf, message: String) -> io::Result<()> {
        self.reader = LargeFile::open(&path)?;
        self.active_path = path;
        self.offset = 0;
        self.line_number = 1;
        self.search_handle = None;
        self.search_matches.clear();
        self.current_match = None;
        self.search_query.clear();
        self.search_input.clear();
        self.command_input.clear();
        self.pending_count.clear();
        self.message = message;
        Ok(())
    }

    fn view_label(&self) -> &'static str {
        if self.format_requested && self.formatted_path.as_ref() == Some(&self.active_path) {
            "formatted"
        } else if self.format_requested {
            "formatting"
        } else {
            "raw"
        }
    }

    fn gutter_width_hint(&self, view_height: usize) -> usize {
        digits(self.line_number + view_height as u64) + 2
    }
}

impl Drop for App {
    fn drop(&mut self) {
        if let Some(path) = &self.formatted_path {
            let _ = fs::remove_file(path);
        }
        if let Some(handle) = &self.format_handle {
            let _ = fs::remove_file(handle.output_path());
        }
    }
}

fn run_app(terminal: &mut Tui, app: &mut App) -> io::Result<()> {
    loop {
        app.poll_background()?;

        let (term_width, term_height) = crossterm::terminal::size()?;
        let view_height = usize::from(term_height.saturating_sub(3)).max(1);
        let gutter_width = app.gutter_width_hint(view_height);
        let view_width = usize::from(term_width)
            .saturating_sub(2 + gutter_width)
            .max(1);
        let window = app.reader.read_window_from_line(
            app.offset,
            view_width,
            view_height,
            app.line_number,
        )?;

        terminal.draw(|frame| render(frame, app, &window))?;

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        if handle_key(app, &window, key, view_width, view_height)? {
            break;
        }
    }

    Ok(())
}

fn handle_key(
    app: &mut App,
    window: &Window,
    key: KeyEvent,
    width: usize,
    height: usize,
) -> io::Result<bool> {
    if app.mode == InputMode::Search {
        return handle_search_key(app, key);
    }
    if app.mode == InputMode::Command {
        return handle_command_key(app, key);
    }

    if let KeyCode::Char(ch) = key.code
        && ch.is_ascii_digit()
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        app.pending_count.push(ch);
        app.message.clone_from(&app.pending_count);
        return Ok(false);
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
        KeyCode::Char('j') | KeyCode::Down => app.move_down(window),
        KeyCode::Char('k') | KeyCode::Up => app.move_up(width)?,
        KeyCode::PageDown | KeyCode::Char(' ') => app.page_down(window),
        KeyCode::PageUp => app.page_up(width, height)?,
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.half_down(window)
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.half_up(width, height)?;
        }
        KeyCode::Char('g') => app.go_top(),
        KeyCode::Char('G') => {
            if app.pending_count.is_empty() {
                app.go_end(width, height)?;
            } else if let Ok(line_number) = app.pending_count.parse::<u64>() {
                app.jump_to_line(line_number)?;
            }
        }
        KeyCode::Char('/') => app.enter_search(),
        KeyCode::Char(':') => app.enter_command(),
        KeyCode::Char('n') => app.next_match(),
        KeyCode::Char('N') => app.previous_match(),
        KeyCode::Char('f') => app.toggle_format()?,
        _ => {}
    }

    app.pending_count.clear();

    Ok(false)
}

fn handle_search_key(app: &mut App, key: KeyEvent) -> io::Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.mode = InputMode::Normal;
            app.message = "search cancelled".to_owned();
        }
        KeyCode::Enter => app.begin_search()?,
        KeyCode::Backspace => {
            app.search_input.pop();
        }
        KeyCode::Char(ch)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            app.search_input.push(ch);
        }
        _ => {}
    }

    Ok(false)
}

fn handle_command_key(app: &mut App, key: KeyEvent) -> io::Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.mode = InputMode::Normal;
            app.message = "command cancelled".to_owned();
        }
        KeyCode::Enter => return app.finish_command(),
        KeyCode::Backspace => {
            app.command_input.pop();
        }
        KeyCode::Char(ch)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            app.command_input.push(ch);
        }
        _ => {}
    }

    Ok(false)
}

fn render(frame: &mut Frame<'_>, app: &App, window: &Window) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());

    let percent = if window.file_len == 0 {
        100.0
    } else {
        (app.offset as f64 / window.file_len as f64) * 100.0
    };
    let title = format!(
        " {} [{}]  line {} byte {} / {} bytes ({percent:.1}%) ",
        app.active_path.display(),
        app.view_label(),
        app.line_number,
        app.offset,
        window.file_len
    );

    let gutter_width = gutter_width(window);
    let body = app.syntax_highlighter.highlight_window(
        &app.active_path,
        window,
        &app.search_query,
        gutter_width,
    );
    let block = Block::default().borders(Borders::ALL).title(title);
    frame.render_widget(Paragraph::new(body).block(block), chunks[0]);

    frame.render_widget(Paragraph::new(status_line(app)), chunks[1]);
}

fn status_line(app: &App) -> Line<'static> {
    if app.mode == InputMode::Search {
        return Line::from(vec![
            Span::styled(" / ", key_style()),
            Span::raw(app.search_input.clone()),
            Span::raw("  Enter search  Esc cancel"),
        ]);
    }
    if app.mode == InputMode::Command {
        return Line::from(vec![
            Span::styled(" : ", key_style()),
            Span::raw(app.command_input.clone()),
            Span::raw("  Enter run  Esc cancel"),
        ]);
    }

    Line::from(vec![
        Span::raw(app.message.clone()),
        Span::raw("  "),
        Span::styled(" q ", key_style()),
        Span::raw("quit "),
        Span::styled(" / ", key_style()),
        Span::raw("search "),
        Span::styled(" n/N ", key_style()),
        Span::raw("next/prev "),
        Span::styled(" f ", key_style()),
        Span::raw("format "),
        Span::styled(" :line countG ", key_style()),
        Span::raw("jump "),
        Span::styled(" j/k PgUp/PgDn g/G ", key_style()),
        Span::raw("move"),
    ])
}

struct SyntaxHighlighter {
    syntax_set: SyntaxSet,
    theme: Theme,
}

impl SyntaxHighlighter {
    fn new() -> Self {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let themes = ThemeSet::load_defaults();
        let theme = themes
            .themes
            .get("base16-ocean.dark")
            .or_else(|| themes.themes.values().next())
            .cloned()
            .expect("syntect ships default themes");

        Self { syntax_set, theme }
    }

    fn highlight_window(
        &self,
        path: &PathBuf,
        window: &Window,
        query: &str,
        gutter_width: usize,
    ) -> Vec<Line<'static>> {
        let syntax = syntax_for_path(&self.syntax_set, path);
        let mut highlighter = syntax.map(|syntax| HighlightLines::new(syntax, &self.theme));

        window
            .lines
            .iter()
            .map(|line| {
                let spans = if let Some(highlighter) = highlighter.as_mut() {
                    match highlighter.highlight_line(&line.text, &self.syntax_set) {
                        Ok(ranges) => ranges
                            .into_iter()
                            .flat_map(|(style, text)| {
                                split_search_spans(text.to_owned(), syntect_style(style), query)
                            })
                            .collect::<Vec<_>>(),
                        Err(_) => fallback_spans(&line.text, query),
                    }
                } else {
                    fallback_spans(&line.text, query)
                };
                numbered_line(line, gutter_width, spans)
            })
            .collect()
    }
}

fn numbered_line(
    line: &VisualLine,
    gutter_width: usize,
    mut spans: Vec<Span<'static>>,
) -> Line<'static> {
    let mut prefixed = Vec::with_capacity(spans.len() + 1);
    prefixed.push(Span::styled(
        format!("{:>width$} ", line.line_number, width = gutter_width - 1),
        Style::default().fg(Color::DarkGray),
    ));
    prefixed.append(&mut spans);
    Line::from(prefixed)
}

fn fallback_spans(text: &str, query: &str) -> Vec<Span<'static>> {
    highlight_json_line(text)
        .into_iter()
        .flat_map(|token| split_search_spans(token.text, style_for(token.kind), query))
        .collect()
}

fn gutter_width(window: &Window) -> usize {
    let max_line = window
        .lines
        .last()
        .map_or(1, |line| line.line_number.max(line.next_line_number));
    digits(max_line) + 2
}

fn digits(mut value: u64) -> usize {
    let mut digits = 1;
    while value >= 10 {
        digits += 1;
        value /= 10;
    }
    digits
}

fn syntect_style(style: SyntectStyle) -> Style {
    let mut output = Style::default().fg(Color::Rgb(
        style.foreground.r,
        style.foreground.g,
        style.foreground.b,
    ));

    if style.font_style.contains(FontStyle::BOLD) {
        output = output.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        output = output.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        output = output.add_modifier(Modifier::UNDERLINED);
    }

    output
}

fn split_search_spans(text: String, style: Style, query: &str) -> Vec<Span<'static>> {
    if query.is_empty() {
        return vec![Span::styled(text, style)];
    }

    let mut spans = Vec::new();
    let mut start = 0;
    while let Some(relative) = text[start..].find(query) {
        let index = start + relative;
        if index > start {
            spans.push(Span::styled(text[start..index].to_owned(), style));
        }
        let end = index + query.len();
        spans.push(Span::styled(text[index..end].to_owned(), search_style()));
        start = end;
    }

    if start < text.len() {
        spans.push(Span::styled(text[start..].to_owned(), style));
    }

    spans
}

fn key_style() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

fn search_style() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

fn style_for(kind: TokenKind) -> Style {
    match kind {
        TokenKind::String => Style::default().fg(Color::Green),
        TokenKind::Number => Style::default().fg(Color::Cyan),
        TokenKind::Boolean | TokenKind::Null => Style::default().fg(Color::Magenta),
        TokenKind::Bracket | TokenKind::Colon | TokenKind::Comma => {
            Style::default().fg(Color::DarkGray)
        }
        TokenKind::Other => Style::default().fg(Color::Red),
        TokenKind::Whitespace => Style::default(),
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
