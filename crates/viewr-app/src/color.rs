//! macOS color management: pin the wgpu CAMetalLayer to the sRGB
//! colorspace so our sRGB output isn't stretched onto wide-gamut (P3)
//! displays as oversaturated color. Without an explicit colorspace,
//! CoreAnimation treats the layer's pixels as being in the display's
//! native space.

#[cfg(target_os = "macos")]
pub fn pin_srgb_colorspace(cc: &eframe::CreationContext<'_>) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::ffi::c_void;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGColorSpaceCreateWithName(name: *const c_void) -> *mut c_void;
        fn CGColorSpaceRelease(space: *mut c_void);
        static kCGColorSpaceSRGB: *const c_void;
    }

    let Ok(handle) = cc.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return;
    };
    unsafe {
        let view = appkit.ns_view.as_ptr() as *mut AnyObject;
        let layer: *mut AnyObject = msg_send![&*view, layer];
        if layer.is_null() {
            return;
        }
        let srgb = CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
        if srgb.is_null() {
            return;
        }
        let _: () = msg_send![&*layer, setColorspace: srgb];
        CGColorSpaceRelease(srgb);
    }
}

#[cfg(not(target_os = "macos"))]
pub fn pin_srgb_colorspace(_cc: &eframe::CreationContext<'_>) {}
