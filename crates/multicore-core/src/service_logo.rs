use std::io::Cursor;

use image::{DynamicImage, GenericImageView, ImageFormat, ImageReader, Limits, imageops};
use quick_xml::{
    Reader, XmlVersion,
    events::{BytesStart, Event as XmlEvent},
};
use resvg::{tiny_skia, usvg};

pub const MAX_SERVICE_LOGO_BYTES: usize = 512 * 1024;
pub const MAX_SERVICE_LOGO_OUTPUT_BYTES: usize = 512 * 1024;
const MAX_SOURCE_DIMENSION: u32 = 2048;
const MAX_OUTPUT_DIMENSION: u32 = 256;
const MAX_DECODED_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SVG_ELEMENTS: usize = 4096;
const MAX_SVG_DEPTH: usize = 64;
const MAX_SVG_PATH_SEGMENTS: usize = 16_384;

/// Validates an untrusted provider image and converts it into a bounded RGBA PNG.
/// The input format is detected from its bytes; filenames and response MIME types are ignored.
pub fn normalize_service_logo(source: &[u8]) -> Option<Vec<u8>> {
    if source.is_empty() || source.len() > MAX_SERVICE_LOGO_BYTES {
        return None;
    }
    match image::guess_format(source).ok() {
        Some(format @ (ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP)) => {
            normalize_raster(source, format)
        }
        Some(_) => None,
        None if looks_like_svg(source) => normalize_svg(source),
        None => None,
    }
}

pub(crate) fn is_canonical_service_logo(source: &[u8]) -> bool {
    if source.len() > MAX_SERVICE_LOGO_OUTPUT_BYTES
        || image::guess_format(source).ok() != Some(ImageFormat::Png)
        || contains_png_animation(source)
    {
        return false;
    }
    decode_raster(source, ImageFormat::Png).is_some_and(|image| {
        image.width() <= MAX_OUTPUT_DIMENSION
            && image.height() <= MAX_OUTPUT_DIMENSION
            && image.color() == image::ColorType::Rgba8
    })
}

fn normalize_raster(source: &[u8], format: ImageFormat) -> Option<Vec<u8>> {
    if (format == ImageFormat::Png && contains_png_animation(source))
        || (format == ImageFormat::WebP && contains_webp_animation(source))
    {
        return None;
    }
    let image = decode_raster(source, format)?;
    encode_scaled_rgba(image)
}

fn decode_raster(source: &[u8], format: ImageFormat) -> Option<DynamicImage> {
    let mut reader = ImageReader::with_format(Cursor::new(source), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_DIMENSION);
    limits.max_image_height = Some(MAX_SOURCE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    reader.limits(limits);
    let image = reader.decode().ok()?;
    let (width, height) = image.dimensions();
    let decoded = u64::from(width)
        .checked_mul(u64::from(height))?
        .checked_mul(4)?;
    (width > 0
        && height > 0
        && width <= MAX_SOURCE_DIMENSION
        && height <= MAX_SOURCE_DIMENSION
        && decoded <= MAX_DECODED_BYTES)
        .then_some(image)
}

fn encode_scaled_rgba(image: DynamicImage) -> Option<Vec<u8>> {
    let (width, height) = image.dimensions();
    let scale = (MAX_OUTPUT_DIMENSION as f64 / f64::from(width))
        .min(MAX_OUTPUT_DIMENSION as f64 / f64::from(height))
        .min(1.0);
    let target_width = (f64::from(width) * scale).round().max(1.0) as u32;
    let target_height = (f64::from(height) * scale).round().max(1.0) as u32;
    let rgba = image.to_rgba8();
    let rgba = if (target_width, target_height) == (width, height) {
        rgba
    } else {
        imageops::resize(
            &rgba,
            target_width,
            target_height,
            imageops::FilterType::Lanczos3,
        )
    };
    let mut output = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(rgba)
        .write_to(&mut output, ImageFormat::Png)
        .ok()?;
    (output.get_ref().len() <= MAX_SERVICE_LOGO_OUTPUT_BYTES).then(|| output.into_inner())
}

fn normalize_svg(source: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(source).ok()?;
    if !bounded_xml_preflight(text) {
        return None;
    }
    let options = usvg::Options {
        resources_dir: None,
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: Box::new(|_, _, _| None),
            resolve_string: Box::new(|_, _| None),
        },
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_data(source, &options).ok()?;
    let size = tree.size();
    if !size.width().is_finite()
        || !size.height().is_finite()
        || size.width() <= 0.0
        || size.height() <= 0.0
        || size.width() > MAX_SOURCE_DIMENSION as f32
        || size.height() > MAX_SOURCE_DIMENSION as f32
        || !bounded_group(tree.root(), 0, &mut SvgBudget::default())
    {
        return None;
    }

    let scale = (MAX_OUTPUT_DIMENSION as f32 / size.width())
        .min(MAX_OUTPUT_DIMENSION as f32 / size.height())
        .min(1.0);
    let width = (size.width() * scale).round().max(1.0) as u32;
    let height = (size.height() * scale).round().max(1.0) as u32;
    let mut pixmap = tiny_skia::Pixmap::new(width, height)?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let output = pixmap.encode_png().ok()?;
    (output.len() <= MAX_SERVICE_LOGO_OUTPUT_BYTES).then_some(output)
}

fn bounded_xml_preflight(source: &str) -> bool {
    let mut reader = Reader::from_str(source);
    let mut elements = 0_usize;
    let mut depth = 0_usize;
    loop {
        match reader.read_event() {
            Ok(XmlEvent::Start(element)) => {
                if has_forbidden_svg_expansion(&element, &reader) {
                    return false;
                }
                elements += 1;
                depth += 1;
                if elements > MAX_SVG_ELEMENTS || depth > MAX_SVG_DEPTH {
                    return false;
                }
            }
            Ok(XmlEvent::Empty(element)) => {
                if has_forbidden_svg_expansion(&element, &reader) {
                    return false;
                }
                elements += 1;
                if elements > MAX_SVG_ELEMENTS || depth + 1 > MAX_SVG_DEPTH {
                    return false;
                }
            }
            Ok(XmlEvent::End(_)) => {
                let Some(parent_depth) = depth.checked_sub(1) else {
                    return false;
                };
                depth = parent_depth;
            }
            Ok(XmlEvent::DocType(_) | XmlEvent::GeneralRef(_)) => return false,
            Ok(XmlEvent::Eof) => return depth == 0,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
}

fn has_forbidden_svg_expansion(element: &BytesStart<'_>, reader: &Reader<&[u8]>) -> bool {
    let local_name = element.local_name();
    let name = local_name.as_ref();
    const SAFE_ELEMENTS: &[&[u8]] = &[
        b"svg",
        b"g",
        b"defs",
        b"path",
        b"rect",
        b"circle",
        b"ellipse",
        b"line",
        b"polyline",
        b"polygon",
        b"linearGradient",
        b"radialGradient",
        b"stop",
        b"metadata",
        b"title",
        b"desc",
    ];
    if !SAFE_ELEMENTS
        .iter()
        .any(|allowed| name.eq_ignore_ascii_case(allowed))
    {
        return true;
    }
    for attribute in element.attributes() {
        let Ok(attribute) = attribute else {
            return true;
        };
        let local_name = attribute.key.local_name();
        let name = local_name.as_ref();
        if name.eq_ignore_ascii_case(b"href")
            || name.eq_ignore_ascii_case(b"filter")
            || name.eq_ignore_ascii_case(b"mask")
            || name.eq_ignore_ascii_case(b"clip-path")
            || name.eq_ignore_ascii_case(b"marker")
            || name.eq_ignore_ascii_case(b"marker-start")
            || name.eq_ignore_ascii_case(b"marker-mid")
            || name.eq_ignore_ascii_case(b"marker-end")
            || (name.len() > 2 && name[..2].eq_ignore_ascii_case(b"on"))
        {
            return true;
        }
        if name.eq_ignore_ascii_case(b"style") {
            let Ok(value) =
                attribute.decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
            else {
                return true;
            };
            let value = value.to_ascii_lowercase();
            if ["filter", "mask", "clip-path", "marker"]
                .iter()
                .any(|forbidden| value.contains(forbidden))
            {
                return true;
            }
        }
    }
    false
}

#[derive(Default)]
struct SvgBudget {
    nodes: usize,
    path_segments: usize,
}

fn bounded_group(group: &usvg::Group, depth: usize, budget: &mut SvgBudget) -> bool {
    if depth > MAX_SVG_DEPTH {
        return false;
    }
    for node in group.children() {
        budget.nodes += 1;
        if budget.nodes > MAX_SVG_ELEMENTS {
            return false;
        }
        if let usvg::Node::Path(path) = node {
            budget.path_segments = budget
                .path_segments
                .saturating_add(path.data().segments().count());
            if budget.path_segments > MAX_SVG_PATH_SEGMENTS {
                return false;
            }
        }
        if let usvg::Node::Group(child) = node
            && !bounded_group(child, depth + 1, budget)
        {
            return false;
        }
        let mut subroots_are_bounded = true;
        node.subroots(|subroot| {
            if subroots_are_bounded && !bounded_group(subroot, depth + 1, budget) {
                subroots_are_bounded = false;
            }
        });
        if !subroots_are_bounded {
            return false;
        }
    }
    true
}

fn looks_like_svg(source: &[u8]) -> bool {
    std::str::from_utf8(source).ok().is_some_and(|text| {
        text.trim_start_matches(['\u{feff}', ' ', '\t', '\r', '\n'])
            .starts_with('<')
            && text.contains("<svg")
    })
}

fn contains_png_animation(source: &[u8]) -> bool {
    source.windows(4).any(|chunk| chunk == b"acTL")
}

fn contains_webp_animation(source: &[u8]) -> bool {
    source
        .windows(4)
        .any(|chunk| matches!(chunk, b"ANIM" | b"ANMF"))
}

#[cfg(test)]
mod tests {
    use super::bounded_xml_preflight;

    #[test]
    fn streaming_preflight_rejects_expanding_svg_constructs_directly() {
        for svg in [
            "<svg><defs><g id='x'><path d='M0 0'/></g></defs><use href='#x'/></svg>",
            "<svg><defs><marker id='x'><path d='M0 0'/></marker></defs></svg>",
            "<svg><path marker-end='url(#x)' d='M0 0'/></svg>",
            "<svg><path style='marker-start:url(#x)' d='M0 0'/></svg>",
            "<svg><defs><filter id='x'><feImage href='https://example.invalid/a'/></filter></defs></svg>",
            "<svg><rect filter='url(#x)'/></svg>",
            "<svg><rect style='filter:url(#x)'/></svg>",
            "<svg><pattern id='x'/></svg>",
            "<svg><text>logo</text></svg>",
        ] {
            assert!(!bounded_xml_preflight(svg));
        }
    }

    #[test]
    fn streaming_preflight_accepts_simple_internal_gradients() {
        assert!(bounded_xml_preflight(
            "<svg><defs><linearGradient id='g'><stop offset='0' stop-color='#fff'/><stop offset='1' stop-color='#000'/></linearGradient></defs><rect width='1' height='1' fill='url(#g)'/></svg>"
        ));
    }
}
