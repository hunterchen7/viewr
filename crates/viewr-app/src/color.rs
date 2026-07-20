//! macOS color management: pin the wgpu CAMetalLayer to the sRGB
//! colorspace so our sRGB output isn't stretched onto wide-gamut (P3)
//! displays as oversaturated color. Without an explicit colorspace,
//! CoreAnimation treats the layer's pixels as being in the display's
//! native space.
//!
//! wgpu attaches its CAMetalLayer as a sublayer of the NSView's backing
//! layer, so we search the (shallow) layer tree for it. Every step is
//! defensive: on any surprise this is a no-op, never a crash.

#[cfg(target_os = "macos")]
pub fn pin_srgb_colorspace(cc: &eframe::CreationContext<'_>) {
    use objc2::encode::{Encoding, RefEncode};
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::ffi::c_void;

    /// Opaque CGColorSpace with the ObjC type encoding `^{CGColorSpace=}`
    /// that -[CAMetalLayer setColorspace:] expects.
    #[repr(C)]
    struct CGColorSpace {
        _opaque: [u8; 0],
    }
    unsafe impl RefEncode for CGColorSpace {
        const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("CGColorSpace", &[]));
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGColorSpaceCreateWithName(name: *const c_void) -> *mut CGColorSpace;
        fn CGColorSpaceRelease(space: *mut CGColorSpace);
        static kCGColorSpaceSRGB: *const c_void;
    }

    /// Depth-limited search for a CAMetalLayer in the layer tree.
    unsafe fn find_metal_layer(
        layer: *mut AnyObject,
        metal_class: &AnyClass,
        depth: u8,
    ) -> Option<*mut AnyObject> {
        if layer.is_null() {
            return None;
        }
        unsafe {
            let is_metal: bool = msg_send![&*layer, isKindOfClass: metal_class];
            if is_metal {
                return Some(layer);
            }
            if depth == 0 {
                return None;
            }
            let sublayers: *mut AnyObject = msg_send![&*layer, sublayers];
            if sublayers.is_null() {
                return None;
            }
            let count: usize = msg_send![&*sublayers, count];
            for i in 0..count {
                let child: *mut AnyObject = msg_send![&*sublayers, objectAtIndex: i];
                if let Some(found) = find_metal_layer(child, metal_class, depth - 1) {
                    return Some(found);
                }
            }
        }
        None
    }

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Ok(handle) = cc.window_handle() else {
            return;
        };
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            return;
        };
        let Some(metal_class) = AnyClass::get(c"CAMetalLayer") else {
            return;
        };
        unsafe {
            let view = appkit.ns_view.as_ptr() as *mut AnyObject;
            let root: *mut AnyObject = msg_send![&*view, layer];
            let Some(metal) = find_metal_layer(root, metal_class, 3) else {
                eprintln!("color: no CAMetalLayer found; leaving colorspace unmanaged");
                return;
            };
            let srgb = CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
            if srgb.is_null() {
                return;
            }
            let _: () = msg_send![&*metal, setColorspace: srgb];
            CGColorSpaceRelease(srgb);
        }
    }));
    if result.is_err() {
        eprintln!("color: colorspace pin failed; continuing unmanaged");
    }
}

#[cfg(not(target_os = "macos"))]
pub fn pin_srgb_colorspace(_cc: &eframe::CreationContext<'_>) {}
