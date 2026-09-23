//! Status of the long library-maintenance jobs, held outside the view that
//! starts them.
//!
//! Scan server library, Rebuild local cache and Preload all cover art all run
//! for minutes and report a status line on the Settings page. That page is
//! rebuilt on every visit (`Content::Settings` is constructed in
//! `RootView::navigate`), so state kept on the view died the moment the user
//! navigated away — the job carried on, since `cx.spawn` holds a *weak* handle
//! and a detached task runs to completion, but its `this.update` calls found
//! nothing and the status line came back empty on the next visit, reading as
//! if the button had never been pressed. The state therefore lives here, in an
//! entity the root view owns for the session, and the workers hold a strong
//! handle to it.

/// State of one of the maintenance jobs in the Library section.
///
/// All three are long, all three can fail in ways the user needs told about (a
/// server rescan is admin-only on Navidrome), and none has a meaningful total
/// — so they report a status line rather than a bar.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TaskState {
    #[default]
    Idle,
    Running(String),
    Done(String),
    Failed(String),
}

impl TaskState {
    pub fn is_running(&self) -> bool {
        matches!(self, TaskState::Running(_))
    }

    pub fn message(&self) -> Option<&str> {
        match self {
            TaskState::Idle => None,
            TaskState::Running(m) | TaskState::Done(m) | TaskState::Failed(m) => Some(m),
        }
    }
}

/// The three jobs' statuses, shared between the Settings page and the workers.
#[derive(Default)]
pub struct MaintenanceJobs {
    pub server_scan: TaskState,
    pub rebuild: TaskState,
    /// Cover-art preload, when it was started from the Settings page. A pass
    /// the root view starts after a sync runs silently and leaves this Idle.
    pub precache: TaskState,
}
