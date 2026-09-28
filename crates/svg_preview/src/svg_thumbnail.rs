use std::{path::Path, sync::Arc, time::SystemTime};

use gpui::{
    AnyElement, App, Asset, DevicePixels, ImageCacheError, ImageSource, Img, RenderImage,
    StyledImage, SvgSize, Window, img, size,
};
use ui::prelude::*;

/// Files larger than this are shown with their regular file icon, since parsing and
/// rasterizing them for a tiny thumbnail isn't worth the cost.
pub const MAX_SVG_THUMBNAIL_FILE_SIZE: u64 = 1024 * 1024;

#[derive(Clone, Hash, PartialEq, Eq)]
struct SvgThumbnailSource {
    abs_path: Arc<Path>,
    // Part of the cache key so that the thumbnail is re-rendered when the file changes.
    mtime: Option<SystemTime>,
    device_size: u32,
}

enum SvgThumbnailLoader {}

impl Asset for SvgThumbnailLoader {
    type Source = SvgThumbnailSource;
    type Output = Result<Arc<RenderImage>, ImageCacheError>;

    fn load(
        source: Self::Source,
        cx: &mut App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        let renderer = cx.svg_renderer();
        async move {
            let bytes = std::fs::read(&source.abs_path)?;
            let parsed_svg = renderer.parse_svg(&bytes)?;
            let svg_size = parsed_svg.size();
            let longest_side = svg_size.width.max(svg_size.height);
            // `SvgSize::Size` only honors the width, so shrink it for tall SVGs to keep the
            // raster within the thumbnail's square.
            let width = if longest_side > 0.0 {
                (source.device_size as f32 * svg_size.width / longest_side).round()
            } else {
                source.device_size as f32
            };
            let width = DevicePixels(width.max(1.0) as i32);
            Ok(renderer.render_parsed(&parsed_svg, SvgSize::Size(size(width, width)))?)
        }
    }
}

/// A small rendering of the SVG file at `abs_path`, sized like an icon.
///
/// `fallback` is shown while the thumbnail loads and when the file can't be rendered.
pub fn svg_thumbnail(
    abs_path: Arc<Path>,
    mtime: Option<SystemTime>,
    icon_size: IconSize,
    fallback: impl Fn() -> AnyElement + 'static,
) -> Img {
    let fallback = Arc::new(fallback);
    let source = ImageSource::Custom(Arc::new(move |window: &mut Window, cx: &mut App| {
        let logical_size = icon_size.rems().to_pixels(window.rem_size());
        let device_size = (f32::from(logical_size) * window.scale_factor()).ceil() as u32;
        let source = SvgThumbnailSource {
            abs_path: abs_path.clone(),
            mtime,
            device_size,
        };
        window.use_asset::<SvgThumbnailLoader>(&source, cx)
    }));

    img(source)
        .size(icon_size.rems())
        .flex_none()
        .with_loading({
            let fallback = fallback.clone();
            move || fallback()
        })
        .with_fallback(move || fallback())
}
