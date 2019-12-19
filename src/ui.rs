use std::io;
use std::io::{stdin, stdout, Write};
use std::process;
use std::process::Stdio;
use termion::color;
use termion::input::TermRead;
use termion::raw::IntoRawMode;

use gopher;
use gopher::io_error;
use gopher::Type;
use menu::MenuView;
use text::TextView;

pub type Key = termion::event::Key;

pub const SCROLL_LINES: usize = 15;
pub const MAX_COLS: usize = 72;

pub struct UI {
    pages: Vec<Box<dyn View>>, // loaded views
    page: usize,               // currently focused view
    dirty: bool,               // redraw?
    running: bool,             // main ui loop running?
    pub size: (usize, usize),  // cols, rows
}

#[derive(Debug)]
pub enum Action {
    None,          // do nothing
    Back,          // back in history
    Forward,       // also history
    Open(String),  // url
    Keypress(Key), // unknown keypress
    Redraw,        // redraw everything
    Quit,          // yup
    Error(String), // error message
}

pub trait View {
    fn process_input(&mut self, c: Key) -> Action;
    fn render(&self) -> String;
    fn url(&self) -> String;
    fn set_size(&mut self, cols: usize, rows: usize);
}

impl UI {
    pub fn new() -> UI {
        let mut size = (0, 0);
        if let Ok((cols, rows)) = termion::terminal_size() {
            size = (cols as usize, rows as usize);
        }
        UI {
            pages: vec![],
            page: 0,
            dirty: true,
            running: true,
            size,
        }
    }

    pub fn run(&mut self) {
        while self.running {
            self.draw();
            self.update();
        }
    }

    pub fn draw(&mut self) {
        if self.dirty {
            print!(
                "{}{}{}{}",
                termion::clear::All,
                termion::cursor::Goto(1, 1),
                termion::cursor::Hide,
                self.render()
            );
            self.dirty = false;
        }
    }

    pub fn update(&mut self) {
        let mut stdout = stdout().into_raw_mode().unwrap();
        stdout.flush().unwrap();

        let action = self.process_page_input();
        self.process_action(action)
            .map_err(|e| self.error(&e.to_string()));
    }

    pub fn open(&mut self, url: &str) -> io::Result<()> {
        // non-gopher URL
        if !url.starts_with("gopher://") {
            return open_external(url);
        }

        // gopher URL
        self.status(&format!(
            "{}Loading...{}",
            color::Fg(color::LightBlack),
            termion::cursor::Show
        ));
        let (typ, host, port, sel) = gopher::parse_url(url);
        gopher::fetch(host, port, sel)
            .and_then(|response| match typ {
                Type::Menu | Type::Search => {
                    Ok(self.add_page(MenuView::from(url.to_string(), response)))
                }
                Type::Text | Type::HTML => {
                    Ok(self.add_page(TextView::from(url.to_string(), response)))
                }
                _ => Err(io_error(format!("Unsupported Gopher Response: {:?}", typ))),
            })
            .map_err(|e| io_error(format!("Error loading {}: {} ({:?})", url, e, e.kind())))
    }

    pub fn render(&mut self) -> String {
        if let Ok((cols, rows)) = termion::terminal_size() {
            self.set_size(cols as usize, rows as usize);
            if !self.pages.is_empty() && self.page < self.pages.len() {
                if let Some(page) = self.pages.get_mut(self.page) {
                    page.set_size(cols as usize, rows as usize);
                    return page.render();
                }
            }
            String::from("No content to display.")
        } else {
            format!(
                "Error getting terminal size. Please file a bug: {}",
                "https://github.com/dvkt/phetch/issues/new"
            )
        }
    }

    fn set_size(&mut self, cols: usize, rows: usize) {
        self.size = (cols, rows);
    }

    // Display a status message to the user.
    fn status(&self, s: &str) {
        print!(
            "{}{}{}{}{}",
            "\x1b[93m",
            termion::cursor::Goto(1, self.size.1 as u16),
            termion::clear::CurrentLine,
            s,
            color::Fg(color::Reset)
        );
        stdout().flush();
    }

    // Display an error message to the user.
    fn error(&self, e: &str) {
        print!(
            "{}{}{}{}{}",
            "\x1b[91m",
            termion::cursor::Goto(1, self.size.1 as u16),
            termion::clear::CurrentLine,
            e,
            color::Fg(color::Reset)
        );
        stdout().flush();
    }

    // Prompt user for input.
    fn prompt(&self, prompt: &str) -> Option<String> {
        print!(
            "{}{}{}{}{}",
            color::Fg(color::Reset),
            termion::cursor::Goto(1, self.size.1 as u16),
            termion::clear::CurrentLine,
            prompt,
            termion::cursor::Show,
        );
        stdout().flush();

        let mut input = String::new();
        for k in stdin().keys() {
            if let Ok(key) = k {
                match key {
                    Key::Char('\n') => {
                        print!("{}{}", termion::clear::CurrentLine, termion::cursor::Hide);
                        stdout().flush();
                        return Some(input);
                    }
                    Key::Char(c) => input.push(c),
                    Key::Esc | Key::Ctrl('c') => {
                        if input.is_empty() {
                            print!("{}{}", termion::clear::CurrentLine, termion::cursor::Hide);
                            stdout().flush();
                            return None;
                        } else {
                            input.clear();
                        }
                    }
                    Key::Backspace | Key::Delete => {
                        input.pop();
                    }
                    _ => {}
                }
            } else {
                break;
            }

            print!(
                "{}{}{}{}",
                termion::cursor::Goto(1, self.size.1 as u16),
                termion::clear::CurrentLine,
                prompt,
                input,
            );
            stdout().flush();
        }

        if !input.is_empty() {
            Some(input)
        } else {
            None
        }
    }

    fn add_page<T: View + 'static>(&mut self, view: T) {
        self.dirty = true;
        if !self.pages.is_empty() && self.page < self.pages.len() - 1 {
            self.pages.truncate(self.page + 1);
        }
        self.pages.push(Box::from(view));
        if self.pages.len() > 1 {
            self.page += 1;
        }
    }

    fn process_page_input(&mut self) -> Action {
        if let Some(page) = self.pages.get_mut(self.page) {
            if let Ok(key) = stdin()
                .keys()
                .nth(0)
                .ok_or(Action::Error("stdin.keys() error".to_string()))
            {
                if let Ok(key) = key {
                    return page.process_input(key);
                }
            }
        }

        Action::None
    }

    fn process_action(&mut self, action: Action) -> io::Result<()> {
        match action {
            Action::Quit | Action::Keypress(Key::Ctrl('q')) | Action::Keypress(Key::Ctrl('c')) => {
                self.running = false
            }
            Action::Error(e) => return Err(io_error(e)),
            Action::Redraw => self.dirty = true,
            Action::Open(url) => self.open(&url)?,
            Action::Back | Action::Keypress(Key::Left) | Action::Keypress(Key::Backspace) => {
                if self.page > 0 {
                    self.dirty = true;
                    self.page -= 1;
                }
            }
            Action::Forward | Action::Keypress(Key::Right) => {
                if self.page < self.pages.len() - 1 {
                    self.dirty = true;
                    self.page += 1;
                }
            }
            Action::Keypress(Key::Ctrl('g')) => {
                if let Some(url) = self.prompt("Go to URL: ") {
                    if !url.contains("://") && !url.starts_with("gopher://") {
                        self.open(&format!("gopher://{}", url))?;
                    } else {
                        self.open(&url)?;
                    }
                }
            }
            Action::Keypress(Key::Ctrl('u')) => {
                if let Some(page) = self.pages.get(self.page) {
                    self.status(&format!("Current URL: {}", page.url()));
                }
            }
            Action::Keypress(Key::Ctrl('y')) => {
                if let Some(page) = self.pages.get(self.page) {
                    copy_to_clipboard(&page.url())?;
                    self.status(&format!("Copied {} to clipboard.", page.url()));
                }
            }
            _ => (),
        }
        Ok(())
    }
}

impl Drop for UI {
    fn drop(&mut self) {
        print!("\x1b[?25h"); // show cursor
    }
}

fn copy_to_clipboard(data: &str) -> io::Result<()> {
    spawn_os_clipboard()
        .and_then(|mut child| {
            let child_stdin = child.stdin.as_mut().unwrap();
            child_stdin.write_all(data.as_bytes())
        })
        .map_err(|e| io_error(format!("Clipboard error: {}", e)))
}

fn spawn_os_clipboard() -> io::Result<process::Child> {
    if cfg!(target_os = "macos") {
        process::Command::new("pbcopy")
            .stdin(Stdio::piped())
            .spawn()
    } else {
        process::Command::new("xclip")
            .args(&["-sel", "clip"])
            .stdin(Stdio::piped())
            .spawn()
    }
}

// runs the `open` shell command
fn open_external(url: &str) -> io::Result<()> {
    process::Command::new("open")
        .arg(url)
        .output()
        .and_then(|_| Ok(()))
}
