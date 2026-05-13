use std::{
    env,
    error::Error,
    io::{self, Stdout},
    path::PathBuf,
    time::Duration,
};

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use large_json_reader::{LargeFile, TokenKind, Window, highlight_json_line};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
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

struct App {
    path: PathBuf,
    reader: LargeFile,
    offset: u64,
}

impl App {
    fn new(path: PathBuf) -> io::Result<Self> {
        let reader = LargeFile::open(&path)?;
        Ok(Self {
            path,
            reader,
            offset: 0,
        })
    }

    fn move_down(&mut self, window: &Window) {
        if let Some(line) = window.lines.get(1) {
            self.offset = line.start_offset;
        } else if let Some(line) = window.lines.first() {
            self.offset = line.next_offset.min(window.file_len);
        }
    }

    fn move_up(&mut self, width: usize) -> io::Result<()> {
        self.offset = self.reader.previous_visual_offset(self.offset, width)?;
        Ok(())
    }

    fn page_down(&mut self, window: &Window) {
        if let Some(line) = window.lines.last() {
            self.offset = line.next_offset.min(window.file_len);
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
        Ok(())
    }

    fn half_down(&mut self, window: &Window) {
        let target = window.lines.len().saturating_div(2).max(1);
        if let Some(line) = window.lines.get(target) {
            self.offset = line.start_offset;
        } else {
            self.page_down(window);
        }
    }

    fn half_up(&mut self, width: usize, height: usize) -> io::Result<()> {
        self.page_up(width, height.saturating_div(2).max(1))
    }

    fn go_top(&mut self) {
        self.offset = 0;
    }

    fn go_end(&mut self, width: usize, height: usize) -> io::Result<()> {
        self.offset = self.reader.near_end_offset(width, height)?;
        Ok(())
    }
}

fn run_app(terminal: &mut Tui, app: &mut App) -> io::Result<()> {
    loop {
        let (term_width, term_height) = crossterm::terminal::size()?;
        let view_width = usize::from(term_width.saturating_sub(2)).max(1);
        let view_height = usize::from(term_height.saturating_sub(3)).max(1);
        let window = app
            .reader
            .read_window(app.offset, view_width, view_height)?;

        terminal.draw(|frame| render(frame, app, &window))?;

        if !event::poll(Duration::from_millis(250))? {
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
        KeyCode::Char('G') => app.go_end(width, height)?,
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
        " {}  {} / {} bytes ({percent:.1}%) ",
        app.path.display(),
        app.offset,
        window.file_len
    );

    let body = window
        .lines
        .iter()
        .map(|line| highlighted_line(&line.text))
        .collect::<Vec<_>>();
    let block = Block::default().borders(Borders::ALL).title(title);
    frame.render_widget(Paragraph::new(body).block(block), chunks[0]);

    let status = Line::from(vec![
        Span::styled(" q ", key_style()),
        Span::raw("quit  "),
        Span::styled(" j/k ", key_style()),
        Span::raw("move  "),
        Span::styled(" Ctrl-D/U ", key_style()),
        Span::raw("half-page  "),
        Span::styled(" PgUp/PgDn ", key_style()),
        Span::raw("page  "),
        Span::styled(" g/G ", key_style()),
        Span::raw("top/end"),
    ]);
    frame.render_widget(Paragraph::new(status), chunks[1]);
}

fn highlighted_line(text: &str) -> Line<'static> {
    Line::from(
        highlight_json_line(text)
            .into_iter()
            .map(|token| Span::styled(token.text, style_for(token.kind)))
            .collect::<Vec<_>>(),
    )
}

fn key_style() -> Style {
    Style::default()
        .fg(Color::Yellow)
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
