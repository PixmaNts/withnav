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

/// Whether `bytes` is one well-formed XML document whose root element is `<svg>`, with the same
/// parser limits as [`rasterize`] (no DTD, bounded node count): the sniff a `.svg` name alone
/// cannot give. Unlike [`looks_like_svg`], an HTML page or prose that merely mentions `<svg`, or a
/// truncated document, is not one, and a self-closing `<svg .../>` is. Past [`MAX_SVG_BYTES`]
/// nothing is parsed and the answer is `false`.
pub fn is_svg_document(bytes: &[u8]) -> bool {
    bytes.len() <= MAX_SVG_BYTES
        && std::str::from_utf8(bytes).is_ok_and(|text| {
            text.trim_start_matches('\u{feff}')
                .trim_start()
                .starts_with('<')
                && usvg::roxmltree::Document::parse_with_options(
                    text,
                    usvg::roxmltree::ParsingOptions {
                        allow_dtd: false,
                        nodes_limit: MAX_SVG_NODES,
                        ..Default::default()
                    },
                )
                .is_ok_and(|document| document.root_element().tag_name().name() == "svg")
        })
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

/// Largest `order` of an `<feConvolveMatrix>`: its cost is every pixel times every kernel cell.
const MAX_CONVOLVE_ORDER: u32 = 5;

/// Largest `numOctaves` of an `<feTurbulence>`: each octave is another pass over every pixel.
const MAX_TURBULENCE_OCTAVES: u32 = 8;

/// Refuses a document whose rendering would cost far more than its size suggests: `<use>`
/// elements that expand into more than [`MAX_SVG_NODES`] nodes (nested references multiply), too
/// many filter primitives, or primitives whose cost grows with a parameter. The parser's node
/// limit counts the source only; this counts what the renderer would build.
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
        let number = |attribute: &str| {
            node.attribute(attribute)
                .and_then(|v| v.split_whitespace().next())
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.0)
        };
        if name == "feConvolveMatrix" && number("order") > MAX_CONVOLVE_ORDER as f64 {
            return Err(refused(format!(
                "a convolution larger than {MAX_CONVOLVE_ORDER}x{MAX_CONVOLVE_ORDER}"
            )));
        }
        if name == "feTurbulence" && number("numOctaves") > MAX_TURBULENCE_OCTAVES as f64 {
            return Err(refused(format!(
                "more than {MAX_TURBULENCE_OCTAVES} turbulence octaves"
            )));
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
