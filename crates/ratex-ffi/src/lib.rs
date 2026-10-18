//! RaTeX C ABI FFI exports for native platform integration.
//!
//! Platform-specific modules:
//! - `jni` — Android JNI bridge (compiled only on `target_os = "android"`)
//!
//! ## DisplayList JSON protocol
//!
//! The primary output of this crate is a UTF-8 JSON string representing a `DisplayList`.
//! Treat this JSON as a **public protocol**: decoders should ignore unknown fields and
//! tolerate missing optional fields for forward/backward compatibility.
//!
//! See `docs/DISPLAYLIST_JSON_PROTOCOL.md` in the repository for the full schema and
//! change policy.
//!
//! # Usage (C)
//! ```c
//! RatexColor black = {0, 0, 0, 1};
//! RatexOptions opts = { sizeof(RatexOptions), 1, &black };  // display_mode=1 (block)
//! RatexResult result = ratex_parse_and_layout("\\frac{1}{2}", &opts);
//! if (result.error_code == 0) {
//!     // consume result.data ...
//!     ratex_free_display_list(result.data);
//! } else {
//!     const char* err = ratex_get_last_error();
//!     // handle error...
//! }
//! ```

#[cfg(target_os = "android")]
pub mod jni;

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::sync::Arc;

use ratex_layout::{
    layout, layout_with_fallback_metrics, to_display_list, validate_layout, FallbackGlyphMetrics,
    LayoutOptions,
};
use ratex_parser::parse;
use ratex_types::math_style::MathStyle;
use serde_json::Value;

// Thread-local storage for the last error message.
thread_local! {
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

fn set_last_error(msg: &str) {
    let bytes: Vec<u8> = msg.bytes().filter(|&b| b != 0).collect();
    let stored = CString::new(bytes).unwrap_or_else(|_| {
        CString::new("(error message could not be encoded)").expect("static C string")
    });
    LAST_ERROR.with(|cell| {
        *cell.borrow_mut() = Some(stored);
    });
}

fn clear_last_error() {
    LAST_ERROR.with(|cell| {
        *cell.borrow_mut() = None;
    });
}

/// Replace non-finite floats with 0 to produce valid JSON.
fn sanitize_json_numbers(v: Value) -> Value {
    match v {
        Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                if f.is_finite() {
                    Value::Number(n)
                } else {
                    Value::Number(serde_json::Number::from_f64(0.0).unwrap())
                }
            } else {
                Value::Number(n)
            }
        }
        Value::Array(arr) => Value::Array(arr.into_iter().map(sanitize_json_numbers).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, sanitize_json_numbers(v)))
                .collect(),
        ),
        other => other,
    }
}

fn do_layout(
    latex_str: &str,
    style: MathStyle,
    color: ratex_types::color::Color,
    metrics: Option<Arc<HashMap<char, FallbackGlyphMetrics>>>,
) -> Result<String, String> {
    let nodes = parse(latex_str).map_err(|e| format!("parse error: {e}"))?;
    let options = LayoutOptions::default().with_style(style).with_color(color);
    let layout_box = match metrics {
        Some(metrics) => layout_with_fallback_metrics(&nodes, &options, metrics),
        None => layout(&nodes, &options),
    };
    validate_layout(&layout_box)?;
    let display_list = to_display_list(&layout_box);
    let value =
        serde_json::to_value(&display_list).map_err(|e| format!("serialization error: {e}"))?;
    let mut sanitized = sanitize_json_numbers(value);
    // Add a protocol version at the top level for forward-compatible decoding.
    if let Value::Object(ref mut map) = sanitized {
        map.insert("version".to_string(), Value::Number(1.into()));
    }
    serde_json::to_string(&sanitized).map_err(|e| format!("JSON stringify error: {e}"))
}

// ---------------------------------------------------------------------------
// Public structs
// ---------------------------------------------------------------------------

/// Options for [`ratex_parse_and_layout`].
///
/// Always set `struct_size = sizeof(RatexOptions)` before passing to the function.
/// Fields beyond `struct_size` are ignored, enabling forward compatibility.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RatexColor {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl RatexColor {
    pub const BLACK: Self = Self {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 1.0,
    };
}

impl From<RatexColor> for ratex_types::color::Color {
    fn from(value: RatexColor) -> Self {
        Self::new(value.r, value.g, value.b, value.a)
    }
}

fn validate_color(color: RatexColor) -> Result<ratex_types::color::Color, String> {
    fn validate_component(name: &str, value: f32) -> Result<(), String> {
        if !value.is_finite() {
            return Err(format!(
                "invalid color.{name}: expected a finite float in [0, 1], got {value}"
            ));
        }
        if !(0.0..=1.0).contains(&value) {
            return Err(format!(
                "invalid color.{name}: expected a float in [0, 1], got {value}"
            ));
        }
        Ok(())
    }

    validate_component("r", color.r)?;
    validate_component("g", color.g)?;
    validate_component("b", color.b)?;
    validate_component("a", color.a)?;

    Ok(color.into())
}

#[repr(C)]
pub struct RatexOptions {
    /// Must be set to `sizeof(RatexOptions)` by the caller.
    pub struct_size: usize,
    /// Rendering mode:
    /// - `0` — inline (text style, equivalent to `$...$`)
    /// - `1` — display block (display style, equivalent to `$$...$$`)
    pub display_mode: c_int,
    /// Default formula color, in normalized RGBA.
    ///
    /// Explicit LaTeX color commands like `\color{...}` / `\textcolor{...}{...}`
    /// still override this per subtree.
    pub color: *const RatexColor,
}

/// Host-measured metrics for one Unicode scalar, in em at the base font size.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RatexFallbackGlyphMetrics {
    pub char_code: u32,
    pub width: f64,
    pub height: f64,
    pub depth: f64,
}

unsafe fn read_fallback_metrics(
    metrics: *const RatexFallbackGlyphMetrics,
    count: usize,
) -> Result<Option<Arc<HashMap<char, FallbackGlyphMetrics>>>, String> {
    if count > isize::MAX as usize / std::mem::size_of::<RatexFallbackGlyphMetrics>() {
        return Err("fallback metrics array exceeds the addressable size".into());
    }
    if count == 0 {
        return Ok(None);
    }
    if metrics.is_null() || !metrics.is_aligned() {
        return Err("nonempty fallback metrics require a non-null, aligned pointer".into());
    }
    // The byte size fits in isize; the caller guarantees readable storage.
    let entries = unsafe { std::slice::from_raw_parts(metrics, count) };
    let mut map = HashMap::new();
    map.try_reserve(count)
        .map_err(|e| format!("cannot allocate fallback metrics: {e}"))?;
    for entry in entries {
        let ch = char::from_u32(entry.char_code)
            .ok_or_else(|| format!("invalid fallback Unicode scalar: U+{:X}", entry.char_code))?;
        if !entry.width.is_finite() || !entry.height.is_finite() || !entry.depth.is_finite() {
            return Err(format!(
                "non-finite fallback metrics for U+{:X}",
                entry.char_code
            ));
        }
        map.insert(
            ch,
            FallbackGlyphMetrics {
                width: entry.width,
                height: entry.height,
                depth: entry.depth,
            },
        );
    }
    Ok(Some(Arc::new(map)))
}

/// Result returned by [`ratex_parse_and_layout`].
///
/// On success: `error_code == 0` and `data` is a heap-allocated JSON string;
/// free it with [`ratex_free_display_list`].
/// On error: `error_code != 0`, `data` is NULL; call [`ratex_get_last_error`] for details.
#[repr(C)]
pub struct RatexResult {
    /// JSON display list on success, NULL on error.
    pub data: *mut c_char,
    /// `0` on success, non-zero on error.
    pub error_code: c_int,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Parse a LaTeX string and compute its display list with explicit rendering options.
///
/// Pass `opts = NULL` to use display-mode defaults.
///
/// # Safety
/// - `latex` must be a valid non-null null-terminated UTF-8 C string.
/// - `opts` may be NULL. If non-null it must point to a valid `RatexOptions` whose
///   `struct_size` field is set correctly.
#[no_mangle]
pub unsafe extern "C" fn ratex_parse_and_layout(
    latex: *const c_char,
    opts: *const RatexOptions,
) -> RatexResult {
    unsafe { parse_and_layout(latex, opts, std::ptr::null(), 0) }
}

/// Parse and lay out using host metrics for glyphs missing from the KaTeX fonts.
///
/// Measure in base-size em with the font used for drawing; layout applies scaling.
/// All entries require valid scalars and finite values; the last duplicate wins.
/// Entries are copied; no host pointers are retained.
/// Empty tables ignore `metrics` and behave like [`ratex_parse_and_layout`].
///
/// # Safety
/// - `latex` and `opts` follow [`ratex_parse_and_layout`]'s requirements.
/// - For nonzero `metrics_count`, `metrics` must point to that many initialized,
///   aligned, readable entries in one allocation, unchanged until return.
#[no_mangle]
pub unsafe extern "C" fn ratex_parse_and_layout_with_fallback_metrics(
    latex: *const c_char,
    opts: *const RatexOptions,
    metrics: *const RatexFallbackGlyphMetrics,
    metrics_count: usize,
) -> RatexResult {
    unsafe { parse_and_layout(latex, opts, metrics, metrics_count) }
}

unsafe fn parse_and_layout(
    latex: *const c_char,
    opts: *const RatexOptions,
    metrics: *const RatexFallbackGlyphMetrics,
    metrics_count: usize,
) -> RatexResult {
    let err_result = |msg: &str| -> RatexResult {
        set_last_error(msg);
        RatexResult {
            data: std::ptr::null_mut(),
            error_code: 1,
        }
    };

    clear_last_error();

    if latex.is_null() {
        return err_result("ratex_parse_and_layout: latex pointer is null");
    }

    let latex_str = match unsafe { CStr::from_ptr(latex) }.to_str() {
        Ok(s) => s,
        Err(e) => return err_result(&format!("invalid UTF-8 in latex string: {e}")),
    };

    // Read only fields covered by struct_size; legacy callers may allocate
    // less than a complete RatexOptions, so do not form a reference to it.
    let opts_size = if opts.is_null() {
        0
    } else {
        unsafe { std::ptr::addr_of!((*opts).struct_size).read() }
    };
    let style = if opts.is_null() {
        MathStyle::Display
    } else {
        let min_size =
            std::mem::offset_of!(RatexOptions, display_mode) + std::mem::size_of::<c_int>();
        if opts_size >= min_size && unsafe { std::ptr::addr_of!((*opts).display_mode).read() } == 0
        {
            MathStyle::Text
        } else {
            MathStyle::Display
        }
    };

    let color = if opts.is_null() {
        ratex_types::color::Color::BLACK
    } else {
        let color_size =
            std::mem::offset_of!(RatexOptions, color) + std::mem::size_of::<*const RatexColor>();

        let color_ptr = if opts_size >= color_size {
            unsafe { std::ptr::addr_of!((*opts).color).read() }
        } else {
            std::ptr::null()
        };
        if !color_ptr.is_null() {
            match validate_color(unsafe { *color_ptr }) {
                Ok(color) => color,
                Err(msg) => return err_result(&msg),
            }
        } else {
            ratex_types::color::Color::BLACK
        }
    };

    let metrics = match unsafe { read_fallback_metrics(metrics, metrics_count) } {
        Ok(metrics) => metrics,
        Err(message) => return err_result(&message),
    };
    match do_layout(latex_str, style, color, metrics) {
        Ok(json) => match CString::new(json) {
            Ok(cs) => RatexResult {
                data: cs.into_raw(),
                error_code: 0,
            },
            Err(e) => err_result(&format!("JSON contains interior null byte: {e}")),
        },
        Err(e) => err_result(&e),
    }
}

/// Free a display list JSON string returned by either parse-and-layout entry point.
///
/// Passing NULL is a no-op.
///
/// # Safety
/// `ptr` must have been returned by a parse-and-layout entry point and must not be freed twice.
#[no_mangle]
pub unsafe extern "C" fn ratex_free_display_list(ptr: *mut c_char) {
    if !ptr.is_null() {
        unsafe { drop(CString::from_raw(ptr)) };
    }
}

/// Return the last error message set by any layout function on this thread.
///
/// # Returns
/// - A pointer to a null-terminated error string, valid until the next layout call on this thread.
/// - NULL if no error has occurred on this thread.
///
/// # Safety
/// The returned pointer is only valid for the lifetime of the current thread and until the
/// next call to a layout function on this thread.
#[no_mangle]
pub extern "C" fn ratex_get_last_error() -> *const c_char {
    LAST_ERROR.with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|cs| cs.as_ptr())
            .unwrap_or(std::ptr::null())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::ffi::CString;

    #[test]
    fn dashed_separator_expansion_returns_a_resource_error() {
        let input = CString::new(format!(
            r"{}\begin{{array}}{{:c:}}\rule{{1em}}{{10em}}\end{{array}}{}",
            r"\pmb{".repeat(13),
            "}".repeat(13)
        ))
        .unwrap();
        let result = unsafe { ratex_parse_and_layout(input.as_ptr(), std::ptr::null()) };
        assert_eq!(result.error_code, 1);
        assert!(result.data.is_null());
        let error = unsafe { CStr::from_ptr(ratex_get_last_error()) };
        assert!(error
            .to_str()
            .unwrap()
            .contains("Layout resource limit exceeded"));
        assert!(call("x+1", 1).is_some());
    }

    /// Assert the default formula color applied to the first `GlyphPath` in the protocol JSON is black.
    ///
    /// We key off `type == "GlyphPath"` (see `docs/DISPLAYLIST_JSON_PROTOCOL.md`) instead of “first
    /// item with any `color`”, so fraction bars or paths cannot satisfy the assertion by accident.
    fn assert_default_glyph_path_color_is_black(json: &str) {
        let v: Value = serde_json::from_str(json).expect("valid display list JSON");
        let items = v
            .get("items")
            .and_then(|i| i.as_array())
            .expect("display list must have items array");
        let glyph = items
            .iter()
            .find(|item| {
                item.get("type")
                    .and_then(|t| t.as_str())
                    .is_some_and(|ty| ty == "GlyphPath")
            })
            .expect("expected at least one GlyphPath item");
        let color = glyph
            .get("color")
            .expect("GlyphPath must include color per DISPLAYLIST_JSON_PROTOCOL");
        let r = color.get("r").and_then(|x| x.as_f64());
        let g = color.get("g").and_then(|x| x.as_f64());
        let b = color.get("b").and_then(|x| x.as_f64());
        let a = color.get("a").and_then(|x| x.as_f64());
        assert_eq!((r, g, b, a), (Some(0.0), Some(0.0), Some(0.0), Some(1.0)));
    }

    fn call(latex: &str, display_mode: c_int) -> Option<String> {
        let input = CString::new(latex).unwrap();
        let black = RatexColor::BLACK;
        let opts = RatexOptions {
            struct_size: std::mem::size_of::<RatexOptions>(),
            display_mode,
            color: &black,
        };
        let result = unsafe { ratex_parse_and_layout(input.as_ptr(), &opts) };
        if result.error_code != 0 || result.data.is_null() {
            return None;
        }
        let json = unsafe { CStr::from_ptr(result.data) }
            .to_str()
            .unwrap()
            .to_owned();
        unsafe { ratex_free_display_list(result.data) };
        Some(json)
    }

    #[test]
    fn display_fraction() {
        let json = call(r"\frac{1}{2}", 1).expect("should not fail");
        assert!(json.starts_with('{'));
        assert!(json.contains("items"));
    }

    #[test]
    fn inline_fraction() {
        let json = call(r"\frac{1}{2}", 0).expect("should not fail");
        assert!(json.contains("items"));
    }

    #[test]
    fn display_expression() {
        let json = call("x^2 + y^2 = z^2", 1).expect("should not fail");
        assert!(json.contains("items"));
    }

    #[test]
    fn null_latex_returns_error() {
        let black = RatexColor::BLACK;
        let opts = RatexOptions {
            struct_size: std::mem::size_of::<RatexOptions>(),
            display_mode: 1,
            color: &black,
        };
        let result = unsafe { ratex_parse_and_layout(std::ptr::null(), &opts) };
        assert_ne!(result.error_code, 0);
        assert!(result.data.is_null());
        let err = ratex_get_last_error();
        assert!(!err.is_null());
        let msg = unsafe { CStr::from_ptr(err) }.to_str().unwrap();
        assert!(msg.contains("null"));
    }

    #[test]
    fn null_opts_defaults_to_display() {
        let input = CString::new(r"x^2").unwrap();
        let result = unsafe { ratex_parse_and_layout(input.as_ptr(), std::ptr::null()) };
        assert_eq!(result.error_code, 0);
        assert!(!result.data.is_null());
        unsafe { ratex_free_display_list(result.data) };
    }

    #[test]
    fn free_null_is_noop() {
        unsafe { ratex_free_display_list(std::ptr::null_mut()) };
    }

    #[test]
    fn error_on_bad_latex() {
        let result = call(r"\undefined{x}", 1);
        if result.is_none() {
            let err = ratex_get_last_error();
            assert!(!err.is_null());
        }
    }

    #[test]
    fn custom_color_applies_without_overriding_explicit_latex_color() {
        let input = CString::new(r"x + \color{red}{y}").unwrap();
        let blue = RatexColor {
            r: 0.0,
            g: 0.0,
            b: 1.0,
            a: 1.0,
        };
        let opts = RatexOptions {
            struct_size: std::mem::size_of::<RatexOptions>(),
            display_mode: 1,
            color: &blue,
        };
        let result = unsafe { ratex_parse_and_layout(input.as_ptr(), &opts) };
        assert_eq!(result.error_code, 0);
        let json = unsafe { CStr::from_ptr(result.data) }
            .to_str()
            .unwrap()
            .to_owned();
        unsafe { ratex_free_display_list(result.data) };

        assert!(json.contains("\"b\":1.0"));
        assert!(json.contains("\"r\":1.0"));
    }

    #[test]
    fn implicit_geometry_inherits_formula_color() {
        let input = CString::new(r"\boxed{x}\quad\rule{1em}{0.1em}\quad\imageof").unwrap();
        let formula_color = RatexColor {
            r: 0.25,
            g: 0.5,
            b: 0.75,
            a: 1.0,
        };
        let opts = RatexOptions {
            struct_size: std::mem::size_of::<RatexOptions>(),
            display_mode: 1,
            color: &formula_color,
        };
        let result = unsafe { ratex_parse_and_layout(input.as_ptr(), &opts) };
        assert_eq!(result.error_code, 0);
        let json = unsafe { CStr::from_ptr(result.data) }
            .to_str()
            .unwrap()
            .to_owned();
        unsafe { ratex_free_display_list(result.data) };

        let value: Value = serde_json::from_str(&json).expect("valid display list JSON");
        let items = value["items"]
            .as_array()
            .expect("display list must have items array");
        let expected = (Some(0.25), Some(0.5), Some(0.75), Some(1.0));

        for item_type in ["GlyphPath", "Line", "Rect", "Path"] {
            let matching: Vec<_> = items
                .iter()
                .filter(|item| item["type"].as_str() == Some(item_type))
                .collect();
            assert!(
                !matching.is_empty(),
                "expected at least one {item_type} item"
            );

            for item in matching {
                let color = &item["color"];
                let actual = (
                    color["r"].as_f64(),
                    color["g"].as_f64(),
                    color["b"].as_f64(),
                    color["a"].as_f64(),
                );
                assert_eq!(
                    actual, expected,
                    "{item_type} should inherit the formula color"
                );
            }
        }
    }

    #[repr(C)]
    struct LegacyRatexOptions {
        struct_size: usize,
        display_mode: c_int,
    }

    #[test]
    fn short_legacy_options_remain_binary_compatible() {
        let input = CString::new("x").unwrap();
        let legacy_opts = LegacyRatexOptions {
            struct_size: std::mem::size_of::<LegacyRatexOptions>(),
            display_mode: 1,
        };

        let result = unsafe {
            ratex_parse_and_layout(
                input.as_ptr(),
                &legacy_opts as *const LegacyRatexOptions as *const RatexOptions,
            )
        };
        assert_eq!(result.error_code, 0);
        assert!(!result.data.is_null());

        let json = unsafe { CStr::from_ptr(result.data) }
            .to_str()
            .unwrap()
            .to_owned();
        unsafe { ratex_free_display_list(result.data) };

        // Old callers do not provide the color tail, so layout must fall back to black.
        assert_default_glyph_path_color_is_black(&json);
    }

    #[test]
    fn invalid_color_returns_error() {
        let input = CString::new("x").unwrap();
        let invalid = RatexColor {
            r: f32::NAN,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        };
        let opts = RatexOptions {
            struct_size: std::mem::size_of::<RatexOptions>(),
            display_mode: 1,
            color: &invalid,
        };

        let result = unsafe { ratex_parse_and_layout(input.as_ptr(), &opts) };
        assert_ne!(result.error_code, 0);
        assert!(result.data.is_null());

        let err = ratex_get_last_error();
        assert!(!err.is_null());
        let msg = unsafe { CStr::from_ptr(err) }.to_str().unwrap();
        assert!(msg.contains("invalid color.r"));
    }

    #[test]
    fn null_color_pointer_defaults_to_black() {
        let input = CString::new("x").unwrap();
        let opts = RatexOptions {
            struct_size: std::mem::size_of::<RatexOptions>(),
            display_mode: 1,
            color: std::ptr::null(),
        };

        let result = unsafe { ratex_parse_and_layout(input.as_ptr(), &opts) };
        assert_eq!(result.error_code, 0);
        assert!(!result.data.is_null());

        let json = unsafe { CStr::from_ptr(result.data) }
            .to_str()
            .unwrap()
            .to_owned();
        unsafe { ratex_free_display_list(result.data) };

        assert_default_glyph_path_color_is_black(&json);
    }

    #[test]
    fn empty_fallback_metrics_match_original_layout() {
        let source = r"\frac{\text{中😀}}{x}";
        let input = CString::new(source).unwrap();
        let result = unsafe {
            ratex_parse_and_layout_with_fallback_metrics(
                input.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
            )
        };
        assert_eq!(result.error_code, 0);
        assert!(!result.data.is_null());
        let json = unsafe { CStr::from_ptr(result.data) }
            .to_str()
            .unwrap()
            .to_owned();
        unsafe { ratex_free_display_list(result.data) };
        assert_eq!(json, call(source, 1).unwrap());
    }

    #[test]
    fn large_fallback_metrics_apply_to_layout() {
        let input = CString::new(r"\text{中}").unwrap();
        let mut metrics = vec![
            RatexFallbackGlyphMetrics {
                char_code: '中' as u32,
                width: 1.0,
                height: 0.8,
                depth: 0.2,
            };
            65_537
        ];
        metrics.last_mut().unwrap().width = 1.2;
        let result = unsafe {
            ratex_parse_and_layout_with_fallback_metrics(
                input.as_ptr(),
                std::ptr::null(),
                metrics.as_ptr(),
                metrics.len(),
            )
        };
        assert_eq!(result.error_code, 0);
        assert!(!result.data.is_null());
        let json = unsafe { CStr::from_ptr(result.data) }
            .to_str()
            .unwrap()
            .to_owned();
        unsafe { ratex_free_display_list(result.data) };
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["width"], 1.2);
        assert_eq!(value["height"], 0.8);
        assert_eq!(value["depth"], 0.2);
    }

    #[test]
    fn invalid_fallback_metrics_return_error() {
        let input = CString::new("x").unwrap();
        let metrics = RatexFallbackGlyphMetrics {
            char_code: '中' as u32,
            width: 1.2,
            height: 0.8,
            depth: 0.2,
        };
        let invalid_scalar = RatexFallbackGlyphMetrics {
            char_code: 0xD800,
            ..metrics
        };
        let invalid_width = RatexFallbackGlyphMetrics {
            width: f64::NAN,
            ..metrics
        };
        let oversized_count =
            isize::MAX as usize / std::mem::size_of::<RatexFallbackGlyphMetrics>() + 1;
        for (ptr, count) in [
            (std::ptr::null(), 1),
            (&metrics as *const _, oversized_count),
            (&invalid_scalar, 1),
            (&invalid_width, 1),
        ] {
            let result = unsafe {
                ratex_parse_and_layout_with_fallback_metrics(
                    input.as_ptr(),
                    std::ptr::null(),
                    ptr,
                    count,
                )
            };
            assert_eq!(result.error_code, 1);
            assert!(result.data.is_null());
            assert!(!ratex_get_last_error().is_null());
        }
    }
}
