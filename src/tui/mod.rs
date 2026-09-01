//! The interactive app, shown when `steroids` runs with no subcommand.

mod app;
pub(crate) mod job;
mod ui;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use crate::store::Store;
use app::App;

/// How long to wait for a keystroke before redrawing. Also the tick that drives
/// search debouncing and background-job polling.
const TICK: Duration = Duration::from_millis(80);

pub fn run(root: PathBuf, store: Store) -> Result<()> {
    let mut app = App::new(root, store)?;
    // ratatui::run installs a panic hook that restores the terminal first, so a
    // crash cannot leave the user staring at a broken shell.
    ratatui::run(|terminal| -> Result<()> {
        while !app.quit {
            terminal.draw(|frame| ui::draw(frame, &mut app))?;

            if event::poll(TICK)? {
                match event::read()? {
                    // Windows reports press and release; acting on both would
                    // double every keystroke.
                    Event::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key)?,
                    _ => {}
                }
            } else {
                app.tick()?;
            }
            app.poll_jobs()?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::app::Screen;
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent};

    /// Render every screen against a fake terminal. This is the only way to
    /// catch a panic from a bad layout or slice without a real TTY.
    #[test]
    fn every_screen_renders() -> Result<()> {
        let root = std::env::var("STEROIDS_TEST_ROOT").unwrap_or_default();
        if root.is_empty() {
            return Ok(());
        }
        let root = PathBuf::from(root);
        let mut app = App::new(root.clone(), Store::open(&root)?)?;
        let mut terminal = Terminal::new(TestBackend::new(100, 30))?;

        let press = |app: &mut App, code: KeyCode| app.on_key(KeyEvent::from(code));

        terminal.draw(|f| ui::draw(f, &mut app))?;
        press(&mut app, KeyCode::Enter)?; // repos -> files
        terminal.draw(|f| ui::draw(f, &mut app))?;
        press(&mut app, KeyCode::Enter)?; // files -> preview
        terminal.draw(|f| ui::draw(f, &mut app))?;
        press(&mut app, KeyCode::Esc)?;
        press(&mut app, KeyCode::Esc)?;

        press(&mut app, KeyCode::Char('/'))?;
        for c in "max_retries".chars() {
            press(&mut app, KeyCode::Char(c))?;
        }
        app.run_search()?;
        terminal.draw(|f| ui::draw(f, &mut app))?;
        assert!(!app.hits.is_empty(), "live search found nothing");

        press(&mut app, KeyCode::Enter)?; // hit -> preview
        terminal.draw(|f| ui::draw(f, &mut app))?;
        assert!(!app.preview_lines.is_empty(), "preview is empty");

        // Modals draw over whatever screen is beneath them.
        press(&mut app, KeyCode::Esc)?;
        press(&mut app, KeyCode::Esc)?;
        press(&mut app, KeyCode::Char('a'))?;
        terminal.draw(|f| ui::draw(f, &mut app))?;
        press(&mut app, KeyCode::Esc)?;
        press(&mut app, KeyCode::Char('d'))?;
        terminal.draw(|f| ui::draw(f, &mut app))?;
        Ok(())
    }

    /// A narrow window must not panic on any screen.
    #[test]
    fn narrow_terminal_does_not_panic() -> Result<()> {
        let root = std::env::var("STEROIDS_TEST_ROOT").unwrap_or_default();
        if root.is_empty() {
            return Ok(());
        }
        let root = PathBuf::from(root);
        let mut app = App::new(root.clone(), Store::open(&root)?)?;
        for (width, height) in [(20u16, 6u16), (40, 10), (200, 60)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height))?;
            for screen in [
                Screen::Repos,
                Screen::Files,
                Screen::Search,
                Screen::Preview,
            ] {
                app.screen = screen;
                terminal.draw(|f| ui::draw(f, &mut app))?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod snapshot {
    #[allow(unused_imports)]
    use super::app::Screen;
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent};

    #[test]
    #[ignore]
    fn long_query_scrolls() -> Result<()> {
        let root = PathBuf::from(std::env::var("STEROIDS_TEST_ROOT").unwrap());
        let mut app = App::new(root.clone(), Store::open(&root)?)?;
        let mut terminal = Terminal::new(TestBackend::new(60, 12))?;
        app.on_key(KeyEvent::from(KeyCode::Char('/')))?;
        for c in "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaZZ".chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(c)))?;
        }
        terminal.draw(|f| ui::draw(f, &mut app))?;
        let buf = terminal.backend().buffer();
        let mut row = String::new();
        for x in 0..buf.area.width {
            row.push_str(buf[(x, 2)].symbol());
        }
        println!("INPUT ROW: {}", row);
        assert!(row.contains("ZZ"), "caret text scrolled out of view");
        Ok(())
    }

    #[test]
    #[ignore]
    fn print_screens() -> Result<()> {
        let root = PathBuf::from(std::env::var("STEROIDS_TEST_ROOT").unwrap());
        let mut app = App::new(root.clone(), Store::open(&root)?)?;
        let mut terminal = Terminal::new(TestBackend::new(96, 22))?;
        let dump = |t: &Terminal<TestBackend>, label: &str| {
            println!("\n=== {label} ===");
            let buf = t.backend().buffer();
            for y in 0..buf.area.height {
                let mut line = String::new();
                for x in 0..buf.area.width {
                    line.push_str(buf[(x, y)].symbol());
                }
                println!("{}", line.trim_end());
            }
        };
        terminal.draw(|f| ui::draw(f, &mut app))?;
        dump(&terminal, "REPOS");

        app.on_key(KeyEvent::from(KeyCode::Char('/')))?;
        for c in "asyncio.gather".chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(c)))?;
        }
        app.run_search()?;
        terminal.draw(|f| ui::draw(f, &mut app))?;
        dump(&terminal, "SEARCH");

        app.on_key(KeyEvent::from(KeyCode::Enter))?;
        terminal.draw(|f| ui::draw(f, &mut app))?;
        dump(&terminal, "PREVIEW");

        app.on_key(KeyEvent::from(KeyCode::Esc))?;
        app.on_key(KeyEvent::from(KeyCode::Esc))?;
        app.on_key(KeyEvent::from(KeyCode::Char('a')))?;
        terminal.draw(|f| ui::draw(f, &mut app))?;
        dump(&terminal, "ADD MODAL");
        Ok(())
    }
}

#[cfg(test)]
mod interaction {
    use super::app::{App, Screen};
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent};

    /// Removing the repository you are browsing must return you to the list,
    /// not leave you looking at an empty file pane for something gone.
    #[test]
    fn removing_browsed_repo_returns_to_list() -> Result<()> {
        let root = std::env::var("STEROIDS_TEST_ROOT").unwrap_or_default();
        if root.is_empty() {
            return Ok(());
        }
        // Work on a copy: this test deletes a repository.
        let source = PathBuf::from(root);
        let scratch = std::env::temp_dir().join(format!("steroids-rm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch)?;
        for file in ["corpus.db", "blobs.bin"] {
            std::fs::copy(source.join(file), scratch.join(file))?;
        }

        let mut app = App::new(scratch.clone(), Store::open(&scratch)?)?;
        let mut terminal = Terminal::new(TestBackend::new(90, 24))?;
        let victim = app.repos[0].name.clone();

        app.on_key(KeyEvent::from(KeyCode::Enter))?; // into its files
        assert_eq!(app.screen, Screen::Files);
        assert_eq!(app.files_repo, victim);

        // Remove it out of band, as the worker thread would.
        let mut store = Store::open(&scratch)?;
        assert!(store.remove_repo(&victim)?);
        drop(store);
        app.tx
            .send(super::job::Msg::Done("removed".into()))
            .unwrap();
        app.poll_jobs()?;

        assert_eq!(app.screen, Screen::Repos, "stayed on dead repo");
        assert!(app.files.is_empty(), "stale file list retained");
        assert!(!app.repos.iter().any(|summary| summary.name == victim));
        terminal.draw(|f| ui::draw(f, &mut app))?;

        std::fs::remove_dir_all(&scratch)?;
        Ok(())
    }
}

#[cfg(test)]
mod resources {
    use super::app::App;
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent};

    /// The draw loop runs for as long as the user leaves the app open, so any
    /// per-frame allocation that is not released accumulates without bound.
    ///
    /// Measured by allocation count rather than resident set: `ps` reports the
    /// whole process, and cargo runs tests in parallel, so another test
    /// allocating at the same moment would be attributed here. Counting this
    /// thread's own allocations is exact and unaffected by neighbours.
    ///
    /// Needs a populated corpus to exercise real lists; set STEROIDS_TEST_ROOT
    /// to one, otherwise there is nothing to measure and the test skips.
    #[test]
    fn long_session_does_not_grow() -> Result<()> {
        let root = std::env::var("STEROIDS_TEST_ROOT").unwrap_or_default();
        if root.is_empty() {
            println!("SKIP: set STEROIDS_TEST_ROOT to a populated corpus");
            return Ok(());
        }
        let root = PathBuf::from(root);
        let mut app = App::new(root.clone(), Store::open(&root)?)?;
        let mut terminal = Terminal::new(TestBackend::new(120, 40))?;

        let drive =
            |app: &mut App, terminal: &mut Terminal<TestBackend>, frames: usize| -> Result<()> {
                for round in 0..frames {
                    terminal.draw(|f| ui::draw(f, app))?;
                    // The deepest navigation path: repo list, file list, preview,
                    // and back out again.
                    app.on_key(KeyEvent::from(KeyCode::Enter))?;
                    app.on_key(KeyEvent::from(KeyCode::Enter))?;
                    app.on_key(KeyEvent::from(KeyCode::Esc))?;
                    app.on_key(KeyEvent::from(KeyCode::Esc))?;
                    if round % 7 == 0 {
                        app.on_key(KeyEvent::from(KeyCode::Down))?;
                    }
                    app.poll_jobs()?;
                }
                Ok(())
            };

        // Warm up so one-off setup is not counted as growth.
        drive(&mut app, &mut terminal, 300)?;

        // Retained heap after warmup, and again after a long session. Any
        // structure that grows per frame shows up as a difference.
        let retained = |app: &App| -> usize {
            app.repos.len() * std::mem::size_of::<crate::store::RepoSummary>()
                + app.files.len() * 64
                + app.preview_lines.iter().map(|l| l.len()).sum::<usize>()
                + app.hits.len() * 256
        };
        let before = retained(&app);
        drive(&mut app, &mut terminal, 3000)?;
        let after = retained(&app);

        println!("  retained {before} -> {after} bytes over 3000 frames");
        assert!(
            after <= before,
            "state grew from {before} to {after} bytes across a long session"
        );
        Ok(())
    }
}
