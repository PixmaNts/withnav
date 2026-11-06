//! SVG attachments, rasterized to PNG before they are stored.
//!
//! Providers read raster images only, and an SVG shown as is in a client could run script, so the
//! store never keeps one: it keeps the PNG this module renders. The renderer reads no file and no
//! URL (every `href` resolves to nothing), refuses a DTD (no entity expansion), stops past a node
//! budget, and draws text with the system fonts only.

use std::sync::{Arc, OnceLock};

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

/// Renders `bytes` (an SVG document) to PNG bytes.
pub fn rasterize(bytes: &[u8]) -> Result<Vec<u8>, AttachmentError> {
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
