//! SVG attachments, rasterized to PNG before they are stored.
//!
//! Providers read raster images only, and an SVG shown as is in a client could run script, so the
//! store never keeps one: it keeps the PNG this module renders. The renderer reads no file and no
//! URL (every `href` resolves to nothing), refuses a DTD (no entity expansion), stops past a node
//! budget, and draws text with the system fonts only.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use resvg::{tiny_skia, usvg};

use super::AttachmentError;

/// Largest SVG source accepted, in bytes. A diagram exported by a drawing tool is far below.
pub const MAX_SVG_BYTES: usize = 2 * 1024 * 1024;

/// Most XML nodes parsed: enough for a dense diagram, low enough that a crafted file cannot make
/// the renderer walk millions of elements.
const MAX_SVG_NODES: u32 = 200_000;

/// Longest side of the rendered PNG, in pixels. A small SVG is scaled up to be legible, a huge
/// one down, keeping its aspect ratio.
pub const RENDER_SIDE: u32 = 2048;

/// Below this longest side an SVG is scaled up to [`RENDER_SIDE`]: an icon-sized document would
/// otherwise reach the model as a few illegible pixels.
const MIN_RENDER_SIDE: f32 = 512.0;

/// Whether `bytes` looks like an SVG document. Only a routing hint; the parse is the check.
pub fn looks_like_svg(bytes: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]).to_lowercase();
    head.contains("<svg")
}

fn fonts() -> Arc<usvg::fontdb::Database> {
    static FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    FONTS
        .get_or_init(|| {
            let mut db = usvg::fontdb::Database::new();
            db.load_system_fonts();
            Arc::new(db)
        })
        .clone()
}

/// Most filter primitives (`<feGaussianBlur>` and the like): each keeps a full-size intermediate
/// image while the filter renders.
const MAX_FILTER_PRIMITIVES: usize = 16;

/// Most filter work a rendering may do, in full-canvas passes: each filter primitive costs the
/// share of the canvas its filter region covers.
/// Measured 2026-11-09 on a 2048 px canvas (resvg 0.49, opt-level 3): a full-canvas blur pass
/// takes ~0.27 s, so 32 passes is ~9 s; peak memory is ~435 MB for one 16-primitive filter and
/// ~13 MB per nested filtered group. Unbounded, a filtered element repeated through `<use>` ran
/// 0.3 s per copy with no limit, and the source-only primitive cap above never saw the copies.
const MAX_FILTER_PASSES: f64 = 32.0;

/// The least share of the canvas a primitive is charged: a tiny filter region still allocates
/// and runs, so thousands of tiny filtered copies must add up too.
const MIN_PASS_SHARE: f64 = 1.0 / 256.0;

/// The most a primitive is charged: the renderer clips a filter region to a layer five times the
/// output on each side, so a region far beyond the canvas still costs up to 25 canvases.
const MAX_PASS_SHARE: f64 = 25.0;

/// Largest `order` of an `<feConvolveMatrix>`: its cost is every pixel times every kernel cell.
const MAX_CONVOLVE_ORDER: u32 = 5;

/// Largest `numOctaves` of an `<feTurbulence>`: each octave is another pass over every pixel.
const MAX_TURBULENCE_OCTAVES: u32 = 8;

/// The numbers of a cost-bearing attribute (`order`, `numOctaves`), separated by spaces or commas.
/// A value that is not entirely finite numbers is refused rather than read as zero: the renderer
/// parses these attributes its own way, and a guard that reads a malformed value as harmless
/// passes a document the renderer may then take as large.
fn numbers(node: usvg::roxmltree::Node, attribute: &str) -> Result<Vec<f64>, AttachmentError> {
    let Some(value) = node.attribute(attribute) else {
        return Ok(Vec::new());
    };
    let malformed = || {
        AttachmentError::Invalid(format!(
            "SVG too costly to render (malformed {attribute} \"{}\")",
            value.chars().take(32).collect::<String>()
        ))
    };
    let parsed: Vec<f64> = value
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|token| !token.is_empty())
        .map(|token| token.parse::<f64>().ok().filter(|n| n.is_finite()))
        .collect::<Option<_>>()
        .ok_or_else(malformed)?;
    if parsed.is_empty() {
        return Err(malformed());
    }
    Ok(parsed)
}

/// Refuses a document whose rendering would cost far more than its size suggests: `<use>`
/// elements that expand into more than [`MAX_SVG_NODES`] nodes (nested references multiply), too
/// many filter primitives in the source, or primitives whose cost grows with a parameter. The
/// parser's node limit counts the source only; this counts what the renderer would build.
/// [`check_filter_budget`] then counts the filters of the built tree, copies included.
fn check_budget(document: &usvg::roxmltree::Document) -> Result<(), AttachmentError> {
    let refused =
        |why: String| AttachmentError::Invalid(format!("SVG too costly to render ({why})"));
    let ids: HashMap<&str, usvg::roxmltree::Node> = document
        .descendants()
        .filter_map(|n| n.attribute("id").map(|id| (id, n)))
        .collect();
    let mut primitives = 0;
    for node in document.descendants().filter(|n| n.is_element()) {
        let name = node.tag_name().name();
        if name.starts_with("fe") && name.len() > 2 && name.as_bytes()[2].is_ascii_uppercase() {
            primitives += 1;
            if primitives > MAX_FILTER_PRIMITIVES {
                return Err(refused(format!(
                    "more than {MAX_FILTER_PRIMITIVES} filter steps"
                )));
            }
        }
        if name == "feConvolveMatrix" {
            // `order` is one or two numbers (columns, then rows); the renderer reads both.
            let order = numbers(node, "order")?
                .iter()
                .take(2)
                .map(|n| n.trunc())
                .fold(0.0, f64::max);
            if order > MAX_CONVOLVE_ORDER as f64 {
                return Err(refused(format!(
                    "a convolution larger than {MAX_CONVOLVE_ORDER}x{MAX_CONVOLVE_ORDER}"
                )));
            }
        }
        if name == "feTurbulence" {
            // The renderer reads it as a 32-bit float, clamps a negative count to zero and rounds it.
            let octaves = numbers(node, "numOctaves")?
                .first()
                .map_or(0.0, |n| (*n as f32).max(0.0).round() as f64);
            if octaves > MAX_TURBULENCE_OCTAVES as f64 {
                return Err(refused(format!(
                    "more than {MAX_TURBULENCE_OCTAVES} turbulence octaves"
                )));
            }
        }
    }
    let mut sizes = HashMap::new();
    let total = expanded(document.root_element(), &ids, &mut sizes, 0);
    if total > MAX_SVG_NODES as u64 {
        return Err(refused(format!(
            "references expand to more than {MAX_SVG_NODES} elements"
        )));
    }
    Ok(())
}

/// How many elements `node` becomes once every `<use>` below it is replaced by its target,
/// saturating. A reference cycle counts as the budget itself (usvg skips cycles; a cycle is never
/// a legitimate drawing).
fn expanded<'a>(
    node: usvg::roxmltree::Node<'a, 'a>,
    ids: &HashMap<&'a str, usvg::roxmltree::Node<'a, 'a>>,
    sizes: &mut HashMap<usvg::roxmltree::NodeId, u64>,
    depth: usize,
) -> u64 {
    if let Some(size) = sizes.get(&node.id()) {
        return *size;
    }
    if depth > 64 {
        return MAX_SVG_NODES as u64 + 1;
    }
    // Marks the node while it is counted, so a cycle reads as over budget.
    sizes.insert(node.id(), MAX_SVG_NODES as u64 + 1);
    let mut size: u64 = 1;
    for child in node.children().filter(|c| c.is_element()) {
        size = size.saturating_add(expanded(child, ids, sizes, depth + 1));
    }
    if node.tag_name().name() == "use" {
        let href = node
            .attribute("href")
            .or_else(|| node.attribute(("http://www.w3.org/1999/xlink", "href")));
        if let Some(target) = href
            .and_then(|h| h.strip_prefix('#'))
            .and_then(|id| ids.get(id))
        {
            size = size.saturating_add(expanded(*target, ids, sizes, depth + 1));
        }
    }
    sizes.insert(node.id(), size);
    size
}

/// Running total of [`check_filter_budget`].
struct FilterBudget {
    /// Output pixels per tree unit, horizontally and vertically.
    scale: (f32, f32),
    /// Output area in pixels.
    canvas: f64,
    /// Filter passes counted so far.
    passes: f64,
    /// Groups visited so far, repeats included.
    visited: u64,
}

impl FilterBudget {
    fn refused(why: String) -> AttachmentError {
        AttachmentError::Invalid(format!("SVG too costly to render ({why})"))
    }

    /// Walks `group` the way the renderer does: every use of a shared mask or pattern draws its
    /// content again, so its filters are charged again. `outer` is the transform of whatever
    /// references the subtree (identity for the document itself), since a subtree's own
    /// transforms do not include it.
    fn group(
        &mut self,
        group: &usvg::Group,
        outer: tiny_skia::Transform,
        depth: usize,
    ) -> Result<(), AttachmentError> {
        self.visited += 1;
        if depth > 2048 || self.visited > 2 * MAX_SVG_NODES as u64 {
            return Err(Self::refused(
                "the drawing nests or repeats too much".into(),
            ));
        }
        let transform = outer.pre_concat(group.abs_transform());
        for filter in group.filters() {
            // The renderer filters the region in an image whose axes are the element's own, with
            // any rotation or skew applied afterwards, and clips it to the layer, at most five
            // times the output on each side. Rotation does not change that image's size.
            let sx = transform.sx.hypot(transform.ky) * self.scale.0;
            let sy = transform.kx.hypot(transform.sy) * self.scale.1;
            let pixels = filter.rect().width() as f64
                * sx as f64
                * filter.rect().height() as f64
                * sy as f64;
            let share = (pixels / self.canvas).clamp(MIN_PASS_SHARE, MAX_PASS_SHARE);
            self.passes += share * filter.primitives().len().max(1) as f64;
            if self.passes > MAX_FILTER_PASSES {
                return Err(Self::refused(format!(
                    "filters cover more than {MAX_FILTER_PASSES} canvas-sized passes"
                )));
            }
            for primitive in filter.primitives() {
                if let usvg::filter::Kind::Image(image) = primitive.kind() {
                    self.group(image.root(), transform, depth + 1)?;
                }
            }
        }
        let mut mask = group.mask();
        while let Some(m) = mask {
            self.group(m.root(), transform, depth + 1)?;
            mask = m.mask();
        }
        for child in group.children() {
            match child {
                usvg::Node::Group(inner) => self.group(inner, outer, depth + 1)?,
                usvg::Node::Path(path) => {
                    let paints = [
                        path.fill().map(|f| f.paint()),
                        path.stroke().map(|s| s.paint()),
                    ];
                    for paint in paints.into_iter().flatten() {
                        if let usvg::Paint::Pattern(pattern) = paint {
                            self.group(
                                pattern.root(),
                                outer.pre_concat(path.abs_transform()),
                                depth + 1,
                            )?;
                        }
                    }
                }
                usvg::Node::Text(text) => self.group(text.flattened(), outer, depth + 1)?,
                usvg::Node::Image(_) => {}
            }
        }
        Ok(())
    }
}

/// Refuses a tree whose filters, counted once per time the renderer draws them, would cost more
/// than [`MAX_FILTER_PASSES`]. The tree has every `<use>` and every style rule applied, so a
/// filtered element repeated by `<use>`, filtered through CSS, or drawn inside a shared mask or
/// pattern, is counted at each copy. `scale` maps tree units to output pixels and `canvas` is the
/// output area in pixels.
fn check_filter_budget(
    tree: &usvg::Tree,
    scale: (f32, f32),
    canvas: f64,
) -> Result<(), AttachmentError> {
    FilterBudget {
        scale,
        canvas,
        passes: 0.0,
        visited: 0,
    }
    .group(tree.root(), tiny_skia::Transform::identity(), 0)
}

/// Renders `bytes` (an SVG document) to PNG bytes. One rendering at a time: each holds a
/// full-size canvas, and the budget above bounds one, not many in parallel.
pub fn rasterize(bytes: &[u8]) -> Result<Vec<u8>, AttachmentError> {
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    if bytes.len() > MAX_SVG_BYTES {
        return Err(AttachmentError::TooLarge {
            size: bytes.len(),
            max: MAX_SVG_BYTES,
        });
    }
    let unreadable = |error: String| AttachmentError::Invalid(format!("unreadable SVG ({error})"));
    let text = std::str::from_utf8(bytes)
        .map_err(|_| AttachmentError::Invalid("the SVG is not UTF-8 text".into()))?;
    let document = usvg::roxmltree::Document::parse_with_options(
        text,
        usvg::roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: MAX_SVG_NODES,
            ..Default::default()
        },
    )
    .map_err(|error| unreadable(error.to_string()))?;
    let mut options = usvg::Options {
        fontdb: fonts(),
        ..Default::default()
    };
    // Nothing outside the document: a file path, a URL or an embedded payload all resolve to
    // nothing and the element is skipped.
    options.image_href_resolver = usvg::ImageHrefResolver {
        resolve_data: Box::new(|_, _, _| None),
        resolve_string: Box::new(|_, _| None),
    };
    check_budget(&document)?;
    let tree = usvg::Tree::from_xmltree(&document, &options)
        .map_err(|error| unreadable(error.to_string()))?;
    let size = tree.size();
    let (width, height) = (size.width(), size.height());
    if !(width.is_finite() && height.is_finite()) || width <= 0.0 || height <= 0.0 {
        return Err(AttachmentError::Invalid(
            "the SVG has no drawable size".into(),
        ));
    }
    let longest = width.max(height);
    let scale = if longest < MIN_RENDER_SIDE || longest > RENDER_SIDE as f32 {
        RENDER_SIDE as f32 / longest
    } else {
        1.0
    };
    let target_w = ((width * scale).round() as u32).clamp(1, RENDER_SIDE);
    let target_h = ((height * scale).round() as u32).clamp(1, RENDER_SIDE);
    check_filter_budget(
        &tree,
        (target_w as f32 / width, target_h as f32 / height),
        target_w as f64 * target_h as f64,
    )?;
    let mut pixmap = tiny_skia::Pixmap::new(target_w, target_h)
        .ok_or_else(|| AttachmentError::Invalid("the SVG has no drawable size".into()))?;
    // A transparent background reads as black to some models: draw on white, as a viewer does.
    pixmap.fill(tiny_skia::Color::WHITE);
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(target_w as f32 / width, target_h as f32 / height),
        &mut pixmap.as_mut(),
    );
    pixmap
        .encode_png()
        .map_err(|error| AttachmentError::Invalid(format!("SVG rendering failed ({error})")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="100"><rect width="200" height="100" fill="red"/></svg>"#;

    fn pixel(png: &[u8], x: u32, y: u32) -> [u8; 4] {
        let pixmap = tiny_skia::Pixmap::decode_png(png).unwrap();
        let p = pixmap.pixel(x, y).unwrap();
        [p.red(), p.green(), p.blue(), p.alpha()]
    }

    fn side(png: &[u8]) -> (usize, usize) {
        let size = imagesize::blob_size(png).unwrap();
        (size.width, size.height)
    }

    #[test]
    fn a_small_svg_becomes_a_png_scaled_up_to_a_legible_size() {
        let png = rasterize(RED).unwrap();
        assert_eq!(side(&png), (2048, 1024));
        assert_eq!(pixel(&png, 10, 10), [255, 0, 0, 255]);
    }

    #[test]
    fn a_mid_sized_svg_keeps_its_size_and_a_huge_one_is_scaled_down() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="800" height="600"><rect width="800" height="600" fill="blue"/></svg>"#;
        assert_eq!(side(&rasterize(svg).unwrap()), (800, 600));
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="40000" height="10000"><rect width="40000" height="10000"/></svg>"#;
        assert_eq!(side(&rasterize(svg).unwrap()), (2048, 512));
    }

    // Given an SVG that names a local file, a URL and an embedded payload, each covering the page
    // Then none of them is read: the elements are skipped and the white page renders alone
    #[test]
    fn external_and_embedded_references_are_never_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("secret.png");
        std::fs::write(&secret, super::super::fixtures::png(4, 4)).unwrap();
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="600" height="600">
            <rect width="600" height="600" fill="white"/>
            <image href="{}" width="600" height="600"/>
            <image xlink:href="https://example.invalid/x.png" width="600" height="600"/>
            <image href="data:image/svg+xml;base64,PHN2ZyB4bWxucz0naHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmcnIHdpZHRoPScxMCcgaGVpZ2h0PScxMCc+PHJlY3Qgd2lkdGg9JzEwJyBoZWlnaHQ9JzEwJyBmaWxsPSdibGFjaycvPjwvc3ZnPg==" width="600" height="600"/>
            </svg>"#,
            secret.display()
        );
        let png = rasterize(svg.as_bytes()).unwrap();
        assert_eq!(pixel(&png, 300, 300), [255, 255, 255, 255]);
    }

    // Given small documents whose rendering would multiply: nested `<use>` fan-out, many filter
    //       steps, a huge convolution kernel, a self-referencing `<use>`
    // Then each is refused before the renderer builds anything
    #[test]
    fn documents_that_expand_or_filter_too_much_are_refused() {
        let mut fanout = String::from(
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="10" height="10"><defs><rect id="l0" width="1" height="1"/>"#,
        );
        for level in 1..=8 {
            fanout.push_str(&format!(r#"<g id="l{level}">"#));
            for _ in 0..10 {
                fanout.push_str(&format!(r##"<use href="#l{}"/>"##, level - 1));
            }
            fanout.push_str("</g>");
        }
        fanout.push_str(r##"</defs><use href="#l8"/></svg>"##);
        let filters = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><filter id="f">{}</filter></svg>"#,
            "<feGaussianBlur stdDeviation='9'/>".repeat(MAX_FILTER_PRIMITIVES + 1)
        );
        let kernel = r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><filter id="f"><feConvolveMatrix order="300" kernelMatrix="1"/></filter></svg>"#;
        let cycle = r##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><g id="a"><use href="#a"/></g></svg>"##;
        for svg in [fanout.as_str(), filters.as_str(), kernel, cycle] {
            let error = rasterize(svg.as_bytes()).unwrap_err();
            assert!(error.to_string().contains("too costly"), "{error}");
        }
        // A modest drawing with a few reused shapes and one blur still renders.
        let fine = r##"<svg xmlns="http://www.w3.org/2000/svg" width="600" height="600"><defs><filter id="b"><feGaussianBlur stdDeviation="2"/></filter><rect id="r" width="10" height="10"/></defs><use href="#r"/><use href="#r" x="20"/><rect width="50" height="50" filter="url(#b)"/></svg>"##;
        rasterize(fine.as_bytes()).unwrap();
    }

    fn costly(svg: &str) -> String {
        rasterize(svg.as_bytes()).unwrap_err().to_string()
    }

    fn filtered_copies(copies: usize, size: u32, filter_attribute: &str) -> String {
        format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="600" height="600"><defs><filter id="b"><feGaussianBlur stdDeviation="2"/></filter><rect id="r" width="{size}" height="{size}" {filter_attribute}/></defs>{}</svg>"##,
            r##"<use href="#r"/>"##.repeat(copies)
        )
    }

    // Given one blurred full-canvas element reused through `<use>`
    // Then every copy is charged, so enough copies are refused before anything is drawn, while a
    //      few still render
    #[test]
    fn a_filtered_element_repeated_through_use_is_charged_per_copy() {
        let many = filtered_copies(100, 600, r##"filter="url(#b)""##);
        assert!(costly(&many).contains("too costly"));
        let few = filtered_copies(3, 600, r##"filter="url(#b)""##);
        rasterize(few.as_bytes()).unwrap();
    }

    // Given a filter applied through a style rule, and thousands of tiny filtered copies
    // Then the style rule counts like an attribute, and tiny regions are not free
    #[test]
    fn filters_applied_by_style_and_tiny_filtered_copies_are_charged() {
        let styled = filtered_copies(100, 600, "class='s'")
            .replace("<defs>", "<style>.s{filter:url(#b)}</style><defs>");
        assert!(costly(&styled).contains("too costly"));
        let tiny = filtered_copies(9_000, 4, r##"filter="url(#b)""##);
        assert!(costly(&tiny).contains("too costly"));
        let few_tiny = filtered_copies(50, 4, r##"filter="url(#b)""##);
        rasterize(few_tiny.as_bytes()).unwrap();
    }

    // Given filters the first count would miss: inside a mask shared by many elements, over a
    //       region several canvases wide, and on a sheared element whose screen footprint is thin
    // Then each is charged what the renderer builds, and a page of small shadows still renders
    #[test]
    fn filters_in_masks_oversized_regions_and_shears_are_charged() {
        let masked = format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="600" height="600"><defs><filter id="b"><feGaussianBlur stdDeviation="2"/></filter><mask id="m"><rect width="600" height="600" fill="white" filter="url(#b)"/></mask></defs>{}</svg>"##,
            r##"<rect width="5" height="5" mask="url(#m)"/>"##.repeat(100)
        );
        assert!(costly(&masked).contains("too costly"));
        let wide = r##"<svg xmlns="http://www.w3.org/2000/svg" width="600" height="600"><filter id="w" x="-2" y="-2" width="5" height="5"><feGaussianBlur stdDeviation="2"/><feOffset dx="1"/></filter><rect width="600" height="600" filter="url(#w)"/></svg>"##;
        assert!(costly(wide).contains("too costly"));
        let sheared = filtered_copies(100, 600, r##"filter="url(#b)""##).replace(
            r##"<use href="#r"/>"##,
            r##"<use href="#r" transform="matrix(1 0 1 0.001 0 0)"/>"##,
        );
        assert!(costly(&sheared).contains("too costly"));
        let shadows = format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="600" height="600"><defs><filter id="s"><feGaussianBlur stdDeviation="2"/><feOffset dx="2" dy="2"/><feMerge><feMergeNode/></feMerge></filter></defs>{}</svg>"##,
            (0..20)
                .map(|i| format!(
                    r##"<rect x="{}" y="{}" width="40" height="40" filter="url(#s)"/>"##,
                    i * 25,
                    i * 25
                ))
                .collect::<String>()
        );
        rasterize(shadows.as_bytes()).unwrap();
    }

    // Given cost attributes the renderer may read differently from a plain number
    // Then a malformed one is refused instead of counted as zero, and the second `order` value
    //      counts too
    #[test]
    fn malformed_or_hidden_cost_attributes_are_refused() {
        let convolve = |order: &str| {
            format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" width="600" height="600"><filter id="f"><feConvolveMatrix order="{order}" kernelMatrix="1"/></filter></svg>"#
            )
        };
        let turbulence = |octaves: &str| {
            format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" width="600" height="600"><filter id="f"><feTurbulence numOctaves="{octaves}"/></filter></svg>"#
            )
        };
        for svg in [
            convolve("3 300"),
            convolve("3,300"),
            convolve("many"),
            convolve("3px"),
            convolve(""),
            turbulence("9"),
            turbulence("8.4999999"),
            turbulence("9px"),
            turbulence("1e3"),
            turbulence("NaN"),
            turbulence("inf"),
        ] {
            assert!(costly(&svg).contains("too costly"), "{svg}");
        }
        rasterize(convolve("3").as_bytes()).unwrap();
        rasterize(convolve("3, 3").as_bytes()).unwrap();
        rasterize(turbulence("2").as_bytes()).unwrap();
        rasterize(turbulence("-4").as_bytes()).unwrap();
    }

    #[test]
    fn a_dtd_and_its_entities_are_refused() {
        let svg = br#"<?xml version="1.0"?><!DOCTYPE svg [<!ENTITY a "aaaaaaaaaa"><!ENTITY b "&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;">]><svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><text>&b;</text></svg>"#;
        assert!(matches!(rasterize(svg), Err(AttachmentError::Invalid(_))));
    }

    #[test]
    fn too_many_nodes_too_many_bytes_no_size_and_non_svg_are_refused() {
        let many = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">{}</svg>"#,
            "<g/>".repeat(MAX_SVG_NODES as usize + 1)
        );
        assert!(matches!(
            rasterize(many.as_bytes()),
            Err(AttachmentError::Invalid(_))
        ));
        let big = vec![b' '; MAX_SVG_BYTES + 1];
        assert!(matches!(
            rasterize(&big),
            Err(AttachmentError::TooLarge { .. })
        ));
        assert!(rasterize(b"<html></html>").is_err());
        assert!(
            rasterize(br#"<svg xmlns="http://www.w3.org/2000/svg" width="0" height="0"/>"#)
                .is_err()
        );
    }
}
