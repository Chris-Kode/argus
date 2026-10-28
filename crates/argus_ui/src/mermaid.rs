//! Mermaid diagrams in Markdown (```` ```mermaid ```` fences).
//!
//! [`render`] turns a diagram's source into an SVG with `mermaid-svg`, a pure
//! Rust renderer (no browser, no font or file access), in the colors of the
//! light or dark theme. `mermaid-svg` estimates text widths, so a long label
//! or a subgraph title can spill past its canvas; the SVG is then fitted to
//! what it really draws, measured by `usvg` with the bundled IBM Plex Sans.
//! GPUI draws the labels in the same font ([`crate::assets`] serves it to
//! its SVG renderer) and rasterizes the image off the UI thread.
//!
//! [`Diagrams`] is the global cache of rendered diagrams by source and
//! appearance. [`Diagrams::request`] renders a diagram on the background
//! executor and repaints the windows when it is done; a diagram that is not
//! ready, or that `mermaid-svg` cannot render, shows as its source.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, LazyLock};

use gpui::{App, Global, Image, ImageFormat};

use crate::fonts::{BUNDLED_UI_FAMILY, PLEX_SANS_REGULAR};
use crate::theme::Appearance;

/// Longest diagram source rendered, in bytes; longer ones show as source.
const MAX_SOURCE_BYTES: usize = 16 * 1024;
/// Room kept around what a diagram draws, in pixels.
const PADDING: f32 = 8.0;
/// Rendered diagrams kept; the cache starts over past it.
const MAX_CACHED: usize = 512;

/// A rendered diagram.
#[derive(Debug)]
pub struct Diagram {
    /// The SVG image.
    pub image: Arc<Image>,
    /// Natural width, in pixels.
    pub width: f32,
    /// Natural height, in pixels.
    pub height: f32,
}

#[derive(Debug)]
enum Entry {
    Pending,
    Ready(Arc<Diagram>),
    Failed,
}

/// Rendered diagrams by source and appearance.
#[derive(Debug, Default)]
pub struct Diagrams {
    entries: HashMap<u64, Entry>,
}

impl Global for Diagrams {}

fn key(source: &str, appearance: Appearance) -> u64 {
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    matches!(appearance, Appearance::Dark).hash(&mut hasher);
    hasher.finish()
}

impl Diagrams {
    /// The rendered diagram, if it is ready.
    pub fn get(source: &str, appearance: Appearance, cx: &App) -> Option<Arc<Diagram>> {
        match cx
            .try_global::<Self>()?
            .entries
            .get(&key(source, appearance))?
        {
            Entry::Ready(diagram) => Some(diagram.clone()),
            Entry::Pending | Entry::Failed => None,
        }
    }

    /// Renders `source` on the background executor unless it is rendered
    /// or rendering already; the windows repaint when it is done.
    pub fn request(source: &str, appearance: Appearance, cx: &mut App) {
        let key = key(source, appearance);
        let diagrams = cx.default_global::<Self>();
        if diagrams.entries.contains_key(&key) {
            return;
        }
        if diagrams.entries.len() >= MAX_CACHED {
            diagrams
                .entries
                .retain(|_, entry| matches!(entry, Entry::Pending));
        }
        diagrams.entries.insert(key, Entry::Pending);
        let source = source.to_owned();
        let task = cx
            .background_executor()
            .spawn(async move { render(&source, appearance) });
        cx.spawn(async move |cx| {
            let entry = match task.await {
                Ok(diagram) => Entry::Ready(Arc::new(diagram)),
                Err(error) => {
                    tracing::debug!(%error, "cannot render a Mermaid diagram");
                    Entry::Failed
                }
            };
            cx.update(|cx| {
                cx.default_global::<Self>().entries.insert(key, entry);
                cx.refresh_windows();
            });
        })
        .detach();
    }
}

/// Renders a Mermaid diagram to an SVG image fitted to what it draws.
/// Blocking; run it off the UI thread.
pub fn render(source: &str, appearance: Appearance) -> Result<Diagram, String> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(format!("longer than {MAX_SOURCE_BYTES} bytes"));
    }
    let mut theme = match appearance {
        Appearance::Dark => mermaid_svg::Theme::dark(),
        Appearance::Light => mermaid_svg::Theme::default_theme(),
    }
    .with_font(BUNDLED_UI_FAMILY);
    theme.responsive = false;
    // The renderer is young; a diagram it trips over must not take Argus
    // down.
    let svg = catch_unwind(AssertUnwindSafe(|| {
        mermaid_svg::render_with(source, &theme)
    }))
    .map_err(|_| "the renderer panicked".to_owned())?
    .map_err(|error| error.to_string())?;
    let (svg, width, height) = fit(&svg)?;
    Ok(Diagram {
        image: Arc::new(Image::from_bytes(ImageFormat::Svg, svg.into_bytes())),
        width,
        height,
    })
}

/// The fonts the fit measures labels with: the bundled UI font only.
static FONTS: LazyLock<Arc<usvg::fontdb::Database>> = LazyLock::new(|| {
    let mut fonts = usvg::fontdb::Database::new();
    fonts.load_font_data(PLEX_SANS_REGULAR.to_vec());
    fonts.set_sans_serif_family(BUNDLED_UI_FAMILY);
    Arc::new(fonts)
});

/// `svg` with its size and `viewBox` set to the bounds of what it draws,
/// plus [`PADDING`]; and that size.
fn fit(svg: &str) -> Result<(String, f32, f32), String> {
    let options = usvg::Options {
        fontdb: FONTS.clone(),
        ..Default::default()
    };
    let tree = usvg::Tree::from_str(svg, &options).map_err(|error| error.to_string())?;
    let bounds = tree.root().abs_layer_bounding_box();
    let start = svg.find("<svg").ok_or("no <svg> element")?;
    let end = start + svg[start..].find('>').ok_or("unterminated <svg> element")?;
    let root = &svg[start..end];

    // The bounds are in canvas pixels; the viewBox is in user units.
    let size = tree.size();
    let (mut x, mut y, mut scale_x, mut scale_y) = (0.0, 0.0, 1.0, 1.0);
    if let Some(view_box) = attribute(root, "viewBox") {
        let numbers: Vec<f32> = view_box
            .split([' ', ','])
            .filter_map(|part| part.parse().ok())
            .collect();
        if let [vx, vy, vw, vh] = numbers[..] {
            (x, y) = (vx, vy);
            (scale_x, scale_y) = (vw / size.width(), vh / size.height());
        }
    }
    let width = bounds.width() + 2.0 * PADDING;
    let height = bounds.height() + 2.0 * PADDING;
    let view_box = format!(
        "{} {} {} {}",
        x + (bounds.x() - PADDING) * scale_x,
        y + (bounds.y() - PADDING) * scale_y,
        width * scale_x,
        height * scale_y
    );
    let mut root = root.to_owned();
    for (name, value) in [
        ("width", width.to_string()),
        ("height", height.to_string()),
        ("viewBox", view_box),
    ] {
        root = set_attribute(&root, name, &value);
    }
    Ok((
        format!("{}{root}{}", &svg[..start], &svg[end..]),
        width,
        height,
    ))
}

/// The value of attribute `name` of the start tag `tag`.
fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let key = format!(" {name}=\"");
    let start = tag.find(&key)? + key.len();
    let end = start + tag[start..].find('"')?;
    Some(&tag[start..end])
}

/// The start tag `tag` with attribute `name` set to `value`.
fn set_attribute(tag: &str, name: &str, value: &str) -> String {
    let key = format!(" {name}=\"");
    match tag.find(&key) {
        Some(found) => {
            let start = found + key.len();
            let end = tag[start..].find('"').map_or(tag.len(), |end| start + end);
            format!("{}{value}{}", &tag[..start], &tag[end..])
        }
        None => format!("{tag}{key}{value}\""),
    }
}
