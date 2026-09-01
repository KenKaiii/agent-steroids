//! Interactive state: what is on screen and what the keys do.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};

use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::ListState;
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use super::job::{self, Msg};
use crate::search::{self, Query};
use crate::store::Store;

/// Keystrokes are cheap but searches are not, so wait for a pause in typing.
pub const SEARCH_DEBOUNCE_TICKS: u8 = 2;
const SEARCH_LIMIT: usize = 40;
const PREVIEW_MAX_LINES: usize = 5000;
/// Files listed for one repository. A larger list is unreadable by scrolling
/// anyway, and holding every path of a huge repository is the app's biggest
/// avoidable allocation.
const MAX_FILES_LISTED: usize = 5000;

#[derive(PartialEq, Debug, Clone, Copy)]
pub enum Screen {
    Repos,
    Files,
    Search,
    Preview,
}

pub enum Modal {
    None,
    /// Typing repository names to add.
    AddRepo(Input),
    /// Confirming removal of the named repository.
    ConfirmRemove(String),
    /// A background job is running; the string is its latest progress line.
    Working(String),
}

pub struct SearchHit {
    pub repo: String,
    pub path: String,
    pub line_number: usize,
    pub scope: String,
    pub context: Vec<String>,
    /// Index within `context` of the line that matched.
    pub context_offset: usize,
}

pub struct App {
    pub root: PathBuf,
    pub store: Store,
    pub screen: Screen,
    pub modal: Modal,
    pub quit: bool,
    pub status: String,

    pub repos: Vec<crate::store::RepoSummary>,
    pub repos_state: ListState,
    /// Total bytes the corpus occupies, refreshed with the repo list.
    pub disk_bytes: u64,
    /// Bytes held by the trigram index alone, so the header can explain the
    /// gap between the per-repository sizes and the total.
    pub index_bytes: u64,

    /// (path, language, raw size) for the repository being browsed.
    pub files: Vec<(String, String, i64)>,
    pub files_state: ListState,
    pub files_repo: String,

    pub query: Input,
    pub hits: Vec<SearchHit>,
    pub hits_state: ListState,
    /// Ticks remaining before the pending query runs. None when nothing is due.
    pub pending_search: Option<u8>,
    pub searching_message: Option<String>,

    pub preview_title: String,
    pub preview_lines: Vec<String>,
    pub preview_scroll: usize,

    pub tx: Sender<Msg>,
    pub rx: Receiver<Msg>,
}

impl App {
    pub fn new(root: PathBuf, store: Store) -> Result<Self> {
        let (tx, rx) = channel();
        let mut app = Self {
            root,
            store,
            screen: Screen::Repos,
            modal: Modal::None,
            quit: false,
            status: String::new(),
            repos: Vec::new(),
            repos_state: ListState::default(),
            disk_bytes: 0,
            index_bytes: 0,
            files: Vec::new(),
            files_state: ListState::default(),
            files_repo: String::new(),
            query: Input::default(),
            hits: Vec::new(),
            hits_state: ListState::default(),
            pending_search: None,
            searching_message: None,
            preview_title: String::new(),
            preview_lines: Vec::new(),
            preview_scroll: 0,
            tx,
            rx,
        };
        app.reload_repos()?;
        Ok(app)
    }

    pub fn reload_repos(&mut self) -> Result<()> {
        self.repos = self.store.list_repos()?;
        let size_of = |name: &str| {
            std::fs::metadata(self.root.join(name))
                .map(|meta| meta.len())
                .unwrap_or(0)
        };
        self.index_bytes = size_of("corpus.db");
        self.disk_bytes = self.index_bytes + size_of("blobs.bin");
        if self.repos.is_empty() {
            self.repos_state.select(None);
        } else {
            let index = self
                .repos_state
                .selected()
                .unwrap_or(0)
                .min(self.repos.len() - 1);
            self.repos_state.select(Some(index));
        }
        Ok(())
    }

    // -- background jobs ----------------------------------------------------

    /// Drain worker messages, refreshing anything a finished job changed.
    pub fn poll_jobs(&mut self) -> Result<()> {
        let mut finished = false;
        while let Ok(message) = self.rx.try_recv() {
            match message {
                Msg::Progress(text) => self.modal = Modal::Working(text),
                Msg::Done(text) => {
                    self.status = text;
                    self.modal = Modal::None;
                    finished = true;
                }
                Msg::Failed(text) => {
                    self.status = format!("failed: {text}");
                    self.modal = Modal::None;
                    finished = true;
                }
            }
        }
        if finished {
            // The worker wrote through its own connection, so ours may hold a
            // stale view of both the repo list and the trigram stop list.
            self.store = Store::open(&self.root)?;
            self.reload_repos()?;

            let browsing_gone_repo = matches!(self.screen, Screen::Files | Screen::Preview)
                && !self
                    .repos
                    .iter()
                    .any(|summary| summary.name == self.files_repo);
            if browsing_gone_repo {
                // The repository being browsed was just removed; its file list
                // and preview now describe nothing.
                self.files.clear();
                self.files_state.select(None);
                self.preview_lines.clear();
                self.screen = Screen::Repos;
            } else if self.screen == Screen::Files {
                self.load_files(self.files_repo.clone())?;
            }

            if !self.query.value().is_empty() {
                self.run_search()?;
            }
        }
        Ok(())
    }

    // -- navigation ---------------------------------------------------------

    fn step(state: &mut ListState, len: usize, delta: isize) {
        if len == 0 {
            state.select(None);
            return;
        }
        let current = state.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, len as isize - 1);
        state.select(Some(next as usize));
    }

    /// Load one repository's file list, reusing what is already loaded.
    ///
    /// Navigating in and out of a repository is the commonest thing a user
    /// does, and re-querying on every entry churns a few hundred kilobytes of
    /// allocations per round trip for a list that has not changed.
    pub fn load_files(&mut self, repo: String) -> Result<()> {
        if self.files_repo == repo && !self.files.is_empty() {
            return Ok(());
        }
        self.files = self.store.list_files(&repo, MAX_FILES_LISTED)?;
        self.files_repo = repo;
        self.files_state
            .select(if self.files.is_empty() { None } else { Some(0) });
        Ok(())
    }

    /// Release the memory held by the file list and preview.
    ///
    /// `clear` keeps the backing allocation, which for a 5,000-entry list is
    /// the bulk of what is worth releasing when the user navigates away.
    fn release_browsing_memory(&mut self) {
        self.files = Vec::new();
        self.files_repo.clear();
        self.files_state.select(None);
        self.preview_lines = Vec::new();
        self.preview_title.clear();
    }

    fn open_preview(&mut self, repo: &str, path: &str, focus_line: usize) -> Result<()> {
        match self.store.read_path(repo, path)? {
            Some(content) => {
                let text = String::from_utf8_lossy(&content);
                self.preview_lines = text
                    .lines()
                    .take(PREVIEW_MAX_LINES)
                    .map(|line| line.to_string())
                    .collect();
                self.preview_title = format!("{repo}/{path}");
                // Show a little above the interesting line rather than putting
                // it flush against the top edge.
                self.preview_scroll = focus_line.saturating_sub(search::DEFAULT_CONTEXT_LINES + 1);
                self.screen = Screen::Preview;
            }
            None => self.status = format!("not in corpus: {repo}/{path}"),
        }
        Ok(())
    }

    // -- search -------------------------------------------------------------

    pub fn run_search(&mut self) -> Result<()> {
        let pattern = self.query.value().to_string();
        self.pending_search = None;
        if pattern.trim().is_empty() {
            self.hits.clear();
            self.hits_state.select(None);
            self.searching_message = None;
            return Ok(());
        }

        // simplification: the search runs on the draw thread, so a query
        // slower than the debounce would make typing stutter. Fine at the
        // tens-of-milliseconds this takes for hundreds of repositories; if it
        // ever is not, move it to a worker with its own Store and a generation
        // counter to discard stale results.
        let query = Query::new(SEARCH_LIMIT);
        match search::search(&mut self.store, &pattern, &query) {
            Ok(matches) => {
                self.hits = matches
                    .matches
                    .into_iter()
                    .map(|item| SearchHit {
                        // Derive from the match's own context start, so the
                        // highlight stays on the matched line whatever the
                        // context width happens to be.
                        context_offset: item.line_number - item.context_start(),
                        repo: item.repo,
                        path: item.path,
                        line_number: item.line_number,
                        scope: item.scope,
                        context: item.context,
                    })
                    .collect();
                self.hits_state
                    .select(if self.hits.is_empty() { None } else { Some(0) });
                self.searching_message = if self.hits.is_empty() {
                    // The same diagnosis the CLI prints, condensed to one line.
                    let facts = search::diagnose(&mut self.store, &pattern)?;
                    Some(super::ui::short_diagnosis(&facts))
                } else {
                    None
                };
            }
            Err(error) => {
                self.hits.clear();
                self.hits_state.select(None);
                // An incomplete regex is normal while typing, not a failure.
                self.searching_message = Some(error.to_string());
            }
        }
        Ok(())
    }

    /// Called once per tick; runs the query after typing pauses.
    pub fn tick(&mut self) -> Result<()> {
        if let Some(remaining) = self.pending_search {
            if remaining == 0 {
                self.run_search()?;
            } else {
                self.pending_search = Some(remaining - 1);
            }
        }
        Ok(())
    }

    // -- input --------------------------------------------------------------

    pub fn on_key(&mut self, key: KeyEvent) -> Result<()> {
        // Ctrl-C is not a signal in raw mode, so quitting is our job.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return Ok(());
        }

        match std::mem::replace(&mut self.modal, Modal::None) {
            Modal::Working(text) => {
                // Nothing to do but wait; keep showing progress.
                self.modal = Modal::Working(text);
                return Ok(());
            }
            Modal::AddRepo(mut input) => {
                match key.code {
                    KeyCode::Enter => {
                        let names: Vec<String> = input
                            .value()
                            .split_whitespace()
                            .map(|name| name.to_string())
                            .collect();
                        if names.is_empty() {
                            // Nothing typed yet: keep the dialog open rather
                            // than closing it as if the key did something.
                            self.modal = Modal::AddRepo(input);
                            return Ok(());
                        }
                        self.modal = Modal::Working("starting…".into());
                        job::add_repos(self.root.clone(), names, self.tx.clone());
                    }
                    KeyCode::Esc => {}
                    _ => {
                        input.handle_event(&ratatui::crossterm::event::Event::Key(key));
                        self.modal = Modal::AddRepo(input);
                    }
                }
                return Ok(());
            }
            Modal::ConfirmRemove(name) => {
                match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        self.modal = Modal::Working("starting…".into());
                        job::remove_repo(self.root.clone(), name, self.tx.clone());
                    }
                    _ => {}
                }
                return Ok(());
            }
            Modal::None => {}
        }

        match self.screen {
            Screen::Repos => self.on_key_repos(key)?,
            Screen::Files => self.on_key_files(key)?,
            Screen::Search => self.on_key_search(key)?,
            Screen::Preview => self.on_key_preview(key),
        }
        Ok(())
    }

    fn on_key_repos(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Down | KeyCode::Char('j') => {
                Self::step(&mut self.repos_state, self.repos.len(), 1)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                Self::step(&mut self.repos_state, self.repos.len(), -1)
            }
            KeyCode::Char('/') => {
                self.screen = Screen::Search;
                self.status.clear();
            }
            KeyCode::Enter | KeyCode::Right => {
                if let Some(index) = self.repos_state.selected() {
                    let name = self.repos[index].name.clone();
                    self.load_files(name)?;
                    self.screen = Screen::Files;
                }
            }
            KeyCode::Char('a') => self.modal = Modal::AddRepo(Input::default()),
            KeyCode::Char('d') => {
                if let Some(index) = self.repos_state.selected() {
                    self.modal = Modal::ConfirmRemove(self.repos[index].name.clone());
                }
            }
            KeyCode::Char('u') => {
                self.modal = Modal::Working("starting…".into());
                job::update_all(self.root.clone(), self.tx.clone());
            }
            _ => {}
        }
        Ok(())
    }

    fn on_key_files(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc | KeyCode::Left => {
                // Leaving the repository entirely: its file list and any
                // preview are dead weight until the user comes back.
                self.release_browsing_memory();
                self.screen = Screen::Repos;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                Self::step(&mut self.files_state, self.files.len(), 1)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                Self::step(&mut self.files_state, self.files.len(), -1)
            }
            KeyCode::Char('/') => self.screen = Screen::Search,
            KeyCode::Enter | KeyCode::Right => {
                if let Some(index) = self.files_state.selected() {
                    let path = self.files[index].0.clone();
                    let repo = self.files_repo.clone();
                    self.open_preview(&repo, &path, 0)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn on_key_search(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc => {
                self.screen = Screen::Repos;
                self.searching_message = None;
            }
            KeyCode::Down => Self::step(&mut self.hits_state, self.hits.len(), 1),
            KeyCode::Up => Self::step(&mut self.hits_state, self.hits.len(), -1),
            KeyCode::Enter => {
                if let Some(index) = self.hits_state.selected() {
                    let hit = &self.hits[index];
                    let (repo, path, line) = (hit.repo.clone(), hit.path.clone(), hit.line_number);
                    self.open_preview(&repo, &path, line)?;
                }
            }
            _ => {
                self.query
                    .handle_event(&ratatui::crossterm::event::Event::Key(key));
                self.pending_search = Some(SEARCH_DEBOUNCE_TICKS);
            }
        }
        Ok(())
    }

    fn on_key_preview(&mut self, key: KeyEvent) {
        let last = self.preview_lines.len().saturating_sub(1);
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc | KeyCode::Left => {
                self.screen = if self.hits.is_empty() {
                    Screen::Files
                } else {
                    Screen::Search
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.preview_scroll = (self.preview_scroll + 1).min(last)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.preview_scroll = self.preview_scroll.saturating_sub(1)
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                self.preview_scroll = (self.preview_scroll + 20).min(last)
            }
            KeyCode::PageUp => self.preview_scroll = self.preview_scroll.saturating_sub(20),
            KeyCode::Home => self.preview_scroll = 0,
            KeyCode::End => self.preview_scroll = last,
            _ => {}
        }
    }
}
