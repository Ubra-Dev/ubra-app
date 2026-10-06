//! Opt-in main-thread smoke for GPUI's explicit AppKit modal ownership.
//! Run: cargo test -p ubra-app --test owned_dialog_appkit -- --ignored

fn main() {
    if !std::env::args().any(|arg| arg == "--ignored" || arg == "--include-ignored") {
        println!("owned_dialog_appkit: ignored (requires macOS desktop)");
        return;
    }
    #[cfg(target_os = "macos")]
    native::run();
}

#[cfg(target_os = "macos")]
mod native {
    use gpui::{
        App, AppContext as _, Context, IntoElement, Render, Window, WindowBackgroundAppearance,
        WindowKind, WindowOptions, div, prelude::*,
    };
    use objc2::rc::Retained;
    use objc2_app_kit::{NSView, NSWindow};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::time::Duration;

    struct Surface;
    impl Render for Surface {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child("Native modal ownership fixture")
        }
    }

    fn native_window(window: &Window) -> Retained<NSWindow> {
        let RawWindowHandle::AppKit(handle) =
            HasWindowHandle::window_handle(window).unwrap().as_raw()
        else {
            panic!("expected AppKit window");
        };
        // SAFETY: GPUI supplies a live NSView for this callback on the main thread.
        unsafe { handle.ns_view.cast::<NSView>().as_ref().window().unwrap() }
    }

    pub fn run() {
        gpui_platform::application().run(|cx: &mut App| {
            let owner = cx.open_window(WindowOptions::default(), |_, cx| cx.new(|_| Surface)).unwrap();
            let other = cx.open_window(WindowOptions::default(), |_, cx| cx.new(|_| Surface)).unwrap();
            other.update(cx, |_, window, _| window.activate_window()).unwrap();
            let owner_native = owner.update(cx, |_, window, _| native_window(window)).unwrap();
            let other_native = other.update(cx, |_, window, _| native_window(window)).unwrap();
            let child = cx.open_window(WindowOptions {
                kind: WindowKind::OwnedDialog(owner.into()),
                window_background: WindowBackgroundAppearance::Blurred,
                ..Default::default()
            }, |_, cx| cx.new(|_| Surface)).unwrap();
            let child_native = child.update(cx, |_, window, _| native_window(window)).unwrap();
            cx.activate(true);
            cx.spawn(async move |cx| {
                cx.background_executor().timer(Duration::from_millis(350)).await;
                cx.update(|cx| {
                    assert_eq!(child_native.sheetParent().as_deref(), Some(&*owner_native), "dialog must belong to requested owner, not current main window");
                    assert!(other_native.attachedSheet().is_none());
                    assert!(child_native.isSheet(), "AppKit must provide native owner modality");
                    assert!(!child_native.isOpaque(), "blur window must remain non-opaque");
                    child.update(cx, |_, window, cx| {
                        assert_eq!(window.owned_dialog_parent(), Some(owner.into()));
                        window.remove_window();
                        let _ = cx;
                    }).unwrap();
                });
                cx.background_executor().timer(Duration::from_millis(350)).await;
                cx.update(|cx| {
                    assert!(owner_native.attachedSheet().is_none(), "closing child must release modal owner");
                    let reopened = cx.open_window(WindowOptions {
                        kind: WindowKind::OwnedDialog(owner.into()),
                        ..Default::default()
                    }, |_, cx| cx.new(|_| Surface)).unwrap();
                    owner.update(cx, |_, window, _| window.remove_window()).unwrap();
                    cx.defer(move |cx| {
                        assert!(!cx.windows().contains(&reopened.into()), "owner removal must remove owned dialog");
                        assert!(cx.open_window(WindowOptions {
                            kind: WindowKind::OwnedDialog(owner.into()),
                            ..Default::default()
                        }, |_, cx| cx.new(|_| Surface)).is_err(), "stale owner must fail closed");
                        other.update(cx, |_, window, _| window.remove_window()).unwrap();
                        println!("owned_dialog_appkit ... ok (explicit sheet owner, native modality, translucent window, close/reopen, owner cleanup, stale-owner rejection)");
                        cx.quit();
                    });
                });
            }).detach();
        });
    }
}
