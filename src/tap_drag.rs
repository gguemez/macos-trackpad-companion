//! Single-finger tap-then-drag recognition. No platform events or timers.

use crate::report::Contact;
use crate::time::Timestamp;
use std::time::Duration;

// Deliberately shorter than the double-click window: a tap followed by an
// ordinary later pointer movement should not unexpectedly drag.
const FOLLOWUP_WINDOW: Duration = Duration::from_millis(300);
const FOLLOWUP_DISTANCE_MM: f64 = 3.0;

#[derive(Default)]
enum Stage {
    #[default]
    Idle,
    Armed {
        at: Timestamp,
        position: (f64, f64),
    },
    Pending {
        id: u8,
        origin: (f64, f64),
        at: Timestamp,
    },
    Dragging {
        id: u8,
    },
    // A finger added/replaced during a drag cannot become an accidental tap.
    Cancelled,
}

#[derive(Default)]
pub(crate) struct TapDrag {
    stage: Stage,
    last_position: Option<(f64, f64)>,
}

#[derive(Default)]
pub(crate) struct Action {
    pub press: bool,
    pub release: bool,
    pub suppress_taps: bool,
}

impl TapDrag {
    pub fn arm(&mut self, now: Timestamp) {
        if let Some(position) = self.last_position {
            self.stage = Stage::Armed { at: now, position };
        }
    }

    pub fn held(&self) -> bool {
        matches!(self.stage, Stage::Dragging { .. })
    }

    pub fn cancel(&mut self) -> bool {
        let held = self.held();
        self.stage = Stage::Idle;
        self.last_position = None;
        held
    }

    pub fn suppress_until_lift(&mut self) {
        self.stage = Stage::Cancelled;
    }

    pub fn update(
        &mut self,
        active: &[Contact],
        now: Timestamp,
        tap_time: Duration,
        tap_move_mm: f64,
    ) -> Action {
        let mut action = Action::default();
        let single = match active {
            [c] => Some(*c),
            _ => None,
        };
        if let Some(c) = single {
            self.last_position = Some((c.x, c.y));
        }
        self.stage = match std::mem::take(&mut self.stage) {
            Stage::Armed { at, position } => {
                if now.saturating_duration_since(at) > FOLLOWUP_WINDOW {
                    Stage::Idle
                } else if let Some(c) = single {
                    if distance(position, (c.x, c.y)) <= FOLLOWUP_DISTANCE_MM {
                        Stage::Pending {
                            id: c.id,
                            origin: (c.x, c.y),
                            at: now,
                        }
                    } else {
                        Stage::Idle
                    }
                } else if active.is_empty() {
                    Stage::Armed { at, position }
                } else {
                    Stage::Idle
                }
            }
            Stage::Pending { id, origin, at } => {
                if let Some(c) = single.filter(|c| c.id == id) {
                    if distance(origin, (c.x, c.y)) >= tap_move_mm
                        || now.saturating_duration_since(at) >= tap_time
                    {
                        action.press = true;
                        action.suppress_taps = true;
                        Stage::Dragging { id }
                    } else {
                        Stage::Pending { id, origin, at }
                    }
                } else {
                    // A quick lift takes the normal double-click path.
                    Stage::Idle
                }
            }
            Stage::Dragging { id } => {
                action.suppress_taps = true;
                if active.is_empty() {
                    action.release = true;
                    Stage::Idle
                } else if single.is_some_and(|c| c.id == id) {
                    Stage::Dragging { id }
                } else {
                    action.release = true;
                    Stage::Cancelled
                }
            }
            Stage::Cancelled => {
                action.suppress_taps = true;
                if active.is_empty() {
                    Stage::Idle
                } else {
                    Stage::Cancelled
                }
            }
            Stage::Idle => Stage::Idle,
        };
        action
    }
}

fn distance(a: (f64, f64), b: (f64, f64)) -> f64 {
    (a.0 - b.0).hypot(a.1 - b.1)
}
