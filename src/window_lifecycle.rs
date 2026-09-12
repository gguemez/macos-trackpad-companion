//! Close notifications settle pending writes immediately, even when the
//! window's normal polling timer has not fired yet.

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSWindow, NSWindowDelegate};
use objc2_foundation::NSNotification;

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "TrackpadCompanionWindowLifecycle"]
    #[ivars = fn()]
    pub(crate) struct CloseDelegate;

    unsafe impl NSObjectProtocol for CloseDelegate {}

    unsafe impl NSWindowDelegate for CloseDelegate {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, notification: &NSNotification) {
            (self.ivars())();
            let closing = notification.object().and_then(|o| o.downcast::<NSWindow>().ok());
            crate::app_kit::settle_activation_except(self.mtm(), closing.as_deref());
        }
    }
);

impl CloseDelegate {
    pub(crate) fn install(window: &NSWindow, on_close: fn()) -> Retained<Self> {
        let this = Self::alloc(MainThreadMarker::from(window)).set_ivars(on_close);
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };
        window.setDelegate(Some(ProtocolObject::from_ref(&*this)));
        this
    }
}
