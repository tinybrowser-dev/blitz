//! `@font-face` load outcomes.
//!
//! Blitz advances each `@font-face` rule's `src` list itself: a source whose
//! bytes yield no face moves the list to the next source, and a list that
//! runs out is an error (<https://drafts.csswg.org/css-fonts-4/#src-desc>).
//! Page script only learns the outcome through the `FontFace` status and the
//! `FontFaceSet` events, which are the embedder's to fire, so this module
//! records every terminal transition and the embedder drains it
//! (<https://drafts.csswg.org/css-font-loading/#dom-fontface-status>).

use style::font_face::FontFaceRule;
use style::servo_arc::Arc as ServoArc;
use style::shared_lock::Locked;

/// One settled `@font-face` rule.
#[derive(Debug, Clone)]
pub struct PendingFontFaceEvent {
    /// The rule the outcome belongs to, by identity: the same `Arc` the
    /// document's sheets hold.
    pub rule: ServoArc<Locked<FontFaceRule>>,
    pub status: FontFaceLoadStatus,
    /// The final URL of the source that settled the rule, when the settling
    /// source had one.
    pub url: Option<String>,
}

/// The terminal state of one rule's source list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontFaceLoadStatus {
    /// At least one face registered from one of the rule's sources.
    Loaded,
    /// Every source failed: the rule names no usable font
    /// (<https://drafts.csswg.org/css-fonts-4/#src-desc>).
    Error,
}

impl crate::document::BaseDocument {
    /// Take the `@font-face` rules whose source list settled and empty the
    /// list. A rule is reported once, at the transition that settled it
    /// (<https://drafts.csswg.org/css-font-loading/#dom-fontface-status>).
    pub fn take_pending_font_face_events(&mut self) -> Vec<PendingFontFaceEvent> {
        std::mem::take(&mut self.pending_font_face_events)
    }
}
