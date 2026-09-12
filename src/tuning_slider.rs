//! Slider tracking is an AppKit event lifetime, not global mouse-button state.

use std::cell::Cell;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSEvent, NSSlider};
use objc2_foundation::NSRect;

#[derive(Default)]
pub(crate) struct Tracking {
    active: Cell<bool>,
}

define_class!(
    #[unsafe(super(NSSlider))]
    #[thread_kind = MainThreadOnly]
    #[name = "TrackpadCompanionTuningSlider"]
    #[ivars = Tracking]
    pub(crate) struct TuningSlider;

    impl TuningSlider {
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            // NSSlider otherwise need not take focus when full keyboard
            // access is disabled. A pointer-selected slider should still
            // support precise arrow-key adjustments.
            if let Some(window) = self.window() {
                window.makeFirstResponder(Some(self));
            }
            self.ivars().active.set(true);
            self.notify();
            let _: () = unsafe { msg_send![super(self), mouseDown: event] };
            self.ivars().active.set(false);
            // Continuous actions update the readout during tracking;
            // this explicit action commits exactly after tracking ends.
            self.notify();
        }
    }
);

impl TuningSlider {
    pub(crate) fn control(
        mtm: MainThreadMarker,
        min: f64,
        max: f64,
        target: &AnyObject,
        action: Sel,
    ) -> Retained<NSSlider> {
        let this = Self::alloc(mtm).set_ivars(Tracking::default());
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        this.setMinValue(min);
        this.setMaxValue(max);
        this.setDoubleValue(min);
        this.setContinuous(true);
        unsafe {
            this.setTarget(Some(target));
            this.setAction(Some(action));
        }
        this.into_super()
    }

    fn notify(&self) {
        let target = self.target();
        unsafe {
            self.sendAction_to(self.action(), target.as_deref());
        }
    }
}

pub(crate) fn is_tracking(slider: &NSSlider) -> bool {
    slider
        .downcast_ref::<TuningSlider>()
        .is_some_and(|s| s.ivars().active.get())
}
