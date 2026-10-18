/**
 * ratex.h — RaTeX C ABI public header
 *
 * Provides LaTeX-to-DisplayList rendering for iOS, Android, Flutter, and React Native.
 *
 * Usage:
 *   RatexColor black = {0, 0, 0, 1};
 *   RatexOptions opts = { sizeof(RatexOptions), 1, &black };
 *   RatexResult r = ratex_parse_and_layout("\\frac{1}{2}", &opts);
 *   if (r.error_code == 0) {
 *       // r.data is a heap-allocated UTF-8 JSON string
 *       ratex_free_display_list(r.data);
 *   } else {
 *       const char* err = ratex_get_last_error();
 *       fprintf(stderr, "RaTeX error: %s\n", err ? err : "(unknown)");
 *   }
 *
 * display_mode values:
 *   1 — display (block) style, equivalent to $$...$$
 *   0 — inline (text) style,   equivalent to $...$
 *
 * Thread safety:
 *   Both layout entry points and ratex_get_last_error use thread-local storage for
 *   error state, so they are safe to call concurrently from multiple threads.
 *   Each thread has its own last-error slot.
 *
 * DisplayList JSON format:
 *   {
 *     "version": 1,         // protocol version (optional; decoders may treat missing as 0)
 *     "width":  <number>,   // total width in em units
 *     "height": <number>,   // ascent above baseline in em units
 *     "depth":  <number>,   // descent below baseline in em units
 *     "items":  [           // array of drawing commands (see below)
 *       { "type": "GlyphPath", "x": <f64>, "y": <f64>, "scale": <f64>,
 *         "font": <string>, "char_code": <u32>, "color": {"r":<f32>,"g":<f32>,"b":<f32>,"a":<f32>} },
 *       { "type": "Line", "x": <f64>, "y": <f64>, "width": <f64>, "thickness": <f64>,
 *         "color": {...}, "dashed": <bool?> },
 *       { "type": "Rect", "x": <f64>, "y": <f64>, "width": <f64>, "height": <f64>,
 *         "color": {...} },
 *       { "type": "Path", "x": <f64>, "y": <f64>, "commands": [<PathCommand>, ...],
 *         "fill": <bool>, "color": {...} }
 *     ]
 *   }
 *
 * PathCommand variants:
 *   { "type": "MoveTo",  "x":<f64>,"y":<f64> }
 *   { "type": "LineTo",  "x":<f64>,"y":<f64> }
 *   { "type": "CubicTo", "x1":<f64>,"y1":<f64>,"x2":<f64>,"y2":<f64>,"x":<f64>,"y":<f64> }
 *   { "type": "QuadTo",  "x1":<f64>,"y1":<f64>,"x":<f64>,"y":<f64> }
 *   { "type": "Close" }
 *
 * Protocol and compatibility:
 *   Treat this JSON as a public protocol. Decoders should ignore unknown fields
 *   and tolerate missing optional fields for forward/backward compatibility.
 *   See docs/DISPLAYLIST_JSON_PROTOCOL.md for the full schema and change policy.
 *
 * Coordinate system:
 *   All coordinates are in em units. Multiply by font_size (pt or px) to get
 *   screen coordinates. X increases rightward; Y increases downward. The baseline
 *   is at y = height (measured from the top of the bounding box).
 */

#ifndef RATEX_H
#define RATEX_H

#ifdef __cplusplus
extern "C" {
#endif

#include <stddef.h>
#include <stdint.h>

/**
 * Options for ratex_parse_and_layout().
 *
 * Always set struct_size = sizeof(RatexOptions) before use.
 * Fields beyond struct_size are ignored, allowing forward compatibility.
 */
typedef struct {
    float r; /* normalized 0..1 */
    float g; /* normalized 0..1 */
    float b; /* normalized 0..1 */
    float a; /* normalized 0..1 */
} RatexColor;

typedef struct {
    size_t struct_size;
    int display_mode; /* 0 = inline ($...$), 1 = display block ($$...$$) */
    const RatexColor* color; /* NULL = default black */
} RatexOptions;

/**
 * Result returned by ratex_parse_and_layout().
 *
 * On success: error_code == 0 and data is a heap-allocated JSON string;
 * free it with ratex_free_display_list().
 * On error: error_code != 0, data is NULL; call ratex_get_last_error() for details.
 */
typedef struct {
    char* data;      /* JSON display list on success, NULL on error */
    int error_code;  /* 0 on success, non-zero on error */
} RatexResult;

/**
 * Parse a LaTeX string and compute its display list.
 *
 * @param latex  Null-terminated UTF-8 LaTeX string. Must not be NULL.
 * @param opts   Pointer to RatexOptions with struct_size set. May be NULL (defaults to display mode).
 * @return       RatexResult. On success error_code == 0 and data is a JSON string
 *               (free with ratex_free_display_list). On error error_code != 0 and data is NULL.
 */
RatexResult ratex_parse_and_layout(const char* latex, const RatexOptions* opts);

/** Metrics of one Unicode scalar in em at the base font size. */
typedef struct {
    uint32_t char_code;
    double width;
    double height;
    double depth;
} RatexFallbackGlyphMetrics;

/**
 * Parse and lay out using host metrics for glyphs missing from the KaTeX fonts.
 *
 * Measure in base-size em with the font used for drawing; layout applies scaling.
 * All entries require valid Unicode scalars and finite values (signed values are
 * allowed). The last duplicate wins.
 * Known KaTeX glyphs and unlisted characters keep their normal metrics.
 *
 * @param latex          Same as ratex_parse_and_layout().
 * @param opts           Same as ratex_parse_and_layout().
 * @param metrics        Aligned, readable array of initialized entries, unchanged
 *                       until return. Copied during the call; no pointers retained.
 * @param metrics_count  Number of entries; array byte size must not exceed PTRDIFF_MAX.
 *                       Zero ignores metrics (NULL allowed) and matches the original API.
 * @return               Same result and error contract as ratex_parse_and_layout();
 *                       free successful results with ratex_free_display_list().
 */
RatexResult ratex_parse_and_layout_with_fallback_metrics(
    const char* latex,
    const RatexOptions* opts,
    const RatexFallbackGlyphMetrics* metrics,
    size_t metrics_count);

/**
 * Free a JSON string returned by either layout entry point.
 *
 * @param json  Pointer to free. Passing NULL is a no-op.
 */
void ratex_free_display_list(char* json);

/**
 * Return the last error message produced by either layout entry point on this thread.
 *
 * @return  A null-terminated UTF-8 error string, or NULL if no error has occurred.
 *          The pointer is valid until the next call to either layout entry point on
 *          this thread. Do NOT free this pointer.
 */
const char* ratex_get_last_error(void);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* RATEX_H */
