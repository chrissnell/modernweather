//! The supervisor's half of the `ComponentControl` capability.
//!
//! Observing is a load from the published state table; steering is a message to the
//! supervisor's one loop. Neither takes a lock on component state, because there is no
//! lock to take: the supervisor is the only owner (W3 §3.2), and that is what makes "a
//! component never sets its own state" a property of the architecture rather than a
//! convention.

use std::fmt;
use std::sync::Arc;

use mw_component::{
    ComponentControl, ComponentId, ComponentStatus, ControlError, Generation, LogControl,
};
use tokio::sync::mpsc;

use crate::publish::{Publication, StateTable};

/// What the management API can ask the supervisor to do.
#[derive(Debug)]
pub enum Command {
    /// Run a `Tick` body now, in addition to its schedule.
    RunNow(ComponentId),
    /// Start a stopped component.
    Start(ComponentId),
    /// Stop a running component: cancel, then await inside the grace.
    Stop(ComponentId),
}

/// The `ComponentControl` handle.
pub struct Control {
    published: Arc<Publication<StateTable>>,
    commands: mpsc::UnboundedSender<Command>,
    log: Arc<dyn LogControl>,
}

impl fmt::Debug for Control {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Control")
            .field("components", &self.published.load().components.len())
            .field("generation", &self.published.load().generation)
            .finish_non_exhaustive()
    }
}

impl Control {
    /// Assemble the handle the supervisor publishes as a root capability.
    pub fn new(
        published: Arc<Publication<StateTable>>,
        commands: mpsc::UnboundedSender<Command>,
        log: Arc<dyn LogControl>,
    ) -> Self {
        Self {
            published,
            commands,
            log,
        }
    }

    fn send(&self, command: Command) -> Result<(), ControlError> {
        self.commands
            .send(command)
            .map_err(|_| ControlError::ShuttingDown)
    }

    fn status(&self, id: &ComponentId) -> Result<ComponentStatus, ControlError> {
        self.published
            .load()
            .get(id)
            .cloned()
            .ok_or_else(|| ControlError::NoSuchComponent { id: id.clone() })
    }
}

impl ComponentControl for Control {
    fn components(&self) -> Vec<ComponentStatus> {
        self.published.load().components.clone()
    }

    fn component(&self, id: &ComponentId) -> Option<ComponentStatus> {
        self.published.load().get(id).cloned()
    }

    fn generation(&self) -> Generation {
        self.published.load().generation
    }

    fn run_now(&self, id: &ComponentId) -> Result<(), ControlError> {
        let status = self.status(id)?;
        // Rejected here rather than ignored in the loop, so the management API can answer
        // "that component has nothing to run" instead of reporting a success that did
        // nothing. A `Service` has no occurrence and an `OnReading` component runs when a
        // reading arrives.
        if matches!(status.trigger, "service" | "on_reading") {
            return Err(ControlError::NotRunnable {
                id: id.clone(),
                trigger: status.trigger,
            });
        }
        // Refused rather than accepted-and-dropped. The supervisor's own fireability check
        // would log the request and discard it, so without this the management API would
        // answer 200 for a check that never ran.
        if !matches!(status.state.as_str(), "running" | "backoff") {
            return Err(ControlError::NotLive {
                id: id.clone(),
                state: status.state.as_str(),
            });
        }
        self.send(Command::RunNow(id.clone()))
    }

    fn start(&self, id: &ComponentId) -> Result<(), ControlError> {
        self.status(id)?;
        self.send(Command::Start(id.clone()))
    }

    fn stop(&self, id: &ComponentId) -> Result<(), ControlError> {
        self.status(id)?;
        self.send(Command::Stop(id.clone()))
    }

    fn log_control(&self) -> &dyn LogControl {
        &*self.log
    }

    /// The publication's own signal, which is the one place a state change is committed.
    fn changes(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        Some(self.published.changes())
    }
}
