//! AppKit owns close-sheet appearance, keyboard handling, and suppression UI.

use super::CloseResponse;
use block2::RcBlock;
use gpui::{App, ForegroundExecutor, Task, Window};
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSControlStateValueOn, NSModalResponse,
    NSModalResponseCancel, NSView, NSWindow, NSWindowWillCloseNotification,
};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSOperationQueue, NSString};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::cell::RefCell;
use std::ptr::NonNull;
use std::rc::Rc;
use tokio::sync::oneshot;

type ResponseSender = Rc<RefCell<Option<oneshot::Sender<CloseResponse>>>>;

fn respond(sender: &ResponseSender, response: CloseResponse) {
    let sender = sender.borrow_mut().take();
    if let Some(sender) = sender {
        let _ = sender.send(response);
    }
}

pub(super) fn close_prompt(
    window: &mut Window,
    title: &str,
    message: &str,
    cx: &mut App,
) -> Task<CloseResponse> {
    let Some(marker) = MainThreadMarker::new() else {
        return Task::ready(CloseResponse::Unavailable(
            "The native close alert requires the AppKit main thread.".into(),
        ));
    };
    let parent = HasWindowHandle::window_handle(window)
        .ok()
        .and_then(|handle| match handle.as_raw() {
            RawWindowHandle::AppKit(handle) => {
                // SAFETY: GPUI's view is live while this Window is borrowed,
                // and the main-thread marker was obtained before touching it.
                let view = unsafe { &*handle.ns_view.as_ptr().cast::<NSView>() };
                view.window()
            }
            _ => None,
        });
    let Some(parent) = parent else {
        return Task::ready(CloseResponse::Unavailable(
            "The native close alert could not find its AppKit window.".into(),
        ));
    };
    let alert = close_alert(marker, title, message);
    // AppKit can synchronously call GPUI when presenting or ending a sheet.
    // Defer presentation until the caller releases its App/Window borrows.
    let cancellation_executor = cx.foreground_executor().clone();
    cx.foreground_executor().spawn(async move {
        if !parent.isVisible() {
            return CloseResponse::Cancel;
        }
        let (mut sheet, mut response) = match CloseSheet::begin(parent, alert) {
            Ok(sheet) => sheet,
            Err(message) => return CloseResponse::Unavailable(message.into()),
        };
        sheet.cancellation_executor = Some(cancellation_executor);
        let result = (&mut response).await.unwrap_or(CloseResponse::Cancel);
        // The guard remains live across the await. Dropping this task cancels
        // only this exact sheet, never a subsequent prompt on the same window.
        drop(sheet);
        result
    })
}

fn close_alert(marker: MainThreadMarker, title: &str, message: &str) -> Retained<NSAlert> {
    let alert = NSAlert::new(marker);
    alert.setAlertStyle(NSAlertStyle::Warning);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(message));
    // Leave the icon unset: AppKit supplies the application's native icon.
    let close = alert.addButtonWithTitle(&NSString::from_str("Close"));
    close.setKeyEquivalent(&NSString::from_str("\r"));
    let cancel = alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    cancel.setKeyEquivalent(&NSString::from_str("\u{1b}"));
    alert.setShowsSuppressionButton(true);
    if let Some(suppression) = alert.suppressionButton() {
        suppression.setTitle(&NSString::from_str("Don't ask again"));
    }
    alert
}

struct CloseSheet {
    parent: Retained<NSWindow>,
    alert: Retained<NSAlert>,
    observer: Retained<ProtocolObject<dyn NSObjectProtocol>>,
    cancellation_executor: Option<ForegroundExecutor>,
}

impl CloseSheet {
    fn begin(
        parent: Retained<NSWindow>,
        alert: Retained<NSAlert>,
    ) -> Result<(Self, oneshot::Receiver<CloseResponse>), &'static str> {
        if parent.attachedSheet().is_some() {
            return Err("Another native sheet is already open in this window.");
        }
        let suppression = alert
            .suppressionButton()
            .ok_or("AppKit could not create the close alert's suppression checkbox.")?;
        let (sender, receiver) = oneshot::channel();
        let sender = Rc::new(RefCell::new(Some(sender)));
        let completion_sender = sender.clone();
        let completion = RcBlock::new(move |response: NSModalResponse| {
            let response = if response == NSAlertFirstButtonReturn {
                CloseResponse::Close {
                    suppress: suppression.state() == NSControlStateValueOn,
                }
            } else {
                CloseResponse::Cancel
            };
            respond(&completion_sender, response);
        });
        let closing_parent = parent.clone();
        let closing_sheet = alert.window();
        let on_close = RcBlock::new(move |_: NonNull<NSNotification>| {
            // Resolve cancellation before endSheet can invoke completion.
            respond(&sender, CloseResponse::Cancel);
            if closing_sheet.sheetParent().as_deref() == Some(&*closing_parent) {
                closing_parent.endSheet_returnCode(&closing_sheet, NSModalResponseCancel);
            }
        });
        // SAFETY: NSWindowWillClose is observed only on this AppKit window;
        // the main queue keeps the captured main-thread-only objects and Rc
        // confined to the AppKit thread. The token is removed by the guard.
        let observer = unsafe {
            NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
                Some(NSWindowWillCloseNotification),
                Some(&parent),
                Some(&NSOperationQueue::mainQueue()),
                &on_close,
            )
        };
        let sheet = Self {
            parent,
            alert,
            observer,
            cancellation_executor: None,
        };
        sheet
            .alert
            .beginSheetModalForWindow_completionHandler(&sheet.parent, Some(&completion));
        Ok((sheet, receiver))
    }
}

impl Drop for CloseSheet {
    fn drop(&mut self) {
        // SAFETY: this is the exact token returned by the notification center.
        let observer: &objc2::runtime::AnyObject = self.observer.as_ref();
        unsafe {
            NSNotificationCenter::defaultCenter().removeObserver(observer);
        }
        if let Some(executor) = &self.cancellation_executor {
            let alert = self.alert.clone();
            let parent = self.parent.clone();
            // A request can be replaced while GPUI is mutably borrowed.
            // Schedule dismissal so AppKit cannot re-enter that borrow.
            executor
                .spawn(async move {
                    let window = alert.window();
                    if window.sheetParent().as_deref() == Some(&*parent) {
                        parent.endSheet_returnCode(&window, NSModalResponseCancel);
                    }
                })
                .detach();
        } else {
            // The direct native fixture has no GPUI App to re-enter.
            let window = self.alert.window();
            if window.sheetParent().as_deref() == Some(&*self.parent) {
                self.parent
                    .endSheet_returnCode(&window, NSModalResponseCancel);
            }
        }
    }
}

/// Direct main-thread fixture used by the opt-in AppKit test harness; the
/// running application never calls it, so it is allowed to be uncalled here.
#[cfg(test)]
#[allow(dead_code)]
pub(super) fn native_close_smoke() {
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSEventMask,
        NSWindowStyleMask,
    };
    use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSRunLoop, NSSize};
    use std::time::{Duration, Instant};

    fn pump_until(mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let app = NSApplication::sharedApplication(MainThreadMarker::new().unwrap());
        while !done() {
            assert!(
                Instant::now() < deadline,
                "native close sheet fixture timed out"
            );
            while let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
                NSEventMask::Any,
                Some(&NSDate::distantPast()),
                unsafe { NSDefaultRunLoopMode },
                true,
            ) {
                app.sendEvent(&event);
            }
            app.updateWindows();
            NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.008));
        }
    }

    let marker = MainThreadMarker::new().expect("native close smoke must run on the main thread");
    let app = NSApplication::sharedApplication(marker);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    app.finishLaunching();
    let parent = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            marker.alloc(),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(640.0, 480.0)),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // Retained Rust ownership, rather than AppKit's historical close release.
    unsafe { parent.setReleasedWhenClosed(false) };
    parent.makeKeyAndOrderFront(None);

    for (button, suppress, expected) in [
        (1, false, CloseResponse::Cancel),
        (0, false, CloseResponse::Close { suppress: false }),
        (0, true, CloseResponse::Close { suppress: true }),
        (1, true, CloseResponse::Cancel),
    ] {
        let alert = close_alert(
            marker,
            "Close fixture session?",
            "The running session will stop.",
        );
        assert_eq!(alert.alertStyle(), NSAlertStyle::Warning);
        assert!(alert.showsSuppressionButton());
        let buttons = alert.buttons();
        assert_eq!(
            buttons.count(),
            2,
            "suppression must not be a third response button"
        );
        assert_eq!(buttons.objectAtIndex(0).keyEquivalent().to_string(), "\r");
        assert_eq!(
            buttons.objectAtIndex(1).keyEquivalent().to_string(),
            "\u{1b}"
        );
        let (sheet, mut response) = CloseSheet::begin(parent.clone(), alert).unwrap();
        let native_sheet = sheet.alert.window();
        pump_until(|| parent.attachedSheet().as_deref() == Some(&*native_sheet));
        assert!(
            native_sheet.isSheet(),
            "NSAlert must be a native attached sheet"
        );
        assert_eq!(native_sheet.sheetParent().as_deref(), Some(&*parent));
        let checkbox = sheet.alert.suppressionButton().unwrap();
        assert_eq!(checkbox.title().to_string(), "Don't ask again");
        checkbox.setState(if suppress { NSControlStateValueOn } else { 0 });
        // Exercise AppKit's actual response-button action and completion block.
        unsafe { buttons.objectAtIndex(button).performClick(None) };
        let mut result = None;
        pump_until(|| {
            if let Ok(value) = response.try_recv() {
                result = Some(value);
            }
            result.is_some()
        });
        assert_eq!(result, Some(expected));
        drop(sheet);
        pump_until(|| parent.attachedSheet().is_none());
    }

    // Dropping a pending task's guard must remove only its own native sheet.
    let (sheet, mut response) = CloseSheet::begin(
        parent.clone(),
        close_alert(marker, "Cancel pending request?", "Cancellation fixture."),
    )
    .unwrap();
    pump_until(|| parent.attachedSheet().is_some());
    drop(sheet);
    let mut result = None;
    pump_until(|| {
        if let Ok(value) = response.try_recv() {
            result = Some(value);
        }
        result.is_some() && parent.attachedSheet().is_none()
    });
    assert_eq!(result, Some(CloseResponse::Cancel));

    let (sheet, mut response) = CloseSheet::begin(
        parent.clone(),
        close_alert(marker, "Close parent window?", "Window lifetime fixture."),
    )
    .unwrap();
    pump_until(|| parent.attachedSheet().is_some());
    parent.close();
    let mut result = None;
    pump_until(|| {
        if let Ok(value) = response.try_recv() {
            result = Some(value);
        }
        result.is_some()
    });
    assert_eq!(result, Some(CloseResponse::Cancel));
    drop(sheet);
}
