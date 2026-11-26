//! OS backdrop blur behind the egui surface. macOS vibrancy only.
//! Windows and Linux stay opaque: DWM Mica/acrylic is not live frost.

#[cfg(target_os = "macos")]
use eframe::egui::Color32;
use raw_window_handle::HasWindowHandle;

#[cfg(target_os = "macos")]
pub fn tint(color: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
}

/// Install the blur behind the window. True once it is in place (always
/// true off macOS, where there is nothing to install); false when the window
/// is not ready yet, so the caller can try again.
pub fn apply(window: &impl HasWindowHandle) -> bool {
    #[cfg(target_os = "macos")]
    {
        macos::apply(window)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = window;
        true
    }
}

#[cfg(target_os = "macos")]
pub fn clear_color() -> [f32; 4] {
    [0.0, 0.0, 0.0, 0.0]
}

#[cfg(target_os = "macos")]
mod macos {
    use objc2_app_kit::{
        NSAutoresizingMaskOptions, NSColor, NSView, NSVisualEffectBlendingMode,
        NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView, NSWindowOrderingMode,
    };
    use objc2_foundation::MainThreadMarker;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    fn strip_effect_views(host: &NSView) {
        let subs = host.subviews();
        let mut doomed = Vec::new();
        for index in 0..subs.count() {
            let sub = subs.objectAtIndex(index);
            if sub.downcast_ref::<NSVisualEffectView>().is_some() {
                doomed.push(sub);
            }
        }
        for sub in doomed {
            sub.removeFromSuperview();
        }
    }

    fn same_view(a: &NSView, b: &NSView) -> bool {
        std::ptr::eq(a as *const NSView, b as *const NSView)
    }

    pub fn apply(window: &impl HasWindowHandle) -> bool {
        let Ok(handle) = window.window_handle() else {
            return false;
        };
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            return false;
        };
        let Some(mtm) = MainThreadMarker::new() else {
            return false;
        };

        // SAFETY: eframe's AppKit handle is the live winit NSView for this window.
        let gl_view: &NSView = unsafe { appkit.ns_view.cast().as_ref() };

        let Some(ns_window) = gl_view.window() else {
            return false;
        };
        ns_window.setOpaque(false);
        ns_window.setBackgroundColor(Some(&NSColor::clearColor()));

        gl_view.setWantsLayer(true);
        if let Some(layer) = gl_view.layer() {
            layer.setOpaque(false);
        }

        // SAFETY: the view is in a live window, so its superview pointer is valid.
        let parent = match ns_window.contentView() {
            Some(content) if !same_view(&content, gl_view) => content,
            _ => match unsafe { gl_view.superview() } {
                Some(super_view) => super_view,
                None => return false,
            },
        };

        strip_effect_views(gl_view);
        strip_effect_views(&parent);

        let effect = NSVisualEffectView::initWithFrame(mtm.alloc(), gl_view.frame());
        effect.setMaterial(NSVisualEffectMaterial::HUDWindow);
        effect.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
        effect.setState(NSVisualEffectState::Active);
        effect.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        parent.addSubview_positioned_relativeTo(
            &effect,
            NSWindowOrderingMode::Below,
            Some(gl_view),
        );
        true
    }
}
