use blitz_traits::node_id::NodeId;
use selectors::context::QuirksMode;
use std::sync::atomic::Ordering as Ao;
use std::{
    io::Cursor,
    sync::{Arc, atomic::AtomicUsize, mpsc::Sender},
};
use style::{
    font_face::{
        FontFaceRule, FontFaceSourceFormat, FontFaceSourceFormatKeyword, FontStyleRange, Source,
    },
    media_queries::MediaList,
    servo_arc::Arc as ServoArc,
    shared_lock::SharedRwLock,
    shared_lock::{Locked, SharedRwLockReadGuard},
    stylesheets::{
        DocumentStyleSheet, ImportRule, Origin, Stylesheet,
        StylesheetLoader as ServoStylesheetLoader, UrlExtraData,
        import_rule::{ImportLayer, ImportSheet, ImportSupportsCondition},
    },
    values::{CssUrl, SourceLocation},
};

use blitz_traits::net::{AbortSignal, Bytes, NetHandler, NetProvider, Request, RequestKind};
use blitz_traits::shell::ShellProvider;

use url::Url;

use crate::{document::DocumentEvent, util::ImageType};

pub(crate) fn stamped_request(url: Url, signal: Option<&AbortSignal>) -> Request {
    let mut req = Request::get(url);
    if let Some(sig) = signal {
        req = req.signal(sig.clone());
    }
    req
}

/// Carries `@font-face` descriptors from CSS parsing through to font
/// registration so `parley::Collection::register_fonts` can alias the bytes
/// under the `font-family` declared in CSS rather than whatever family name
/// the TTF's own `name` table reports.
///
/// All fields are `Option` because each descriptor is independently optional
/// at the CSS level. Missing fields fall back to the values parley reads
/// from the font's own metadata.
#[derive(Clone, Debug, Default)]
pub struct FontFaceOverrides {
    /// `font-family` descriptor (the alias the rest of the stylesheet uses).
    pub family_name: Option<String>,
    /// `font-weight` descriptor as a single CSS weight (100–900). Stylo
    /// parses this as a range; we record the lower bound, which equals the
    /// upper bound in the common single-value case.
    pub weight: Option<f32>,
    /// `font-style` descriptor mapped to fontique's `FontStyle`.
    pub style: Option<parley::fontique::FontStyle>,
    /// `font-stretch` (`font-width`) descriptor as a single width. Like
    /// `weight`, we record the lower bound of Stylo's range.
    pub width: Option<parley::fontique::FontWidth>,
}

#[derive(Clone, Debug)]
pub enum Resource {
    Image(ImageType, u32, u32, Arc<Vec<u8>>),
    #[cfg(feature = "svg")]
    Svg(ImageType, crate::node::SvgImageData),
    Css(DocumentStyleSheet),
    /// Stylesheet loaded for an `@import` rule, to be attached to the rule on the document thread
    ImportedCss(ServoArc<Locked<ImportRule>>, ServoArc<Stylesheet>),
    Font(Bytes, FontFaceOverrides),
    /// HTML fetched for an `<iframe>` element's `src`
    DocumentSrc(String),
    None,
}

pub(crate) struct ResourceHandler<T: Send + Sync + 'static> {
    doc_id: usize,
    request_id: usize,
    node_id: Option<NodeId>,
    tx: Sender<DocumentEvent>,
    shell_provider: Arc<dyn ShellProvider>,
    data: T,
}

impl<T: Send + Sync + 'static> ResourceHandler<T> {
    pub(crate) fn new(
        tx: Sender<DocumentEvent>,
        doc_id: usize,
        node_id: Option<NodeId>,
        shell_provider: Arc<dyn ShellProvider>,
        data: T,
    ) -> Self {
        static REQUEST_ID_COUNTER: AtomicUsize = AtomicUsize::new(0);
        Self {
            request_id: REQUEST_ID_COUNTER.fetch_add(1, Ao::Relaxed),
            doc_id,
            node_id,
            tx,
            shell_provider,
            data,
        }
    }

    pub(crate) fn boxed(
        tx: Sender<DocumentEvent>,
        doc_id: usize,
        node_id: Option<NodeId>,
        shell_provider: Arc<dyn ShellProvider>,
        data: T,
    ) -> Box<dyn NetHandler>
    where
        ResourceHandler<T>: NetHandler,
    {
        Box::new(Self::new(tx, doc_id, node_id, shell_provider, data)) as _
    }

    pub(crate) fn request_id(&self) -> usize {
        self.request_id
    }

    /// Hands one resource result to the document. `resolved_url` is the URL
    /// the bytes came from, which a redirect can change; an image instead
    /// names the key it was filed under, because the key also carries the
    /// element's CORS state
    /// (<https://html.spec.whatwg.org/multipage/urls-and-fetching.html#cors-settings-attributes>).
    /// `origin_clean` is the response taint an image request settles with;
    /// other resources carry `false` and ignore it. `font_face` names the
    /// `@font-face` rule and source a font response settled, so the document
    /// can advance the rule's source list on failure
    /// (<https://drafts.csswg.org/css-fonts-4/#src-desc>).
    fn respond(
        &self,
        resolved_url: Option<String>,
        image_key: Option<crate::document::ImageKey>,
        origin_clean: bool,
        font_face: Option<crate::net::FontFaceLoadKey>,
        result: Result<Resource, String>,
    ) {
        let response = ResourceLoadResponse {
            request_id: self.request_id,
            node_id: self.node_id,
            resolved_url,
            image_key,
            origin_clean,
            font_face,
            result,
        };
        let _ = self.tx.send(DocumentEvent::ResourceLoad(response));
        self.shell_provider.request_redraw();
    }
}

#[allow(unused)]
pub struct ResourceLoadResponse {
    pub request_id: usize,
    pub node_id: Option<NodeId>,
    pub resolved_url: Option<String>,
    /// The `(url, crossorigin)` key an image request was filed under.
    pub image_key: Option<crate::document::ImageKey>,
    /// Whether the response is origin-clean, so an image request that settles
    /// with it carries the taint into canvas
    /// (<https://html.spec.whatwg.org/multipage/canvas.html#the-image-argument>).
    pub origin_clean: bool,
    /// The `@font-face` rule and source index a font response belongs to.
    pub font_face: Option<crate::net::FontFaceLoadKey>,
    pub result: Result<Resource, String>,
}

pub struct StylesheetHandler {
    pub source_url: Url,
    pub guard: SharedRwLock,
    pub net_provider: Arc<dyn NetProvider>,
    pub abort_signal: Option<AbortSignal>,
}

impl NetHandler for ResourceHandler<StylesheetHandler> {
    fn error(self: Box<Self>, url: String, error: String) {
        self.respond(Some(url), None, false, None, Err(error));
    }

    fn bytes(self: Box<Self>, resolved_url: String, bytes: Bytes) {
        // NOTE(Nico): I don't *think* external stylesheets should have HTML entities escaped
        // let escaped_css = html_escape::decode_html_entities(css);

        let sheet = Stylesheet::from_bytes(
            &bytes,
            self.data.source_url.clone().into(),
            None,
            None,
            Origin::Author,
            ServoArc::new(self.data.guard.wrap(MediaList::empty())),
            self.data.guard.clone(),
            Some(&StylesheetLoader {
                tx: self.tx.clone(),
                doc_id: self.doc_id,
                net_provider: self.data.net_provider.clone(),
                shell_provider: self.shell_provider.clone(),
                abort_signal: self.data.abort_signal.clone(),
                import_depth: 0,
                sheet_url: Some(self.data.source_url.clone()),
            }),
            None, // error_reporter
            QuirksMode::NoQuirks,
        );

        self.respond(
            Some(resolved_url),
            None,
            false,
            None,
            Ok(Resource::Css(DocumentStyleSheet(ServoArc::new(sheet)))),
        );
    }
}

/// Maximum depth of nested `@import` rules. Imports nested deeper than this
/// are refused, preventing unbounded recursion (e.g. a stylesheet whose
/// resolved `@import` URL yields another stylesheet importing in turn, with
/// the URL growing geometrically at each level).
const MAX_IMPORT_DEPTH: usize = 16;

#[derive(Clone)]
pub(crate) struct StylesheetLoader {
    pub(crate) tx: Sender<DocumentEvent>,
    pub(crate) doc_id: usize,
    pub(crate) net_provider: Arc<dyn NetProvider>,
    pub(crate) shell_provider: Arc<dyn ShellProvider>,
    pub(crate) abort_signal: Option<AbortSignal>,
    /// Depth of `@import` nesting for the stylesheet this loader is parsing
    /// (0 for a top-level stylesheet)
    pub(crate) import_depth: usize,
    /// Location of the stylesheet this loader is parsing, if it has one. A
    /// nested sheet's request uses it as its referrer
    /// (<https://w3c.github.io/webappsec-referrer-policy/#integration-with-css>).
    pub(crate) sheet_url: Option<Url>,
}
impl ServoStylesheetLoader for StylesheetLoader {
    fn request_stylesheet(
        &self,
        url: CssUrl,
        location: SourceLocation,
        lock: &SharedRwLock,
        media: ServoArc<Locked<MediaList>>,
        supports: Option<ImportSupportsCondition>,
        layer: ImportLayer,
    ) -> ServoArc<Locked<ImportRule>> {
        // A refused import installs nothing: one nested too deep, one whose
        // condition is unsupported, and one whose URL does not resolve. An
        // empty `url()` names no sheet, so resolving it against the base URL
        // and fetching that would request the document itself
        // (<https://drafts.csswg.org/css-values-4/#url-empty>).
        let Some(import_url) = url.url().cloned().filter(|_| {
            self.import_depth < MAX_IMPORT_DEPTH && supports.as_ref().is_none_or(|s| s.enabled)
        }) else {
            return ServoArc::new(lock.wrap(ImportRule {
                url,
                stylesheet: ImportSheet::new_refused(),
                supports,
                layer,
                source_location: location,
            }));
        };

        let import = ImportRule {
            url,
            stylesheet: ImportSheet::new_pending(),
            supports,
            layer,
            source_location: location,
        };

        let import = ServoArc::new(lock.wrap(import));
        self.net_provider.fetch(
            self.doc_id,
            stamped_request(import_url.as_ref().clone(), self.abort_signal.as_ref())
                .kind(RequestKind::Stylesheet)
                .style_sheet(self.sheet_url.clone()),
            ResourceHandler::boxed(
                self.tx.clone(),
                self.doc_id,
                None, // node_id
                self.shell_provider.clone(),
                NestedStylesheetHandler {
                    url: import_url.clone(),
                    loader: StylesheetLoader {
                        import_depth: self.import_depth + 1,
                        ..self.clone()
                    },
                    lock: lock.clone(),
                    media,
                    import_rule: import.clone(),
                },
            ),
        );

        import
    }
}

struct NestedStylesheetHandler {
    loader: StylesheetLoader,
    lock: SharedRwLock,
    url: ServoArc<Url>,
    media: ServoArc<Locked<MediaList>>,
    import_rule: ServoArc<Locked<ImportRule>>,
}

impl NetHandler for ResourceHandler<NestedStylesheetHandler> {
    fn bytes(self: Box<Self>, resolved_url: String, bytes: Bytes) {
        // NOTE(Nico): I don't *think* external stylesheets should have HTML entities escaped
        // let escaped_css = html_escape::decode_html_entities(css);
        let loader = StylesheetLoader {
            // The sheet being parsed is the one this URL names: any `@import`
            // inside it uses this URL as its referrer, not the importing
            // sheet's
            // (<https://w3c.github.io/webappsec-referrer-policy/#integration-with-css>).
            sheet_url: Some(self.data.url.as_ref().clone()),
            ..self.data.loader.clone()
        };
        let sheet = ServoArc::new(Stylesheet::from_bytes(
            &bytes,
            UrlExtraData(self.data.url.clone()),
            None,
            None,
            Origin::Author,
            self.data.media.clone(),
            self.data.lock.clone(),
            Some(&loader),
            None, // error_reporter
            QuirksMode::NoQuirks,
        ));

        // The fonts the sheet names are fetched when the document installs
        // it, so the rule's source list is owned where the load outcomes are
        // processed (<https://drafts.csswg.org/css-fonts-4/#font-face-rule-processing>).

        let import_rule = self.data.import_rule.clone();
        self.respond(
            Some(resolved_url),
            None,
            false,
            None,
            Ok(Resource::ImportedCss(import_rule, sheet)),
        )
    }
}

/// One in-flight `@font-face` fetch: the rule the source list came from, the
/// index of the source being fetched, and the parse hints that source gave.
/// The document thread uses the rule to advance the source list when this
/// source fails, and the CSSOM address to report the outcome where script can
/// find the rule
/// (<https://drafts.csswg.org/css-fonts-4/#src-desc>).
#[derive(Clone)]
pub struct FontFaceLoadKey {
    pub rule: ServoArc<Locked<FontFaceRule>>,
    /// The URL the fetch was made from, so the next source's request carries
    /// the same referrer the first one did
    /// (<https://w3c.github.io/webappsec-referrer-policy/#integration-with-css>).
    pub sheet_url: Option<Url>,
    pub source_index: usize,
    /// The rule's CSSOM address, recorded when the walk started: a
    /// re-processed style element replaces its sheet, so resolving the
    /// address at delivery would name a rule that is no longer there.
    pub cssom_sheet: u64,
    pub cssom_path: Vec<usize>,
}

struct FontFaceHandler {
    key: FontFaceLoadKey,
    format: FontFaceSourceFormatKeyword,
    overrides: FontFaceOverrides,
}
impl NetHandler for ResourceHandler<FontFaceHandler> {
    fn bytes(mut self: Box<Self>, resolved_url: String, bytes: Bytes) {
        let result = self.data.parse(bytes);
        self.respond(
            Some(resolved_url),
            None,
            false,
            Some(self.data.key.clone()),
            result,
        )
    }
    fn error(self: Box<Self>, url: String, error: String) {
        self.respond(
            Some(url),
            None,
            false,
            Some(self.data.key.clone()),
            Err(error),
        )
    }
}
impl FontFaceHandler {
    fn parse(&mut self, bytes: Bytes) -> Result<Resource, String> {
        if self.format == FontFaceSourceFormatKeyword::None && bytes.len() >= 4 {
            self.format = match &bytes.as_ref()[0..4] {
                // WOFF (v1) files begin with 0x774F4646 ('wOFF' in ascii)
                // See: <https://w3c.github.io/woff/woff1/spec/Overview.html#WOFFHeader>
                #[cfg(feature = "woff")]
                b"wOFF" => FontFaceSourceFormatKeyword::Woff,
                // WOFF2 files begin with 0x774F4632 ('wOF2' in ascii)
                // See: <https://w3c.github.io/woff/woff2/#woff20Header>
                #[cfg(feature = "woff")]
                b"wOF2" => FontFaceSourceFormatKeyword::Woff2,
                // Opentype fonts with CFF data begin with 0x4F54544F ('OTTO' in ascii)
                // See: <https://learn.microsoft.com/en-us/typography/opentype/spec/otff#organization-of-an-opentype-font>
                b"OTTO" => FontFaceSourceFormatKeyword::Opentype,
                // Opentype fonts truetype outlines begin with 0x00010000
                // See: <https://learn.microsoft.com/en-us/typography/opentype/spec/otff#organization-of-an-opentype-font>
                &[0x00, 0x01, 0x00, 0x00] => FontFaceSourceFormatKeyword::Truetype,
                // Truetype fonts begin with 0x74727565 ('true' in ascii)
                // See: <https://developer.apple.com/fonts/TrueType-Reference-Manual/RM06/Chap6.html#ScalerTypeNote>
                b"true" => FontFaceSourceFormatKeyword::Truetype,
                _ => FontFaceSourceFormatKeyword::None,
            }
        }

        // Satisfy rustc's mutability linting with woff feature both enabled/disabled
        #[cfg(feature = "woff")]
        let mut bytes = bytes;

        match self.format {
            #[cfg(feature = "woff")]
            FontFaceSourceFormatKeyword::Woff => {
                #[cfg(feature = "tracing")]
                tracing::info!("Decompressing woff1 font");

                // Use wuff crate to decompress font
                match wuff::decompress_woff1(&bytes) {
                    Ok(decompressed) => bytes = Bytes::from(decompressed),
                    Err(error) => {
                        // A compressed source whose bytes do not decompress
                        // is a failed source, not a font that keeps its
                        // compressed bytes as data
                        // (<https://drafts.csswg.org/css-fonts-4/#src-desc>).
                        return Err(format!("woff1 decompression failed: {error}"));
                    }
                }
            }
            #[cfg(feature = "woff")]
            FontFaceSourceFormatKeyword::Woff2 => {
                #[cfg(feature = "tracing")]
                tracing::info!("Decompressing woff2 font");

                // Use wuff crate to decompress font
                match wuff::decompress_woff2(&bytes) {
                    Ok(decompressed) => bytes = Bytes::from(decompressed),
                    Err(error) => {
                        return Err(format!("woff2 decompression failed: {error}"));
                    }
                }
            }
            FontFaceSourceFormatKeyword::None => {
                // Bytes that carry no known font magic register no face, so
                // the source failed and the list moves on
                // (<https://drafts.csswg.org/css-fonts-4/#src-desc>).
                return Err("no supported font signature found".to_string());
            }
            _ => {}
        }

        Ok(Resource::Font(bytes, std::mem::take(&mut self.overrides)))
    }
}

/// One `@font-face` source at an index in the rule's list that can start a
/// fetch. A `local()` family is not here: fontique offers no way to make a
/// family the collection already holds become the source of another family
/// name, so a `local()` entry is a failed source and the walk falls through
/// to the next entry (the gap is recorded in the embedder's
/// `docs/upstream.md`; <https://drafts.csswg.org/css-fonts-4/#src-desc>).
pub(crate) enum FontFaceSourceEntry {
    Url(Url, FontFaceSourceFormatKeyword),
}

/// Reads `rule`'s `src` list at `index`, or `None` when that entry cannot
/// name a font: an unsupported format, an unresolvable url, or no entry at
/// that index. The document thread skips `None` entries and moves the index
/// forward (<https://drafts.csswg.org/css-fonts-4/#src-desc>).
pub(crate) fn font_face_source(
    rule: &ServoArc<Locked<FontFaceRule>>,
    guard: &SharedRwLockReadGuard,
    index: usize,
) -> Option<FontFaceSourceEntry> {
    let descriptor = &rule.read_with(guard).descriptors;
    let source = descriptor.src.as_ref()?.0.get(index)?;
    let url_source = match source {
        Source::Local(_) => return None,
        Source::Url(url_source) => url_source,
    };
    // A relative url with no base url to resolve against
    // yields None; skip the source instead of panicking
    let url = url_source.url.url()?;

    let format = match &url_source.format_hint {
        Some(FontFaceSourceFormat::Keyword(keyword)) => *keyword,
        Some(FontFaceSourceFormat::String(string)) => font_format_from_string(string),
        None => font_format_from_extension(url),
    };

    let supported = match format {
        FontFaceSourceFormatKeyword::None
        | FontFaceSourceFormatKeyword::Collection
        | FontFaceSourceFormatKeyword::Opentype
        | FontFaceSourceFormatKeyword::Truetype => true,
        FontFaceSourceFormatKeyword::Woff | FontFaceSourceFormatKeyword::Woff2 => {
            cfg!(feature = "woff")
        }
        FontFaceSourceFormatKeyword::EmbeddedOpentype
        | FontFaceSourceFormatKeyword::Svg
        | FontFaceSourceFormatKeyword::Unknown => false,
    };
    if !supported {
        #[cfg(feature = "tracing")]
        tracing::warn!("Skipping unsupported font of type {:?}", format);
        return None;
    }

    Some(FontFaceSourceEntry::Url(url.as_ref().clone(), format))
}

/// The `@font-face` descriptors parley registers the rule's bytes under.
/// `None` when the rule declares no family to alias the bytes as.
pub(crate) fn font_face_overrides(
    rule: &ServoArc<Locked<FontFaceRule>>,
    guard: &SharedRwLockReadGuard,
) -> Option<FontFaceOverrides> {
    let descriptor = &rule.read_with(guard).descriptors;
    let family = descriptor.font_family.as_ref()?;
    // Capture the @font-face descriptors so parley can register
    // the font under the CSS-declared family name (and weight /
    // style) rather than whatever metadata the TTF reports.
    Some(FontFaceOverrides {
        family_name: Some(family.name.to_string()),
        weight: descriptor
            .font_weight
            .as_ref()
            .and_then(|range| range.0.compute().map(|w| w.value())),
        style: descriptor.font_style.as_ref().map(stylo_to_fontique_style),
        width: descriptor.font_width.as_ref().and_then(|range| {
            range
                .compute()
                .map(|w| parley::fontique::FontWidth::from_percentage(w.0.0.to_float()))
        }),
    })
}

/// Starts the fetch of one `@font-face` source. The response names `key`, so
/// the document thread can advance the rule's source list when it fails
/// (<https://drafts.csswg.org/css-fonts-4/#src-desc>).
#[allow(clippy::too_many_arguments)]
pub(crate) fn fetch_font_face_source(
    key: FontFaceLoadKey,
    url: Url,
    format: FontFaceSourceFormatKeyword,
    overrides: FontFaceOverrides,
    tx: Sender<DocumentEvent>,
    doc_id: usize,
    node_id: Option<NodeId>,
    network_provider: &Arc<dyn NetProvider>,
    shell_provider: &Arc<dyn ShellProvider>,
    abort_signal: Option<&AbortSignal>,
) {
    network_provider.fetch(
        doc_id,
        stamped_request(url, abort_signal)
            .kind(RequestKind::Font)
            .style_sheet(key.sheet_url.clone()),
        ResourceHandler::boxed(
            tx,
            doc_id,
            node_id,
            shell_provider.clone(),
            FontFaceHandler {
                key,
                format,
                overrides,
            },
        ),
    );
}

/// Map a `format("...")` string hint to a format. Besides the keywords, this
/// accepts the legacy `-variations` strings and the `ttf`/`otf` extensions.
fn font_format_from_string(format: &str) -> FontFaceSourceFormatKeyword {
    const FORMATS: &[(&str, FontFaceSourceFormatKeyword)] = &[
        ("collection", FontFaceSourceFormatKeyword::Collection),
        (
            "embedded-opentype",
            FontFaceSourceFormatKeyword::EmbeddedOpentype,
        ),
        ("opentype", FontFaceSourceFormatKeyword::Opentype),
        ("opentype-variations", FontFaceSourceFormatKeyword::Opentype),
        ("otf", FontFaceSourceFormatKeyword::Opentype),
        ("svg", FontFaceSourceFormatKeyword::Svg),
        ("truetype", FontFaceSourceFormatKeyword::Truetype),
        ("truetype-variations", FontFaceSourceFormatKeyword::Truetype),
        ("ttf", FontFaceSourceFormatKeyword::Truetype),
        ("woff", FontFaceSourceFormatKeyword::Woff),
        ("woff-variations", FontFaceSourceFormatKeyword::Woff),
        ("woff2", FontFaceSourceFormatKeyword::Woff2),
        ("woff2-variations", FontFaceSourceFormatKeyword::Woff2),
    ];
    FORMATS
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(format))
        .map_or(FontFaceSourceFormatKeyword::Unknown, |(_, format)| *format)
}

/// Guess a font's format from the file extension of its url's path. Urls
/// with no known extension, such as `data:` urls, get
/// `FontFaceSourceFormatKeyword::None` and are sniffed from their bytes once
/// fetched.
fn font_format_from_extension(url: &Url) -> FontFaceSourceFormatKeyword {
    const EXTENSIONS: &[(&str, FontFaceSourceFormatKeyword)] = &[
        ("eot", FontFaceSourceFormatKeyword::EmbeddedOpentype),
        ("otf", FontFaceSourceFormatKeyword::Opentype),
        ("svg", FontFaceSourceFormatKeyword::Svg),
        ("ttc", FontFaceSourceFormatKeyword::Collection),
        ("ttf", FontFaceSourceFormatKeyword::Truetype),
        ("woff", FontFaceSourceFormatKeyword::Woff),
        ("woff2", FontFaceSourceFormatKeyword::Woff2),
    ];
    if url.cannot_be_a_base() {
        return FontFaceSourceFormatKeyword::None;
    }
    let file_name = url.path().rsplit('/').next().unwrap_or_default();
    let Some((_, extension)) = file_name.rsplit_once('.') else {
        return FontFaceSourceFormatKeyword::None;
    };
    EXTENSIONS
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(extension))
        .map_or(FontFaceSourceFormatKeyword::None, |(_, format)| *format)
}

/// Translate stylo's `@font-face` `font-style` descriptor into the fontique
/// `FontStyle` enum used by parley. Stylo encodes Italic and Oblique-with-
/// angle distinctly; CSS's bare `normal` is parsed as `Oblique(0deg, 0deg)`
/// by stylo (see the `FontStyle::parse` impl in stylo's `font_face.rs`), so
/// that pattern is treated as `Normal` here.
fn stylo_to_fontique_style(style: &FontStyleRange) -> parley::fontique::FontStyle {
    use parley::fontique::FontStyle as Fq;
    match style {
        FontStyleRange::Italic => Fq::Italic,
        FontStyleRange::Oblique(min, max) => {
            let angle = min.degrees();
            // Stylo emits `Oblique(0deg, 0deg)` for the literal CSS `normal`
            // keyword. Map that back to `Normal` so parley's font matching
            // doesn't misclassify upright fonts.
            if angle.is_none_or(|a| a == 0.0) && max.degrees().is_none_or(|a| a == 0.0) {
                Fq::Normal
            } else {
                Fq::Oblique(angle)
            }
        }
    }
}

/// Handles HTML fetched for an `<iframe>` element's `src`
pub(crate) struct DocumentSrcHandler;

impl NetHandler for ResourceHandler<DocumentSrcHandler> {
    fn bytes(self: Box<Self>, resolved_url: String, bytes: Bytes) {
        let html = String::from_utf8_lossy(&bytes).into_owned();
        self.respond(
            Some(resolved_url),
            None,
            false,
            None,
            Ok(Resource::DocumentSrc(html)),
        );
    }
}

pub struct ImageHandler {
    kind: ImageType,
    /// The key the request was filed under, which `pending_images` is keyed
    /// by. The provider hands back the post-redirect URL, so applying the
    /// decoded image under it would never match the request.
    key: crate::document::ImageKey,
}
impl ImageHandler {
    pub fn new(kind: ImageType, key: crate::document::ImageKey) -> Self {
        Self { kind, key }
    }
}

impl NetHandler for ResourceHandler<ImageHandler> {
    /// A provider that only reports bytes cannot say how the response was
    /// tainted, so the delivery is marked tainted: canvas must not read pixels
    /// from a request whose origin-cleanliness is unknown
    /// (<https://html.spec.whatwg.org/multipage/canvas.html#the-image-argument>).
    /// A provider that knows the taint calls [`NetHandler::response`].
    fn bytes(self: Box<Self>, resolved_url: String, bytes: Bytes) {
        self.response(
            resolved_url,
            bytes,
            blitz_traits::net::ResponseInfo {
                origin_clean: false,
            },
        );
    }

    fn response(
        self: Box<Self>,
        resolved_url: String,
        bytes: Bytes,
        info: blitz_traits::net::ResponseInfo,
    ) {
        let result = self.data.parse(bytes);
        self.respond(
            Some(resolved_url),
            Some(self.data.key.clone()),
            info.origin_clean,
            None,
            result,
        );
    }
}

impl ImageHandler {
    fn parse(&self, bytes: Bytes) -> Result<Resource, String> {
        let image_err = match image::ImageReader::new(Cursor::new(&bytes))
            .with_guessed_format()
            .expect("IO errors impossible with Cursor")
            .decode()
        {
            Ok(image) => {
                let width = image.width();
                let height = image.height();
                let raw_rgba8_data = image.into_rgba8().into_raw();
                return Ok(Resource::Image(
                    self.kind,
                    width,
                    height,
                    Arc::new(raw_rgba8_data),
                ));
            }
            Err(e) => e.to_string(),
        };

        #[cfg(feature = "svg")]
        let svg_err = {
            use crate::util::parse_svg;
            match parse_svg(&bytes, None) {
                Ok(svg) => return Ok(Resource::Svg(self.kind, svg)),
                Err(e) => e.to_string(),
            }
        };
        #[cfg(not(feature = "svg"))]
        let svg_err = "svg feature disabled";

        Err(format!(
            "Could not parse image ({} bytes): image-crate error: {image_err}; svg fallback error: {svg_err}",
            bytes.len()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parley::fontique::FontStyle as Fq;
    use style::stylesheets::{AllowImportRules, CssRule, StylesheetInDocument};
    use style::values::specified::Angle;

    fn oblique(min_deg: f32, max_deg: f32) -> FontStyleRange {
        FontStyleRange::Oblique(Angle::from_degrees(min_deg), Angle::from_degrees(max_deg))
    }

    /// The source the walk resolves at `index` for each `@font-face` rule in
    /// `css`, spelled for assertion: a url as its url and format, a
    /// `local()` family as `local(name)`, and an unusable source as `None`.
    fn sources_at(css: &str, index: usize) -> Vec<Option<(String, FontFaceSourceFormatKeyword)>> {
        let lock = SharedRwLock::new();
        let base_url = Url::parse("https://example.com/css/style.css").unwrap();
        let sheet = Stylesheet::from_str(
            css,
            UrlExtraData(ServoArc::new(base_url)),
            Origin::Author,
            ServoArc::new(lock.wrap(MediaList::empty())),
            lock.clone(),
            None,
            None,
            QuirksMode::NoQuirks,
            AllowImportRules::No,
        );
        let guard = lock.read();
        sheet
            .contents(&guard)
            .rules(&guard)
            .iter()
            .filter_map(|rule| match rule {
                CssRule::FontFace(font_face) => Some(font_face.clone()),
                _ => None,
            })
            .map(|rule| match font_face_source(&rule, &guard, index) {
                Some(FontFaceSourceEntry::Url(url, format)) => Some((url.to_string(), format)),
                None => None,
            })
            .collect()
    }

    const GLYPHICONS: &str = "@font-face {
        font-family: 'Glyphicons Halflings';
        src: url('../fonts/glyphicons-halflings-regular.eot');
        src: url('../fonts/glyphicons-halflings-regular.eot?#iefix') format('embedded-opentype'),
            url('../fonts/glyphicons-halflings-regular.woff2') format('woff2'),
            url('../fonts/glyphicons-halflings-regular.woff') format('woff'),
            url('../fonts/glyphicons-halflings-regular.ttf') format('truetype'),
            url('../fonts/glyphicons-halflings-regular.svg#glyphicons_halflingsregular') format('svg');
    }";

    #[test]
    fn bootstrap_glyphicons_skip_eot_source() {
        // The second `src` declaration wins; its first entry names an
        // embedded-opentype url, which cannot start a fetch. The walk moves
        // to the first source that can
        // (<https://drafts.csswg.org/css-fonts-4/#src-desc>).
        assert_eq!(
            sources_at(GLYPHICONS, 0),
            [None],
            "the eot?#iefix source is unsupported"
        );
        if cfg!(feature = "woff") {
            assert_eq!(
                sources_at(GLYPHICONS, 1),
                [Some((
                    "https://example.com/fonts/glyphicons-halflings-regular.woff2".to_string(),
                    FontFaceSourceFormatKeyword::Woff2,
                ))]
            );
        } else {
            assert_eq!(sources_at(GLYPHICONS, 1), [None], "woff2 needs the feature");
            assert_eq!(sources_at(GLYPHICONS, 2), [None], "woff needs the feature");
            assert_eq!(
                sources_at(GLYPHICONS, 3),
                [Some((
                    "https://example.com/fonts/glyphicons-halflings-regular.ttf".to_string(),
                    FontFaceSourceFormatKeyword::Truetype,
                ))]
            );
        }
    }

    #[test]
    fn format_strings_map_to_formats() {
        use FontFaceSourceFormatKeyword as Kw;
        for (string, format) in [
            ("collection", Kw::Collection),
            ("embedded-opentype", Kw::EmbeddedOpentype),
            ("opentype", Kw::Opentype),
            ("OpenType", Kw::Opentype),
            ("opentype-variations", Kw::Opentype),
            ("otf", Kw::Opentype),
            ("svg", Kw::Svg),
            ("truetype", Kw::Truetype),
            ("truetype-variations", Kw::Truetype),
            ("ttf", Kw::Truetype),
            ("woff", Kw::Woff),
            ("woff-variations", Kw::Woff),
            ("woff2", Kw::Woff2),
            ("woff2-variations", Kw::Woff2),
            ("x-unknown", Kw::Unknown),
        ] {
            assert_eq!(font_format_from_string(string), format, "{string}");
        }
    }

    #[test]
    fn extension_ignores_query_and_fragment() {
        use FontFaceSourceFormatKeyword as Kw;
        for (url, format) in [
            ("https://example.com/a.eot?#iefix", Kw::EmbeddedOpentype),
            ("https://example.com/a.svg#font", Kw::Svg),
            ("https://example.com/a.TTF?v=4.7.0", Kw::Truetype),
            ("https://example.com/a.otf", Kw::Opentype),
            ("https://example.com/a.woff2?v=1#x", Kw::Woff2),
            ("https://example.com/fonts.v2/font", Kw::None),
            ("https://example.com/font?name=a.ttf", Kw::None),
            ("data:font/ttf;base64,AAEAAA.ttf", Kw::None),
        ] {
            let url = Url::parse(url).unwrap();
            assert_eq!(font_format_from_extension(&url), format, "{url}");
        }
    }

    #[test]
    fn unsupported_sources_are_skipped() {
        use FontFaceSourceFormatKeyword as Kw;
        let css = "@font-face { font-family: a; src: url(a.ttf) format('x-unknown'), url(a.otf); }
            @font-face { font-family: b; src: url(b.svg#b), url(b.eot), url(b.ttf?v=1#x); }
            @font-face { font-family: c; src: url(c.eot) format(embedded-opentype); }
            @font-face { font-family: d; src: url(d.eot) format(truetype); }";
        assert_eq!(
            sources_at(css, 0),
            [
                None,
                None,
                None,
                Some(("https://example.com/css/d.eot".to_string(), Kw::Truetype)),
            ],
            "only rule d's first source can start a fetch"
        );
        assert_eq!(
            sources_at(css, 1),
            [
                Some(("https://example.com/css/a.otf".to_string(), Kw::Opentype)),
                None,
                None,
                None,
            ],
        );
        assert_eq!(
            sources_at(css, 2),
            [
                None,
                Some((
                    "https://example.com/css/b.ttf?v=1#x".to_string(),
                    Kw::Truetype
                )),
                None,
                None,
            ],
        );
    }

    #[test]
    fn a_local_source_is_a_failed_source() {
        // fontique cannot make a family the collection already holds become
        // the source of another family name, so a local() entry cannot start
        // a fetch (<https://drafts.csswg.org/css-fonts-4/#src-desc>).
        assert_eq!(
            sources_at(
                "@font-face { font-family: x; src: url(a.ttf), local(My Face), url(b.ttf) }",
                1
            ),
            [None],
        );
    }

    #[test]
    fn italic_maps_to_italic() {
        assert_eq!(stylo_to_fontique_style(&FontStyleRange::Italic), Fq::Italic,);
    }

    #[test]
    fn oblique_zero_zero_maps_to_normal() {
        // Stylo parses bare CSS `normal` as `Oblique(0deg, 0deg)`; the
        // helper must round-trip that back to `FontStyle::Normal` so
        // parley's matching doesn't misclassify upright fonts.
        assert_eq!(stylo_to_fontique_style(&oblique(0.0, 0.0)), Fq::Normal);
    }

    #[test]
    fn oblique_single_angle_maps_to_oblique_with_min() {
        assert_eq!(
            stylo_to_fontique_style(&oblique(14.0, 14.0)),
            Fq::Oblique(Some(14.0)),
        );
    }

    #[test]
    fn oblique_range_uses_min_angle() {
        // For a range, fontique's single-angle representation takes the
        // lower bound — confirm `min` (not `max`) is what gets through.
        assert_eq!(
            stylo_to_fontique_style(&oblique(10.0, 20.0)),
            Fq::Oblique(Some(10.0)),
        );
    }
}
