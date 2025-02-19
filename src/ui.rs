//! The UI is what drives the interactive phetch application: it
//! spawns threads to fetch Gopher pages and download binary files, it
//! manages the opened pages (Views), it asks the focused View to
//! respond to user input, and it performs actions based on what the
//! View returns - like opening a telnet client, or displaying an
//! error on the status line.
//!
//! The UI also directly responds to user input on its own, such as
//! ctrl-q to quit the app or keyboard entry during an input prompt.
//!
//! Finally, the UI is what prints to the screen - each View just
//! renders its content to a String. The UI is what draws it.

mod action;
mod mode;
mod view;
pub use self::{action::Action, mode::Mode, view::View};

use crate::{
    bookmarks, color,
    config::Config,
    gopher::{self, Type},
    help, history,
    menu::Menu,
    terminal,
    text::Text,
    utils, BUG_URL,
};
use std::{
    cell::RefCell,
    io::{stdin, stdout, Result, Stdout, Write},
    process::{self, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};
use termion::{
    input::TermRead,
    raw::{IntoRawMode, RawTerminal},
    terminal_size,
};

/// Alias for a termion Key event.
pub type Key = termion::event::Key;

/// How many lines to jump by when using page up/down.
pub const SCROLL_LINES: usize = 15;

/// How big the longest line can be, for the purposes of calculating
/// margin sizes. We often draw longer lines than this and allow
/// wrapping in text views.
pub const MAX_COLS: usize = 77;

/// Fatal errors. In general we want to try and catch any errors
/// (network, parsing gopher response, etc) and just show an error
/// message in the status bar, but if we can't write to STDOUT or
/// control the screen, we need to just crash.
const ERR_RAW_MODE: &str = "Fatal Error using Raw Mode.";
const ERR_SCREEN: &str = "Fatal Error using Alternate Screen.";
const ERR_STDOUT: &str = "Fatal Error writing to STDOUT.";

/// UI is mainly concerned with drawing to the screen, managing the
/// active views, and responding to user input.
pub struct UI {
    /// Current loaded Gopher views. Menu or Text
    views: Vec<Box<dyn View>>,
    /// Index of currently focused View
    focused: usize,
    /// Does the UI need to be entirely redrawn?
    dirty: bool,
    /// Is the UI running?
    running: bool,
    /// Size of screen (cols, rows)
    pub size: (usize, usize),
    /// Status message to display on screen, if any
    status: String,
    /// User config. Command line options + phetch.conf
    config: Config,
    out: RefCell<RawTerminal<Stdout>>,
}

impl UI {
    /// Create a new phetch application from a user provided config.
    pub fn new(config: Config) -> UI {
        let mut size = (0, 0);
        if let Ok((cols, rows)) = terminal_size() {
            size = (cols as usize, rows as usize);
        };

        // Store raw terminal but don't enable it yet or switch the
        // screen. We don't want to stare at a fully blank screen
        // while waiting for a slow page to load.
        let out = stdout().into_raw_mode().expect(ERR_RAW_MODE);
        out.suspend_raw_mode().expect(ERR_RAW_MODE);

        UI {
            views: vec![],
            focused: 0,
            dirty: true,
            running: true,
            size,
            config,
            status: String::new(),
            out: RefCell::new(out),
        }
    }

    /// Prepare stdout for writing. Should be used in interactive
    /// mode, eg inside run()
    pub fn startup(&mut self) {
        let mut out = self.out.borrow_mut();
        out.activate_raw_mode().expect(ERR_RAW_MODE);
        write!(out, "{}", terminal::ToAlternateScreen).expect(ERR_SCREEN);
    }

    /// Clean up after ourselves. Should only be used after running in
    /// interactive mode.
    pub fn shutdown(&mut self) {
        let mut out = self.out.borrow_mut();
        write!(out, "{}", terminal::ToMainScreen).expect(ERR_SCREEN);
    }

    /// Main loop.
    pub fn run(&mut self) -> Result<()> {
        self.startup();
        while self.running {
            self.draw()?;
            self.update();
        }
        self.shutdown();
        Ok(())
    }

    /// Print the current view to the screen in rendered form.
    pub fn draw(&mut self) -> Result<()> {
        let status = self.render_status();
        if self.dirty {
            let screen = self.render()?;
            let mut out = self.out.borrow_mut();
            write!(
                out,
                "{}{}{}{}",
                terminal::Goto(1, 1),
                terminal::HideCursor,
                screen,
                status,
            )?;
            out.flush()?;
            self.dirty = false;
        } else {
            let mut out = self.out.borrow_mut();
            out.write_all(status.as_ref())?;
            out.flush()?;
        }
        Ok(())
    }

    /// Accept user input and update data.
    pub fn update(&mut self) {
        let action = self.process_view_input();
        if !action.is_none() {
            self.status.clear();
        }
        if let Err(e) = self.process_action(action) {
            self.set_status(&format!("{}{}{}", color::Red, e, terminal::HideCursor));
        }
    }

    /// Open a URL - Gopher, internal, telnet, or something else.
    pub fn open(&mut self, title: &str, url: &str) -> Result<()> {
        // no open loops
        if let Some(view) = self.views.get(self.focused) {
            if view.url() == url {
                return Ok(());
            }
        }

        // telnet
        if url.starts_with("telnet://") {
            return self.telnet(url);
        }

        // non-gopher URL
        if url.contains("://") && !url.starts_with("gopher://") {
            self.dirty = true;
            return if self.confirm(&format!("Open external URL? {}", url)) {
                utils::open_external(url)
            } else {
                Ok(())
            };
        }

        // binary downloads
        let typ = gopher::type_for_url(url);
        if typ.is_download() {
            self.dirty = true;
            return if self.confirm(&format!("Download {}?", url)) {
                self.download(url)
            } else {
                Ok(())
            };
        }

        self.load(title, url).and_then(|view| {
            self.add_view(view);
            Ok(())
        })
    }

    /// Download a binary file. Used by `open()` internally.
    fn download(&mut self, url: &str) -> Result<()> {
        let url = url.to_string();
        let (tls, tor) = (self.config.tls, self.config.tor);
        self.spinner(&format!("Downloading {}", url), move || {
            gopher::download_url(&url, tls, tor)
        })
        .and_then(|res| res)
        .and_then(|(path, bytes)| {
            self.set_status(
                format!(
                    "Download complete! {} saved to {}",
                    utils::human_bytes(bytes),
                    path
                )
                .as_ref(),
            );
            Ok(())
        })
    }

    /// Fetches a URL and returns a View for its content.
    fn load(&mut self, title: &str, url: &str) -> Result<Box<dyn View>> {
        // on-line help
        if url.starts_with("gopher://phetch/") {
            return self.load_internal(url);
        }
        // record history urls
        let hurl = url.to_string();
        let hname = title.to_string();
        thread::spawn(move || history::save(&hname, &hurl));
        // request thread
        let thread_url = url.to_string();
        let (tls, tor) = (self.config.tls, self.config.tor);
        // don't spin on first ever request
        let (tls, res) = if self.views.is_empty() {
            gopher::fetch_url(&thread_url, tls, tor)?
        } else {
            self.spinner("", move || gopher::fetch_url(&thread_url, tls, tor))??
        };
        let typ = gopher::type_for_url(&url);
        match typ {
            Type::Menu | Type::Search => Ok(Box::new(Menu::from(url, res, &self.config, tls))),
            Type::Text | Type::HTML => Ok(Box::new(Text::from(url, res, &self.config, tls))),
            _ => Err(error!("Unsupported Gopher Response: {:?}", typ)),
        }
    }

    /// Get Menu for on-line help, home page, etc, ex: gopher://phetch/1/help/types
    fn load_internal(&mut self, url: &str) -> Result<Box<dyn View>> {
        if let Some(source) = help::lookup(
            &url.trim_start_matches("gopher://phetch/")
                .trim_start_matches("1/"),
        ) {
            Ok(Box::new(Menu::from(url, source, &self.config, false)))
        } else {
            Err(error!("phetch URL not found: {}", url))
        }
    }

    /// # of visible columns
    fn cols(&self) -> u16 {
        self.size.0 as u16
    }

    /// # of visible row
    fn rows(&self) -> u16 {
        self.size.1 as u16
    }

    /// Set the current columns and rows.
    fn term_size(&mut self, cols: usize, rows: usize) {
        self.size = (cols, rows);
    }

    /// Show a spinner while running a thread. Used to make gopher requests or
    /// download files.
    fn spinner<T: Send + 'static, F: 'static + Send + FnOnce() -> T>(
        &mut self,
        label: &str,
        work: F,
    ) -> Result<T> {
        let req = thread::spawn(work);

        let (tx, rx) = mpsc::channel();
        let label = label.to_string();
        let rows = self.rows() as u16;
        thread::spawn(move || loop {
            for i in 0..=3 {
                if rx.try_recv().is_ok() {
                    return;
                }
                print!(
                    "{}{}{}{}{}{}{}",
                    terminal::Goto(1, rows),
                    terminal::HideCursor,
                    label,
                    ".".repeat(i),
                    terminal::ClearUntilNewline,
                    color::Reset,
                    terminal::ShowCursor,
                );
                stdout().flush().expect(ERR_STDOUT);
                thread::sleep(Duration::from_millis(500));
            }
        });

        let result = req.join();
        tx.send(true).expect("Fatal Error in Spinner channel."); // stop spinner
        self.dirty = true;
        result.map_err(|e| error!("Spinner error: {:?}", e))
    }

    /// Create a rendered String for the current View in its current state.
    pub fn render(&mut self) -> Result<String> {
        // TODO: only get size on SIGWINCH
        if let Ok((cols, rows)) = terminal_size() {
            self.term_size(cols as usize, rows as usize);
            if !self.views.is_empty() && self.focused < self.views.len() {
                if let Some(view) = self.views.get_mut(self.focused) {
                    view.term_size(cols as usize, rows as usize);
                    return Ok(view.render());
                }
            }
            Err(error!(
                "fatal: No focused View. Please file a bug: {}",
                BUG_URL
            ))
        } else {
            Err(error!(
                "fatal: Can't get terminal size. Please file a bug: {}",
                BUG_URL
            ))
        }
    }

    /// Set the status line's content.
    fn set_status(&mut self, status: &str) {
        self.status = status.replace('\n', "\\n").replace('\r', "\\r");
    }

    /// Render the connection status (TLS or Tor).
    fn render_conn_status(&self) -> Option<String> {
        let view = self.views.get(self.focused)?;
        if view.is_tls() {
            let status = color_string!("TLS", Black, GreenBG);
            return Some(format!(
                "{}{}",
                terminal::Goto(self.cols() - 3, self.rows()),
                if self.config.emoji { "🔐" } else { &status },
            ));
        } else if view.is_tor() {
            let status = color_string!("TOR", Bold, White, MagentaBG);
            return Some(format!(
                "{}{}",
                terminal::Goto(self.cols() - 3, self.rows()),
                if self.config.emoji { "🧅" } else { &status },
            ));
        }
        None
    }

    /// Render the status line.
    fn render_status(&self) -> String {
        format!(
            "{}{}{}{}{}{}",
            terminal::HideCursor,
            terminal::Goto(1, self.rows()),
            terminal::ClearCurrentLine,
            self.status,
            self.render_conn_status().unwrap_or_else(|| "".into()),
            color::Reset,
        )
    }

    /// Add a View to the app's currently opened Views.
    fn add_view(&mut self, view: Box<dyn View>) {
        self.dirty = true;
        if !self.views.is_empty() && self.focused < self.views.len() - 1 {
            self.views.truncate(self.focused + 1);
        }
        self.views.push(view);
        if self.views.len() > 1 {
            self.focused += 1;
        }
    }

    /// Ask user to confirm action with ENTER or Y.
    fn confirm(&self, question: &str) -> bool {
        let rows = self.rows();

        let mut out = self.out.borrow_mut();
        write!(
            out,
            "{}{}{}{} [Y/n]: {}",
            color::Reset,
            terminal::Goto(1, rows),
            terminal::ClearCurrentLine,
            question,
            terminal::ShowCursor,
        )
        .expect(ERR_STDOUT);
        out.flush().expect(ERR_STDOUT);

        if let Some(Ok(key)) = stdin().keys().next() {
            match key {
                Key::Char('\n') => true,
                Key::Char('y') | Key::Char('Y') => true,
                _ => false,
            }
        } else {
            false
        }
    }

    /// Prompt user for input and return what was entered, if anything.
    fn prompt(&self, prompt: &str, value: &str) -> Option<String> {
        let rows = self.rows();
        let mut input = value.to_string();

        let mut out = self.out.borrow_mut();
        write!(
            out,
            "{}{}{}{}{}{}",
            color::Reset,
            terminal::Goto(1, rows),
            terminal::ClearCurrentLine,
            prompt,
            input,
            terminal::ShowCursor,
        )
        .expect(ERR_STDOUT);
        out.flush().expect(ERR_STDOUT);

        for k in stdin().keys() {
            if let Ok(key) = k {
                match key {
                    Key::Char('\n') => {
                        write!(
                            out,
                            "{}{}",
                            terminal::ClearCurrentLine,
                            terminal::HideCursor
                        )
                        .expect(ERR_STDOUT);
                        out.flush().expect(ERR_STDOUT);
                        return Some(input);
                    }
                    Key::Char(c) => input.push(c),
                    Key::Esc | Key::Ctrl('c') => {
                        write!(
                            out,
                            "{}{}",
                            terminal::ClearCurrentLine,
                            terminal::HideCursor
                        )
                        .expect(ERR_STDOUT);
                        out.flush().expect(ERR_STDOUT);
                        return None;
                    }
                    Key::Backspace | Key::Delete => {
                        input.pop();
                    }
                    _ => {}
                }
            } else {
                break;
            }

            write!(
                out,
                "{}{}{}{}",
                terminal::Goto(1, rows),
                terminal::ClearCurrentLine,
                prompt,
                input,
            )
            .expect(ERR_STDOUT);
            out.flush().expect(ERR_STDOUT);
        }

        if !input.is_empty() {
            Some(input)
        } else {
            None
        }
    }

    /// Opens an interactive telnet session.
    fn telnet(&mut self, url: &str) -> Result<()> {
        let gopher::Url { host, port, .. } = gopher::parse_url(url);
        let out = self.out.borrow_mut();
        out.suspend_raw_mode().expect(ERR_RAW_MODE);
        let mut cmd = process::Command::new("telnet")
            .arg(host)
            .arg(port)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .spawn()?;
        cmd.wait()?;
        out.activate_raw_mode().expect(ERR_RAW_MODE);
        self.dirty = true; // redraw when finished with session
        Ok(())
    }

    /// Asks the current View to process user input and produce an Action.
    fn process_view_input(&mut self) -> Action {
        if let Some(view) = self.views.get_mut(self.focused) {
            if let Ok(key) = stdin()
                .keys()
                .nth(0)
                .ok_or_else(|| Action::Error("stdin.keys() error".to_string()))
            {
                if let Ok(key) = key {
                    return view.respond(key);
                }
            }
        }

        Action::Error("No Gopher page loaded.".into())
    }

    /// Ctrl-Z: Suspend Unix process w/ SIGTSTP.
    fn suspend(&mut self) {
        let mut out = self.out.borrow_mut();
        write!(out, "{}", terminal::ToMainScreen).expect(ERR_SCREEN);
        out.flush().expect(ERR_STDOUT);
        unsafe { libc::raise(libc::SIGTSTP) };
        write!(out, "{}", terminal::ToAlternateScreen).expect(ERR_SCREEN);
        out.flush().expect(ERR_STDOUT);
        self.dirty = true;
    }

    /// Given an Action from a View in response to user input, do the
    /// action.
    fn process_action(&mut self, action: Action) -> Result<()> {
        match action {
            Action::List(actions) => {
                for action in actions {
                    self.process_action(action)?;
                }
            }
            Action::Keypress(Key::Ctrl('c')) => {
                self.status = "\x1b[90m(Use q to quit)\x1b[0m".into()
            }
            Action::Keypress(Key::Ctrl('z')) => self.suspend(),
            Action::Keypress(Key::Esc) => {}
            Action::Error(e) => return Err(error!(e)),
            Action::Redraw => self.dirty = true,
            Action::Draw(s) => {
                let mut out = self.out.borrow_mut();
                out.write_all(s.as_ref())?;
                out.flush()?;
            }
            Action::Status(s) => self.set_status(&s),
            Action::Open(title, url) => self.open(&title, &url)?,
            Action::Prompt(query, fun) => {
                if let Some(response) = self.prompt(&query, "") {
                    self.process_action(fun(response))?;
                }
            }
            Action::Keypress(Key::Left) | Action::Keypress(Key::Backspace) => {
                if self.focused > 0 {
                    self.dirty = true;
                    self.focused -= 1;
                }
            }
            Action::Keypress(Key::Right) => {
                if self.focused < self.views.len() - 1 {
                    self.dirty = true;
                    self.focused += 1;
                }
            }
            Action::Keypress(Key::Char(key)) | Action::Keypress(Key::Ctrl(key)) => match key {
                'a' => self.open("History", "gopher://phetch/1/history")?,
                'b' => self.open("Bookmarks", "gopher://phetch/1/bookmarks")?,
                'g' => {
                    if let Some(url) = self.prompt("Go to URL: ", "") {
                        self.open(&url, &url)?;
                    }
                }
                'h' => self.open("Help", "gopher://phetch/1/help")?,
                'r' => {
                    if let Some(view) = self.views.get(self.focused) {
                        let url = view.url();
                        let raw = view.raw().to_string();
                        let mut text = Text::from(url, raw, &self.config, view.is_tls());
                        text.wide = true;
                        self.add_view(Box::new(text));
                    }
                }
                's' => {
                    if let Some(view) = self.views.get(self.focused) {
                        let url = view.url();
                        match bookmarks::save(&url, &url) {
                            Ok(()) => {
                                let msg = format!("Saved bookmark: {}", url);
                                self.set_status(&msg);
                            }
                            Err(e) => return Err(error!("Save failed: {}", e)),
                        }
                    }
                }
                'u' => {
                    if let Some(view) = self.views.get(self.focused) {
                        let current_url = view.url();
                        if let Some(url) = self.prompt("Current URL: ", &current_url) {
                            if url != current_url {
                                self.open(&url, &url)?;
                            }
                        }
                    }
                }
                'y' => {
                    if let Some(view) = self.views.get(self.focused) {
                        let url = view.url();
                        utils::copy_to_clipboard(&url)?;
                        let msg = format!("Copied {} to clipboard.", url);
                        self.set_status(&msg);
                    }
                }
                'w' => {
                    self.config.wide = !self.config.wide;
                    if let Some(view) = self.views.get_mut(self.focused) {
                        let w = view.wide();
                        view.set_wide(!w);
                        self.dirty = true;
                    }
                }
                'q' => self.running = false,
                c => return Err(error!("Unknown keypress: {}", c)),
            },
            _ => (),
        }
        Ok(())
    }
}

impl Drop for UI {
    fn drop(&mut self) {
        let mut out = self.out.borrow_mut();
        write!(out, "{}{}", color::Reset, terminal::ShowCursor).expect(ERR_STDOUT);
        out.flush().expect(ERR_STDOUT);
    }
}
