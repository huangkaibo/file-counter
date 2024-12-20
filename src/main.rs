use crossterm::{
    event::{
        self,
        DisableMouseCapture,
        EnableMouseCapture,
        Event,
        KeyCode,
        MouseButton,
        MouseEventKind,
    },
    execute,
    terminal::{ disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen },
};
use dashmap::DashMap;
use num_cpus;
use ratatui::{
    backend::CrosstermBackend,
    layout::{ Constraint, Direction, Layout, Rect },
    style::{ Modifier, Style, Color },
    text::{ Span, Spans },
    widgets::{ Block, Borders, Table, Cell, Row, TableState, Paragraph, Wrap },
    Terminal,
};
use std::{
    collections::HashSet,
    fs,
    io,
    path::{ Path, PathBuf },
    sync::{ mpsc::{ channel, Receiver, Sender }, Arc },
};
use threadpool::ThreadPool;
use unicode_width::UnicodeWidthStr;

struct App {
    current_dir: PathBuf,
    home_dir: PathBuf,
    current_dir_count: Option<usize>, // Store the file count of the current directory
    items: Vec<DirEntry>,
    table_state: TableState,
    action_pending: Option<Action>,
    file_count_tx: Sender<(PathBuf, usize)>,
    file_count_rx: Receiver<(PathBuf, usize)>,
    thread_pool: ThreadPool,
    spinner_index: usize,
    spinner_frames: Vec<&'static str>,
    file_count_cache: Arc<DashMap<PathBuf, usize>>, // Cache using DashMap
}

enum Action {
    EnterDirectory(usize),
}

struct DirEntry {
    name: String,
    path: PathBuf,
    is_dir: bool,
    file_count: Option<usize>,
}

impl App {
    fn new(start_dir: PathBuf) -> io::Result<Self> {
        let (file_count_tx, file_count_rx) = channel();
        let thread_pool = ThreadPool::new(num_cpus::get());

        // Define spinner frames
        let spinner_frames = vec!["   ", ".  ", ".. ", "..."];

        // Initialize cache
        let file_count_cache = Arc::new(DashMap::new());

        let mut app = App {
            current_dir: start_dir.clone(),
            home_dir: start_dir,
            current_dir_count: None, // Initialize as None
            items: Vec::new(),
            table_state: TableState::default(),
            action_pending: None,
            file_count_tx,
            file_count_rx,
            thread_pool,
            spinner_index: 0,
            spinner_frames,
            file_count_cache,
        };
        app.refresh_items()?;
        Ok(app)
    }

    /// Refresh the item list in the current directory
    fn refresh_items(&mut self) -> io::Result<()> {
        self.items.clear();

        let previous_selection = self.table_state.selected().unwrap_or(0);

        let include_back = self.current_dir != self.home_dir;

        self.table_state.select(Some(previous_selection));

        // Check if the file count of the current directory is in the cache
        self.current_dir_count = self.file_count_cache.get(&self.current_dir).map(|v| *v);

        // If not cached, start a thread to compute the file count
        if self.current_dir_count.is_none() {
            let path = self.current_dir.clone();
            let sender = self.file_count_tx.clone();
            let cache: Arc<DashMap<PathBuf, usize>> = Arc::clone(&self.file_count_cache);

            self.thread_pool.execute(move || {
                let count = count_files(&path).unwrap_or(0);

                // Update cache
                cache.insert(path.clone(), count);

                // Send result
                sender.send((path, count)).unwrap_or(());
            });
        }

        // Add option to go back to parent directory (if not at home_dir)
        if include_back {
            if let Some(parent) = self.current_dir.parent() {
                // Check if the file count of the parent directory is in the cache
                let parent_count = self.file_count_cache.get(&parent.to_path_buf()).map(|v| *v);

                // If not cached, start a thread to compute the file count
                if parent_count.is_none() {
                    let path = parent.to_path_buf();
                    let sender = self.file_count_tx.clone();
                    let cache: Arc<DashMap<PathBuf, usize>> = Arc::clone(&self.file_count_cache);

                    self.thread_pool.execute(move || {
                        let count = count_files(&path).unwrap_or(0);

                        // Update cache
                        cache.insert(path.clone(), count);

                        // Send result
                        sender.send((path, count)).unwrap_or(());
                    });
                }

                self.items.push(DirEntry {
                    name: String::from(".. (Back to parent directory)"),
                    path: parent.to_path_buf(),
                    is_dir: true,
                    file_count: parent_count, // Use cached file count
                });
            }
        }

        let entries: Vec<_> = match fs::read_dir(&self.current_dir) {
            Ok(entries) => entries.collect::<Result<Vec<_>, _>>()?,
            Err(_) => Vec::new(), // Unable to read directory, use empty list
        };

        for entry in entries {
            let path = entry.path();
            let is_dir = path.is_dir();
            let name = entry
                .file_name()
                .into_string()
                .unwrap_or_else(|_| String::from("Unknown"));

            // Check cache
            let cached_count = if is_dir {
                self.file_count_cache.get(&path).map(|v| *v)
            } else {
                None
            };

            self.items.push(DirEntry {
                name,
                path,
                is_dir,
                file_count: cached_count, // Use cached file count if available
            });
        }

        // Submit tasks to compute file counts for each directory (if not cached)
        for item in self.items.iter() {
            if item.is_dir && item.file_count.is_none() {
                // Clone necessary data
                let path = item.path.clone();
                let sender = self.file_count_tx.clone();
                let cache: Arc<DashMap<PathBuf, usize>> = Arc::clone(&self.file_count_cache);

                self.thread_pool.execute(move || {
                    let count = count_files(&path).unwrap_or(0);

                    // Update cache
                    cache.insert(path.clone(), count);

                    // Send result
                    sender.send((path, count)).unwrap_or(());
                });
            }
        }

        // Sort items based on file count
        if include_back && self.items.len() > 1 {
            let (_first, rest) = self.items.split_at_mut(1);
            rest.sort_by(|a, b| {
                match (a.is_dir, b.is_dir) {
                    (true, true) =>
                        match (a.file_count, b.file_count) {
                            (Some(a_count), Some(b_count)) =>
                                b_count
                                    .cmp(&a_count)
                                    .then(a.name.to_lowercase().cmp(&b.name.to_lowercase())),
                            (Some(_), None) => std::cmp::Ordering::Less,
                            (None, Some(_)) => std::cmp::Ordering::Greater,
                            (None, None) => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                        }
                    (false, false) => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                    (true, false) => std::cmp::Ordering::Less,
                    (false, true) => std::cmp::Ordering::Greater,
                }
            });
        } else {
            self.items.sort_by(|a, b| {
                match (a.is_dir, b.is_dir) {
                    (true, true) =>
                        match (a.file_count, b.file_count) {
                            (Some(a_count), Some(b_count)) =>
                                b_count
                                    .cmp(&a_count)
                                    .then(a.name.to_lowercase().cmp(&b.name.to_lowercase())),
                            (Some(_), None) => std::cmp::Ordering::Less,
                            (None, Some(_)) => std::cmp::Ordering::Greater,
                            (None, None) => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                        }
                    (false, false) => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                    (true, false) => std::cmp::Ordering::Less,
                    (false, true) => std::cmp::Ordering::Greater,
                }
            });
        }

        Ok(())
    }

    /// Move selection to the next item
    fn next(&mut self) {
        let i = match self.table_state.selected() {
            Some(i) => {
                if i >= self.items.len() - 1 { 0 } else { i + 1 }
            }
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    /// Move selection to the previous item
    fn previous(&mut self) {
        let i = match self.table_state.selected() {
            Some(i) => {
                if i == 0 { self.items.len() - 1 } else { i - 1 }
            }
            None => self.items.len() - 1,
        };
        self.table_state.select(Some(i));
    }
}

/// Count the number of files in a directory using an iterative approach to avoid stack overflow
fn count_files(dir: &Path) -> io::Result<usize> {
    let mut count = 0usize;
    let mut dirs_to_visit = Vec::new();
    let mut visited = HashSet::new();

    dirs_to_visit.push(dir.to_path_buf());

    while let Some(current_dir) = dirs_to_visit.pop() {
        let real_dir = match current_dir.canonicalize() {
            Ok(path) => path,
            Err(_) => {
                continue;
            } // Unable to get real path, skip
        };

        if !visited.insert(real_dir.clone()) {
            continue; // Already visited, skip
        }

        let entries = match fs::read_dir(&real_dir) {
            Ok(entries) => entries,
            Err(_) => {
                continue;
            } // Unable to read directory, skip
        };

        for entry_result in entries {
            match entry_result {
                Ok(entry) => {
                    let path = entry.path();
                    if path.is_file() {
                        count += 1;
                    } else if path.is_dir() {
                        dirs_to_visit.push(path);
                    }
                }
                Err(_) => {
                    continue;
                } // Unable to read entry, skip
            }
        }
    }

    Ok(count)
}

/// Calculate the wrapped height of text given a maximum width
fn calculate_wrapped_height(text: &str, max_width: u16) -> u16 {
    let mut height = 0u16;
    for line in text.lines() {
        let line_width = UnicodeWidthStr::width(line) as u16;
        let line_height = if line_width == 0 { 1 } else { (line_width - 1) / max_width + 1 };
        height += line_height;
    }
    height
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Get the starting directory
    let args: Vec<String> = std::env::args().collect();
    let start_dir = if args.len() > 1 { PathBuf::from(&args[1]) } else { std::env::current_dir()? };

    // Initialize the App
    let mut app = App::new(start_dir)?;

    // Set up the terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Initialize table_area
    let mut table_area = Rect::default();

    // Main loop
    let mut redraw_ui = true;
    loop {
        // Update spinner frame index
        app.spinner_index = (app.spinner_index + 1) % app.spinner_frames.len();

        // Handle messages from file_count_rx
        let mut counts_updated = false;
        while let Ok((path, count)) = app.file_count_rx.try_recv() {
            if path == app.current_dir {
                app.current_dir_count = Some(count);
                counts_updated = true;
            }

            // Update file count for "back to parent directory"
            if let Some(item) = app.items.iter_mut().find(|i| i.path == path) {
                item.file_count = Some(count);
                counts_updated = true;
            }
        }

        if counts_updated {
            // Re-sort items
            let include_back = app.current_dir != app.home_dir;
            if include_back && app.items.len() > 1 {
                let (_first, rest) = app.items.split_at_mut(1);
                rest.sort_by(|a, b| {
                    match (a.is_dir, b.is_dir) {
                        (true, true) =>
                            match (a.file_count, b.file_count) {
                                (Some(a_count), Some(b_count)) =>
                                    b_count
                                        .cmp(&a_count)
                                        .then(a.name.to_lowercase().cmp(&b.name.to_lowercase())),
                                (Some(_), None) => std::cmp::Ordering::Less,
                                (None, Some(_)) => std::cmp::Ordering::Greater,
                                (None, None) => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                            }
                        (false, false) => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                        (true, false) => std::cmp::Ordering::Less,
                        (false, true) => std::cmp::Ordering::Greater,
                    }
                });
            } else {
                app.items.sort_by(|a, b| {
                    match (a.is_dir, b.is_dir) {
                        (true, true) =>
                            match (a.file_count, b.file_count) {
                                (Some(a_count), Some(b_count)) =>
                                    b_count
                                        .cmp(&a_count)
                                        .then(a.name.to_lowercase().cmp(&b.name.to_lowercase())),
                                (Some(_), None) => std::cmp::Ordering::Less,
                                (None, Some(_)) => std::cmp::Ordering::Greater,
                                (None, None) => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                            }
                        (false, false) => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                        (true, false) => std::cmp::Ordering::Less,
                        (false, true) => std::cmp::Ordering::Greater,
                    }
                });
            }

            redraw_ui = true;
        }

        if redraw_ui {
            // Draw the UI
            terminal.draw(|f| {
                let size = f.size();

                // Calculate block width (subtract borders)
                let block_width = size.width - 2;

                // Get current directory path string
                let current_dir_text = if let Some(count) = app.current_dir_count {
                    format!("{} (Total files: {})", app.current_dir.display(), count)
                } else {
                    let spinner_frame = app.spinner_frames[app.spinner_index];
                    format!("{} (Counting files{})", app.current_dir.display(), spinner_frame)
                };

                // Calculate the height after wrapping
                let num_lines = calculate_wrapped_height(&current_dir_text, block_width);

                // Set block height including borders
                let current_dir_height = num_lines + 2; // +2 for borders

                // Set up the layout
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints(
                        [
                            Constraint::Length(current_dir_height), // Current directory
                            Constraint::Min(1), // File list
                            Constraint::Length(3), // Footer
                        ].as_ref()
                    )
                    .split(size);

                // Display the "Current Directory" block
                let title_block = Block::default()
                    .borders(Borders::ALL)
                    .title(
                        Span::styled(
                            "Current Directory",
                            Style::default().add_modifier(Modifier::BOLD)
                        )
                    );

                // Paragraph containing the current directory, with wrapping enabled
                let current_dir_paragraph = Paragraph::new(current_dir_text)
                    .block(title_block)
                    .wrap(Wrap { trim: false });

                f.render_widget(current_dir_paragraph, chunks[0]);

                // Prepare table data
                let header_cells = ["Type", "Name", "Count"]
                    .iter()
                    .map(|h| Cell::from(*h).style(Style::default().fg(Color::Yellow)));
                let header = Row::new(header_cells)
                    .style(Style::default().bg(Color::DarkGray))
                    .height(1);

                let spinner_frame = app.spinner_frames[app.spinner_index];

                let rows = app.items.iter().map(|entry| {
                    let type_cell = if entry.is_dir {
                        Cell::from("Dir").style(Style::default().fg(Color::Blue))
                    } else {
                        Cell::from("File").style(Style::default().fg(Color::Gray))
                    };
                    let name_cell = if
                        entry.is_dir &&
                        entry.name == ".. (Back to parent directory)"
                    {
                        Cell::from(entry.name.clone()).style(Style::default().fg(Color::Green))
                    } else {
                        Cell::from(entry.name.clone())
                    };
                    let file_count_cell = if entry.is_dir {
                        match entry.file_count {
                            Some(count) => Cell::from(count.to_string()),
                            None => Cell::from(spinner_frame),
                        }
                    } else {
                        Cell::from("-")
                    };
                    Row::new(vec![type_cell, name_cell, file_count_cell]).height(1)
                });

                let t = Table::new(rows)
                    .header(header)
                    .block(Block::default().borders(Borders::ALL).title("File Counter"))
                    .highlight_style(
                        Style::default()
                            .bg(Color::LightGreen)
                            .fg(Color::Black)
                            .add_modifier(Modifier::BOLD)
                    )
                    .highlight_symbol(">> ")
                    .widths(
                        &[Constraint::Length(6), Constraint::Percentage(70), Constraint::Length(6)]
                    );

                let mut state = app.table_state.clone();

                f.render_stateful_widget(t, chunks[1], &mut state);

                // Save the table area for mouse event handling
                table_area = chunks[1];

                // Footer: display key bindings
                let footer_text = vec![
                    Spans::from(
                        vec![
                            Span::styled(
                                "q - Quit",
                                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                            ),
                            Span::raw(" | "),
                            Span::styled(
                                "↑/↓/k/j - Move",
                                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                            ),
                            Span::raw(" | "),
                            Span::styled(
                                "Enter - Open",
                                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                            ),
                            Span::raw(" | "),
                            Span::styled(
                                "h - Home",
                                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                            )
                        ]
                    )
                ];
                let footer_paragraph = Paragraph::new(footer_text)
                    .block(Block::default().borders(Borders::ALL))
                    .wrap(Wrap { trim: true });

                f.render_widget(footer_paragraph, chunks[2]);
            })?;
            redraw_ui = false;
        }

        // After drawing, handle any pending actions
        if let Some(action) = app.action_pending.take() {
            match action {
                Action::EnterDirectory(index) => {
                    if index < app.items.len() {
                        let selected_entry = &app.items[index];
                        if selected_entry.is_dir {
                            app.current_dir = selected_entry.path.clone();
                            app.refresh_items()?;
                            redraw_ui = true;
                        }
                    }
                }
            }
        }

        // Handle input events
        if event::poll(std::time::Duration::from_millis(100))? {
            match event::read() {
                Ok(evt) =>
                    match evt {
                        // Handle keyboard events
                        Event::Key(key) =>
                            match key.code {
                                // Quit the program
                                KeyCode::Char('q') => {
                                    break;
                                }
                                // Move up
                                KeyCode::Up | KeyCode::Char('k') => {
                                    app.previous();
                                    redraw_ui = true;
                                }
                                // Move down
                                KeyCode::Down | KeyCode::Char('j') => {
                                    app.next();
                                    redraw_ui = true;
                                }
                                // Enter directory
                                KeyCode::Enter => {
                                    if let Some(selected) = app.table_state.selected() {
                                        app.action_pending = Some(Action::EnterDirectory(selected));
                                    }
                                }
                                // Go to home directory
                                KeyCode::Char('h') => {
                                    app.current_dir = app.home_dir.clone();
                                    app.refresh_items()?;
                                    redraw_ui = true;
                                }
                                _ => {}
                            }
                        // Handle mouse events
                        Event::Mouse(mouse_event) =>
                            match mouse_event.kind {
                                MouseEventKind::Down(MouseButton::Left) => {
                                    let mouse_row = mouse_event.row;
                                    let mouse_col = mouse_event.column;
                                    // Check if the click is within the table area
                                    if
                                        mouse_row >= table_area.top() + 2 &&
                                        // +1 for top border, +1 for header
                                        mouse_row < table_area.bottom() - 1 &&
                                        // -1 for bottom border
                                        mouse_col >= table_area.left() + 1 &&
                                        // +1 for left border
                                        mouse_col < table_area.right() - 1
                                        // -1 for right border
                                    {
                                        // Calculate the index of the clicked item
                                        let relative_row = mouse_row - table_area.top() - 2;
                                        // -2 for top border and header
                                        if relative_row < (app.items.len() as u16) {
                                            app.table_state.select(Some(relative_row as usize));
                                            // Set pending action
                                            app.action_pending = Some(
                                                Action::EnterDirectory(relative_row as usize)
                                            );
                                            redraw_ui = true;
                                        }
                                    }
                                }
                                _ => {}
                            }
                        _ => {}
                    }
                Err(e) => {
                    // Handle errors, such as logging or displaying error messages
                    eprintln!("Error reading event: {}", e);
                }
            }
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;

    Ok(())
}
