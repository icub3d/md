use std::io::{self, Write};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyModifiers},
    execute, queue,
    style::{self, Color, Stylize},
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use crate::renderer::ImageProtocol;

/// Run the interactive terminal pager on the rendered markdown content.
pub fn run_pager(content: &str, filename: &str, image_protocol: ImageProtocol) -> io::Result<()> {
    let mut stdout = io::stdout();

    // Enable raw mode and enter alternate screen
    terminal::enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen, cursor::Hide)?;

    let res = pager_loop(&mut stdout, content, filename, image_protocol);

    // Clean up any remaining Kitty images before leaving
    if image_protocol == ImageProtocol::Kitty {
        let _ = execute!(stdout, style::Print("\x1b_Ga=d\x1b\\"));
    }

    // Restore terminal state
    let _ = execute!(stdout, cursor::Show, LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();

    res
}

struct SearchState {
    query: String,
    matches: Vec<usize>,
    current_match_idx: usize,
}

fn strip_ansi_and_images(s: &str) -> String {
    let mut result = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if let Some(&next) = chars.peek() {
                if next == '[' {
                    // ANSI escape code: ESC [ ... <alphabetic>
                    chars.next();
                    while let Some(&nc) = chars.peek() {
                        chars.next();
                        if nc.is_ascii_alphabetic() {
                            break;
                        }
                    }
                } else if next == '_' {
                    // Kitty escape code: ESC _ G ... ESC \
                    chars.next();
                    if let Some(&nc) = chars.peek() {
                        if nc == 'G' {
                            chars.next();
                            while let Some(cc) = chars.next() {
                                if cc == '\x1b' {
                                    if let Some(&nc2) = chars.peek() {
                                        if nc2 == '\\' {
                                            chars.next();
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                } else if next == ']' {
                    // OSC/iTerm2 escape code: ESC ] 1337 ; ... BEL
                    chars.next();
                    while let Some(cc) = chars.next() {
                        if cc == '\x07' {
                            break;
                        }
                    }
                }
            }
        } else {
            result.push(c);
        }
    }
    result
}

fn pager_loop(stdout: &mut io::Stdout, content: &str, filename: &str, image_protocol: ImageProtocol) -> io::Result<()> {
    let raw_lines: Vec<&str> = content.split('\n').collect();
    // Pre-calculate clean lines for searching
    let clean_lines: Vec<String> = raw_lines
        .iter()
        .map(|&line| strip_ansi_and_images(line).to_lowercase())
        .collect();

    let mut scroll_row = 0;
    let mut search: Option<SearchState> = None;
    let mut input_buffer: Option<String> = None;

    loop {
        let (width_val, height_val) = terminal::size()?;
        let width = width_val as usize;
        let height = height_val as usize;

        // Leave 1 line for top title bar and 1 line for bottom status bar
        let viewport_height = height.saturating_sub(2);

        // Clear the entire screen once at the beginning of the draw loop to
        // prevent stale/stuck images and text overlapping when redrawing or scrolling.
        queue!(stdout, terminal::Clear(terminal::ClearType::All))?;

        // Draw the top header bar
        let header_left = format!(" md ── {} ", filename);
        let header_right = " ['q': quit | '/': search | 'j'/'k': scroll] ";
        let header_len = header_left.chars().count() + header_right.chars().count();
        let header_padding = width.saturating_sub(header_len);
        let header_text = format!("{}{}{}", header_left, " ".repeat(header_padding), header_right);

        queue!(
            stdout,
            cursor::MoveTo(0, 0),
            style::PrintStyledContent(
                header_text
                    .bold()
                    .with(Color::Black)
                    .on(Color::Cyan)
            )
        )?;

        // Draw the viewport lines
        if image_protocol == ImageProtocol::Kitty {
            // Delete all visible Kitty graphics protocol images to prevent duplicating and getting stuck
            queue!(stdout, style::Print("\x1b_Ga=d\x1b\\"))?;
        }

        for i in 0..viewport_height {
            let file_row = scroll_row + i;
            queue!(
                stdout,
                cursor::MoveTo(0, (i + 1) as u16)
            )?;

            if file_row < raw_lines.len() {
                let line_content = raw_lines[file_row];
                // Highlight the line if it is the current search match
                let is_current_match = search
                    .as_ref()
                    .map(|s| {
                        !s.matches.is_empty() && s.matches[s.current_match_idx] == file_row
                    })
                    .unwrap_or(false);

                if is_current_match {
                    // Highlight match line with a yellow indicator or background style
                    queue!(
                        stdout,
                        style::PrintStyledContent("➔ ".yellow().bold()),
                        style::Print(line_content)
                    )?;
                } else {
                    queue!(stdout, style::Print(line_content))?;
                }
            }
        }

        // Draw the bottom status/input bar
        queue!(
            stdout,
            cursor::MoveTo(0, (height - 1) as u16),
            terminal::Clear(terminal::ClearType::CurrentLine)
        )?;

        if let Some(ref current_input) = input_buffer {
            // We are currently typing a search query
            let search_prompt = format!(" Search: /{}", current_input);
            let search_padding = width.saturating_sub(search_prompt.chars().count());
            let search_text = format!("{}{}", search_prompt, " ".repeat(search_padding));
            queue!(
                stdout,
                style::PrintStyledContent(
                    search_text
                        .bold()
                        .with(Color::Black)
                        .on(Color::Yellow)
                )
            )?;
        } else {
            // Standard status line
            let progress = if raw_lines.is_empty() {
                100
            } else {
                let pos = scroll_row + viewport_height;
                (pos * 100 / raw_lines.len()).min(100)
            };

            let left_text = if let Some(ref s) = search {
                if s.matches.is_empty() {
                    format!(" No matches found for \"{}\" ", s.query)
                } else {
                    format!(
                        " Match {} of {} for \"{}\" (press 'n'/'N' for next/prev) ",
                        s.current_match_idx + 1,
                        s.matches.len(),
                        s.query
                    )
                }
            } else {
                " md viewer ".to_string()
            };

            let right_text = format!(
                " Line {}/{} ({}%) ",
                scroll_row + 1,
                raw_lines.len(),
                progress
            );

            let status_len = left_text.chars().count() + right_text.chars().count();
            let status_padding = width.saturating_sub(status_len);
            let status_text = format!("{}{}{}", left_text, " ".repeat(status_padding), right_text);

            let status_color = if search.is_some() {
                Color::Yellow
            } else {
                Color::Green
            };

            queue!(
                stdout,
                style::PrintStyledContent(
                    status_text
                        .bold()
                        .with(Color::Black)
                        .on(status_color)
                )
            )?;
        }

        stdout.flush()?;

        // Wait for user input
        if let Event::Key(key) = event::read()? {
            // If typing search query
            if let Some(mut current_input) = input_buffer.take() {
                match key.code {
                    KeyCode::Enter => {
                        let query = current_input.trim().to_lowercase();
                        if !query.is_empty() {
                            let mut matches = Vec::new();
                            for (idx, line) in clean_lines.iter().enumerate() {
                                if line.contains(&query) {
                                    matches.push(idx);
                                }
                            }

                            let search_state = SearchState {
                                query: current_input,
                                matches,
                                current_match_idx: 0,
                            };

                            if !search_state.matches.is_empty() {
                                // Jump to the first match
                                scroll_row = search_state.matches[0];
                            }
                            search = Some(search_state);
                        } else {
                            search = None;
                        }
                        input_buffer = None;
                    }
                    KeyCode::Esc => {
                        // Cancel searching
                        input_buffer = None;
                    }
                    KeyCode::Backspace => {
                        current_input.pop();
                        input_buffer = Some(current_input);
                    }
                    KeyCode::Char(c) => {
                        current_input.push(c);
                        input_buffer = Some(current_input);
                    }
                    _ => {
                        input_buffer = Some(current_input);
                    }
                }
                continue;
            }

            // Normal pager controls
            let is_ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    break;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if scroll_row > 0 {
                        scroll_row -= 1;
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if scroll_row + viewport_height < raw_lines.len() {
                        scroll_row += 1;
                    }
                }
                KeyCode::PageUp => {
                    scroll_row = scroll_row.saturating_sub(viewport_height);
                }
                KeyCode::PageDown | KeyCode::Char(' ') => {
                    scroll_row = (scroll_row + viewport_height)
                        .min(raw_lines.len().saturating_sub(viewport_height));
                }
                KeyCode::Char('b') => {
                    scroll_row = scroll_row.saturating_sub(viewport_height);
                }
                KeyCode::Char('f') => {
                    scroll_row = (scroll_row + viewport_height)
                        .min(raw_lines.len().saturating_sub(viewport_height));
                }
                KeyCode::Char('u') => {
                    let amount = if is_ctrl {
                        (viewport_height / 2).max(1)
                    } else {
                        viewport_height
                    };
                    scroll_row = scroll_row.saturating_sub(amount);
                }
                KeyCode::Char('d') => {
                    let amount = if is_ctrl {
                        (viewport_height / 2).max(1)
                    } else {
                        viewport_height
                    };
                    scroll_row = (scroll_row + amount)
                        .min(raw_lines.len().saturating_sub(viewport_height));
                }
                KeyCode::Home | KeyCode::Char('g') => {
                    scroll_row = 0;
                }
                KeyCode::End | KeyCode::Char('G') => {
                    scroll_row = raw_lines.len().saturating_sub(viewport_height);
                }
                KeyCode::Char('/') => {
                    input_buffer = Some(String::new());
                }
                KeyCode::Char('n') => {
                    if let Some(ref mut s) = search {
                        if !s.matches.is_empty() {
                            s.current_match_idx = (s.current_match_idx + 1) % s.matches.len();
                            scroll_row = s.matches[s.current_match_idx];
                        }
                    }
                }
                KeyCode::Char('N') => {
                    if let Some(ref mut s) = search {
                        if !s.matches.is_empty() {
                            if s.current_match_idx == 0 {
                                s.current_match_idx = s.matches.len() - 1;
                            } else {
                                s.current_match_idx -= 1;
                            }
                            scroll_row = s.matches[s.current_match_idx];
                        }
                    }
                }
                _ => {}
            }
        }
    }

    Ok(())
}



