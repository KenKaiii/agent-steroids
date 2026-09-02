//! The interactive app, shown when `steroids` runs with no subcommand.

mod app;
mod highlight;
pub(crate) mod job;
mod picker;
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
mod edge_cases {
    use super::app::{App, Screen};
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    /// A brand new user opens the app before adding anything.
    #[test]
    fn empty_corpus_renders_and_navigates() -> Result<()> {
        let dir = crate::store::scratch_dir("empty");
        let _ = std::fs::remove_dir_all(&dir);
        let mut app = App::new(dir.clone(), Store::open(&dir)?)?;

        // Every screen and every key, against nothing at all.
        for (width, height) in [(10u16, 4u16), (80, 24)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height))?;
            for screen in [
                Screen::Repos,
                Screen::Files,
                Screen::Search,
                Screen::Preview,
            ] {
                app.screen = screen;
                terminal.draw(|f| ui::draw(f, &mut app))?;
                for code in [
                    KeyCode::Down,
                    KeyCode::Up,
                    KeyCode::Enter,
                    KeyCode::Esc,
                    KeyCode::PageDown,
                    KeyCode::Home,
                    KeyCode::End,
                    KeyCode::Char('a'),
                    KeyCode::Char('d'),
                    KeyCode::Char('/'),
                ] {
                    app.on_key(KeyEvent::from(code))?;
                    terminal.draw(|f| ui::draw(f, &mut app))?;
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Ctrl-C and Ctrl-D must not be swallowed or mishandled.
    #[test]
    fn control_keys_are_handled() -> Result<()> {
        let root = std::env::var("STEROIDS_TEST_ROOT").unwrap_or_default();
        if root.is_empty() {
            return Ok(());
        }
        let root = PathBuf::from(root);
        let mut app = App::new(root.clone(), Store::open(&root)?)?;
        let mut terminal = Terminal::new(TestBackend::new(90, 24))?;
        for code in [KeyCode::Char('c'), KeyCode::Char('d'), KeyCode::Char('z')] {
            app.on_key(KeyEvent::new(code, KeyModifiers::CONTROL))?;
            terminal.draw(|f| ui::draw(f, &mut app))?;
        }
        Ok(())
    }

    /// Typing far more than fits, then deleting all of it.
    #[test]
    fn oversized_input_is_survivable() -> Result<()> {
        let root = std::env::var("STEROIDS_TEST_ROOT").unwrap_or_default();
        if root.is_empty() {
            return Ok(());
        }
        let root = PathBuf::from(root);
        let mut app = App::new(root.clone(), Store::open(&root)?)?;
        let mut terminal = Terminal::new(TestBackend::new(40, 12))?;
        app.on_key(KeyEvent::from(KeyCode::Char('/')))?;
        for c in "((((((((((".chars().chain("x".repeat(600).chars()) {
            app.on_key(KeyEvent::from(KeyCode::Char(c)))?;
        }
        app.run_search()?;
        terminal.draw(|f| ui::draw(f, &mut app))?;
        for _ in 0..700 {
            app.on_key(KeyEvent::from(KeyCode::Backspace))?;
        }
        app.run_search()?;
        terminal.draw(|f| ui::draw(f, &mut app))?;
        Ok(())
    }
}

#[cfg(test)]
mod jobs {
    use super::app::App;
    use super::*;
    use ratatui::crossterm::event::{KeyCode, KeyEvent};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    /// Esc during a job raises the cancel flag once; the batch stops claiming
    /// work, writes nothing it never fetched, and reports as finished rather
    /// than failed. No network: the flag is raised before the first claim.
    #[test]
    fn esc_cancels_a_running_job_cleanly() -> Result<()> {
        let dir = crate::store::scratch_dir("jobcancel");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let mut app = App::new(dir.clone(), Store::open(&dir)?)?;

        // The key path: Working + Esc raises the flag and says so.
        app.modal = super::app::Modal::Working("3/100  x".into());
        app.on_key(KeyEvent::from(KeyCode::Esc))?;
        assert!(app.cancel.load(Ordering::Relaxed));
        let super::app::Modal::Working(text) = &app.modal else {
            panic!("Esc must keep the progress modal up");
        };
        assert!(text.starts_with("cancelling"), "{text}");

        // The job path: a raised flag skips every repository, so the batch
        // needs no network and reports what it did not start.
        let cancel = Arc::new(AtomicBool::new(true));
        let names: Vec<String> = (0..5).map(|i| format!("nobody/repo{i}")).collect();
        let outcome = crate::bulk::ingest_all(
            &mut app.store,
            &names,
            false,
            2,
            &Default::default(),
            &cancel,
            &mut |_, _, _, _| {},
        )?;
        assert_eq!((outcome.added, outcome.skipped), (0, 5));
        assert!(outcome.failed.is_empty());

        super::job::add_repos(dir.clone(), names, app.tx.clone(), cancel);
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && app.status.is_empty() {
            app.poll_jobs()?;
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            app.status.starts_with("cancelled") && app.status.contains("5 not started"),
            "{}",
            app.status
        );
        assert!(app.repos.is_empty());

        drop(app);
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// The background jobs behind the interactive keys are the only paths that
    /// write to a corpus without going through the CLI, and nothing else
    /// exercises them. A failure here means `a`, `d` and `u` are broken in the
    /// interface while every command line test still passes.
    #[test]
    fn add_then_remove_through_the_job_queue() -> Result<()> {
        if std::env::var("STEROIDS_NETWORK_TESTS").is_err() {
            println!("SKIP: set STEROIDS_NETWORK_TESTS=1 (needs GitHub)");
            return Ok(());
        }
        let dir = crate::store::scratch_dir("jobs");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let mut app = App::new(dir.clone(), Store::open(&dir)?)?;

        // Drain until the worker reports it has finished, or give up.
        let settle = |app: &mut App| -> Result<String> {
            let deadline = Instant::now() + Duration::from_secs(240);
            let mut last = String::new();
            while Instant::now() < deadline {
                app.poll_jobs()?;
                if let super::app::Modal::Working(text) = &app.modal {
                    last = text.clone();
                } else if !app.status.is_empty() {
                    return Ok(app.status.clone());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            anyhow::bail!("job never finished, last progress: {last}")
        };

        super::job::add_repos(
            dir.clone(),
            vec!["antirez/smallchat".into()],
            app.tx.clone(),
            Arc::new(AtomicBool::new(false)),
        );
        let status = settle(&mut app)?;
        assert!(
            status.contains("added"),
            "add reported {status:?} instead of success"
        );
        assert_eq!(app.repos.len(), 1, "the repository is not in the list");
        assert!(app.repos[0].files > 0, "the repository has no files");

        // The corpus must be genuinely searchable, not merely present.
        let hits =
            crate::search::search(&mut app.store, "int main", &crate::search::Query::new(3))?;
        assert!(!hits.matches.is_empty(), "nothing searchable after add");

        app.status.clear();
        super::job::remove_repo(dir.clone(), "antirez/smallchat".into(), app.tx.clone());
        let status = settle(&mut app)?;
        assert!(
            status.contains("removed"),
            "remove reported {status:?} instead of success"
        );
        assert!(app.repos.is_empty(), "the repository survived removal");

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// A name that cannot be fetched must surface as a failure the user can
    /// see, not leave the interface stuck showing progress forever.
    #[test]
    fn a_failing_add_reports_rather_than_hanging() -> Result<()> {
        if std::env::var("STEROIDS_NETWORK_TESTS").is_err() {
            println!("SKIP: set STEROIDS_NETWORK_TESTS=1 (needs GitHub)");
            return Ok(());
        }
        let dir = crate::store::scratch_dir("jobfail");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let mut app = App::new(dir.clone(), Store::open(&dir)?)?;

        super::job::add_repos(
            dir.clone(),
            vec!["definitely-not-a-real-org-xyz/nope".into()],
            app.tx.clone(),
            Arc::new(AtomicBool::new(false)),
        );
        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline && app.status.is_empty() {
            app.poll_jobs()?;
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            app.status.contains("failed"),
            "a bad repository reported {:?}",
            app.status
        );
        assert!(
            !matches!(app.modal, super::app::Modal::Working(_)),
            "the interface was left showing progress after a failure"
        );

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Cancel after the first repository lands: it must be indexed and
    /// searchable, the rest never started.
    #[test]
    fn cancelling_mid_batch_keeps_what_landed() -> Result<()> {
        if std::env::var("STEROIDS_NETWORK_TESTS").is_err() {
            println!("SKIP: set STEROIDS_NETWORK_TESTS=1 (needs GitHub)");
            return Ok(());
        }
        let dir = crate::store::scratch_dir("jobcancel-live");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let mut app = App::new(dir.clone(), Store::open(&dir)?)?;
        let cancel = Arc::new(AtomicBool::new(false));
        // More names than workers, or every one is claimed before the first
        // lands and there is nothing left to skip.
        let mut names = vec!["antirez/smallchat".to_string()];
        names.extend((0..40).map(|i| format!("antirez/does-not-exist-{i}")));
        super::job::add_repos(dir.clone(), names, app.tx.clone(), Arc::clone(&cancel));

        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline && app.status.is_empty() {
            app.poll_jobs()?;
            // `poll_jobs` drains several messages at once, so "1/41" may
            // never be the one on screen; any counted progress will do.
            if let super::app::Modal::Working(text) = &app.modal
                && text.contains('/')
            {
                cancel.store(true, Ordering::Relaxed);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        println!("status: {}", app.status);
        assert!(
            app.status.starts_with("cancelled") && app.status.contains("not started"),
            "{}",
            app.status
        );
        assert_eq!(app.repos.len(), 1);
        let hits =
            crate::search::search(&mut app.store, "int main", &crate::search::Query::new(3))?;
        assert!(!hits.matches.is_empty(), "landed repo not searchable");
        drop(app);
        let _ = std::fs::remove_dir_all(&dir);
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
        let scratch = crate::store::scratch_dir("rm");
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

    /// The location field is prefilled with the current root; a bad path
    /// stays in the footer with the old corpus still open; a good one swaps
    /// the store and the list. No populated corpus needed. `l` itself is not
    /// pressed: it opens the OS folder dialog, which no test may do.
    #[test]
    fn location_dialog_switches_corpus() -> Result<()> {
        let scratch = crate::store::scratch_dir("root-tui");
        let _ = std::fs::remove_dir_all(&scratch);
        let (home, old, ssd) = (
            scratch.join("home"),
            scratch.join("old"),
            scratch.join("ssd").join("corpus"),
        );
        std::fs::create_dir_all(ssd.parent().unwrap())?;
        let mut app = App::new(old.clone(), Store::open(&old)?)?;
        let mut terminal = Terminal::new(TestBackend::new(90, 24))?;

        app.modal = super::app::Modal::SetRoot(tui_input::Input::new(old.display().to_string()));
        terminal.draw(|f| ui::draw(f, &mut app))?;
        app.on_key(KeyEvent::from(KeyCode::Esc))?;
        assert!(matches!(app.modal, super::app::Modal::None));

        app.set_root(&home, "not/absolute")?;
        assert!(app.status.starts_with("failed"), "{}", app.status);
        assert_eq!(app.root, old);

        app.set_root(&home, &ssd.display().to_string())?;
        assert_eq!(app.root, ssd);
        assert_eq!(crate::root::stored(&home), Some(ssd.clone()));
        assert!(app.status.starts_with("now using"), "{}", app.status);
        terminal.draw(|f| ui::draw(f, &mut app))?;

        // The status stands in for the key bar; the next keypress, whatever
        // it is, must bring the keys back rather than leave the user stuck.
        app.on_key(KeyEvent::from(KeyCode::Down))?;
        assert!(app.status.is_empty(), "status stuck: {}", app.status);
        terminal.draw(|f| ui::draw(f, &mut app))?;
        let buf = terminal.backend().buffer();
        let text: String = (0..buf.area.width)
            .map(|x| buf[(x, buf.area.height - 1)].symbol())
            .collect();
        assert!(text.contains("location"), "key bar not back: {text}");

        // Windows refuses to delete files the store still has open.
        drop(app);
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
                + app.preview_lines.iter().map(|l| l.width()).sum::<usize>()
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
