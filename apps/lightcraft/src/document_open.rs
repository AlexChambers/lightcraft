//! Queue native document requests until a usable library and an idle importer are available.
//! Paths follow the same add-in-place import as command-line files and window drops.

use std::collections::VecDeque;
use std::path::PathBuf;

use lightcraft_ui_egui::LightcraftApp;

#[derive(Default)]
pub struct PendingOpens {
    batches: VecDeque<Vec<String>>,
}

impl PendingOpens {
    pub fn enqueue(&mut self, paths: Vec<Result<PathBuf, String>>, notices: &mut Vec<String>) {
        let mut batch = Vec::new();
        for path in paths {
            match path.and_then(|p| {
                if !p.is_absolute() {
                    return Err("The open request contains a path that is not absolute.".into());
                }
                // The catalog stores UTF-8 paths. Never silently change an unrepresentable path
                // into the name of another file. Spaces, Unicode and literal % signs are kept.
                p.into_os_string().into_string().map_err(|_| "This file's name cannot be represented as UTF-8.".into())
            }) {
                Ok(path) => batch.push(path),
                Err(error) => notices.push(error),
            }
        }
        if !batch.is_empty() {
            self.batches.push_back(batch);
        }
    }

    pub fn tick(&mut self, app: &mut LightcraftApp) {
        if app.library_problem.as_ref().is_some_and(|p| !p.dismissed)
            || app.import.is_some()
            || app.scan.is_some()
            || app.lightroom.is_some()
            || app.sync_run.is_some()
            || app.sync_owns_changes
        {
            return;
        }
        if let Some(paths) = self.batches.front() {
            // start_documents uses the same worker, import params and library.import commits as
            // a window drop. The checks above keep requests queued while an importer is busy;
            // an unexpected start error is reported before removing that request.
            match lightcraft_ui_egui::import::start_documents(app, paths.clone()) {
                Ok(_) => {
                    self.batches.pop_front();
                }
                Err(error) => {
                    app.notices.push(error);
                    self.batches.pop_front();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightcraft_engine::Session;
    use lightcraft_ui_egui::{Services, panels::library_problem::LibraryProblem};

    fn app() -> LightcraftApp {
        LightcraftApp::new(Session::new(), Services::default())
    }

    #[test]
    fn native_paths_become_an_add_in_place_import() {
        let mut app = app();
        let mut pending = PendingOpens::default();
        pending.enqueue(vec![Ok(std::env::temp_dir().join("撮影 café #1.ARW")), Ok(std::env::temp_dir().join("missing %.jpg"))], &mut app.notices);
        pending.tick(&mut app);
        let task = app.import.as_ref().unwrap();
        assert_eq!(task.total, 2);
        assert!(pending.batches.is_empty());
    }

    #[test]
    fn cold_start_waits_for_library_and_drains_once() {
        let mut app = app();
        app.library_problem = Some(LibraryProblem::new("/unavailable", "offline"));
        let mut pending = PendingOpens::default();
        pending.enqueue(vec![Ok(std::env::temp_dir().join("one.dng"))], &mut app.notices);
        pending.tick(&mut app);
        assert!(app.import.is_none());
        assert_eq!(pending.batches.len(), 1);
        app.library_problem = None;
        pending.tick(&mut app);
        assert_eq!(app.import.as_ref().unwrap().total, 1);
        app.import = None;
        pending.tick(&mut app);
        assert!(app.import.is_none());
    }

    #[test]
    fn warm_requests_wait_in_order_and_temporary_session_can_receive_them() {
        let mut app = app();
        app.library_problem = Some(LibraryProblem { dismissed: true, ..Default::default() });
        let mut pending = PendingOpens::default();
        for path in ["one.jpg", "two.jpg"] {
            pending.enqueue(vec![Ok(std::env::temp_dir().join(path))], &mut app.notices);
        }
        pending.tick(&mut app);
        pending.tick(&mut app);
        assert_eq!(app.import.as_ref().unwrap().total, 1);
        assert_eq!(pending.batches.front().unwrap(), &[std::env::temp_dir().join("two.jpg").to_str().unwrap()]);
        assert_eq!(pending.batches.len(), 1);
        app.import = None;
        pending.tick(&mut app);
        assert_eq!(app.import.as_ref().unwrap().total, 1);
        assert!(pending.batches.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn bad_paths_report_errors_without_discarding_the_good_file() {
        use std::os::unix::ffi::OsStringExt;
        let mut app = app();
        let mut pending = PendingOpens::default();
        pending.enqueue(
            vec![
                Ok(std::ffi::OsString::from_vec(b"/tmp/\xff.jpg".to_vec()).into()),
                Ok("relative.jpg".into()),
                Err("invalid URL".into()),
                Ok("/tmp/good.jpg".into()),
            ],
            &mut app.notices,
        );
        pending.tick(&mut app);
        assert_eq!(app.notices.len(), 3);
        assert_eq!(app.import.as_ref().unwrap().total, 1);
    }
}
