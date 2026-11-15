//! Scrolling: user-initiated (interactive) and programmatic scrolling of nodes and the
//! viewport, and the scroll animations (smooth scrolls and flings) which drive them.

use std::collections::HashMap;

use blitz_traits::events::{BlitzScrollEvent, DomEvent, DomEventData};
use blitz_traits::node_id::NodeId;
use markup5ever::local_name;
use style::logical_geometry::{PhysicalSide, WritingMode};
use style::properties::generated::longhands::position::computed_value::T as Position;
use style::values::computed::Overflow;
use stylo_taffy::CopyStyleGetters;
use web_time::{SystemTime, UNIX_EPOCH};

use crate::BaseDocument;
use crate::util::Point;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScrollBehavior {
    #[default]
    Auto,
    Instant,
    Smooth,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollLogicalPosition {
    Start,
    Center,
    End,
    Nearest,
}

/// How far up the containing block chain a `scrollIntoView` call scrolls
/// (<https://drafts.csswg.org/cssom-view/#dom-scrollintoviewoptions-container>).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollIntoViewContainer {
    /// Scroll every ancestor scrolling box, then the viewport.
    All,
    /// Stop after the nearest ancestor scrolling box, if there is one.
    Nearest,
}

/// What a scroll applies to.
///
/// Per the CSS overflow propagation rules the root element has no scrolling mechanism of its
/// own (its overflow is applied to the viewport), so [`ScrollTarget::Node`] holding the root
/// element is equivalent to [`ScrollTarget::Viewport`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ScrollTarget {
    Node(NodeId),
    Viewport,
}

/// The scrolling box a queued scroll event belongs to
/// (<https://drafts.csswg.org/cssom-view/#scrolling-events>).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollEventTarget {
    /// The viewport, whose events fire at its Document.
    Viewport,
    /// An element's own scrolling box, whose events fire at that element.
    Element(NodeId),
}

/// Which of the two events a queued entry names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollEventKind {
    Scroll,
    ScrollEnd,
}

/// One entry of a document's pending scroll events list. `scroll` entries come
/// before the `scrollend` entries of the same run of scroll steps
/// (<https://drafts.csswg.org/cssom-view/#document-pending-scroll-events>).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingScrollEvent {
    pub target: ScrollEventTarget,
    pub kind: ScrollEventKind,
}

/// The event target for a scroll of `target`.
fn scroll_event_target(target: ScrollTarget) -> ScrollEventTarget {
    match target {
        ScrollTarget::Viewport => ScrollEventTarget::Viewport,
        ScrollTarget::Node(node) => ScrollEventTarget::Element(node),
    }
}

/// Whether an axis with this computed overflow has a scrolling box.
///
/// `visible` and `clip` have none; `hidden` does for programmatic scrolls
/// (<https://drafts.csswg.org/cssom-view/#scrolling-box>).
fn scrollable_overflow(overflow: Overflow) -> bool {
    !matches!(overflow, Overflow::Visible | Overflow::Clip)
}

/// Whether a node establishes a scrolling box of its own. The root element's
/// overflow belongs to the viewport, so the caller handles it separately.
pub(crate) fn establishes_scrolling_box(node: &crate::Node) -> bool {
    if !node.has_boxes() || node.is_non_atomic_inline() {
        return false;
    }
    let Some(styles) = node.primary_styles() else {
        return false;
    };
    scrollable_overflow(styles.clone_overflow_x()) || scrollable_overflow(styles.clone_overflow_y())
}

/// Whether `node` is fixed to the viewport, so scrolling the viewport does not
/// move it
/// (<https://drafts.csswg.org/cssom-view/#dom-element-scrollintoview>).
///
/// Blitz hoists a `position: fixed` box to its fixed-position containing block,
/// which is the root element when no ancestor establishes one, so a fixed box
/// whose containing block is the root is viewport-fixed.
fn fixed_to_viewport(node: &crate::Node, root: Option<NodeId>) -> bool {
    let Some(root) = root else {
        return false;
    };
    node.primary_styles()
        .is_some_and(|styles| styles.clone_position() == Position::Fixed)
        && node
            .containing_block()
            .is_none_or(|containing_block| containing_block == root)
}

/// One logical axis of a writing mode mapped into physical coordinates.
///
/// `reversed` means the logical start is the physical max side (right for
/// `vertical-rl`/`sideways-rl` and RTL inline axes, bottom for a reversed
/// inline axis of a vertical mode), so "start" alignment moves the physical
/// end edges.
#[derive(Clone, Copy)]
struct LogicalAxis {
    horizontal: bool,
    reversed: bool,
}

impl LogicalAxis {
    fn block(writing_mode: WritingMode) -> Self {
        Self {
            horizontal: writing_mode.is_vertical(),
            reversed: matches!(
                writing_mode.block_start_physical_side(),
                PhysicalSide::Right | PhysicalSide::Bottom
            ),
        }
    }

    fn inline(writing_mode: WritingMode) -> Self {
        Self {
            horizontal: !writing_mode.is_vertical(),
            reversed: matches!(
                writing_mode.inline_start_physical_side(),
                PhysicalSide::Right | PhysicalSide::Bottom
            ),
        }
    }

    /// The physical alignment this logical position means on the axis
    /// (<https://drafts.csswg.org/cssom-view/#determine-the-scroll-into-view-position>).
    fn position(self, position: ScrollLogicalPosition) -> ScrollLogicalPosition {
        if !self.reversed {
            return position;
        }
        match position {
            ScrollLogicalPosition::Start => ScrollLogicalPosition::End,
            ScrollLogicalPosition::End => ScrollLogicalPosition::Start,
            ScrollLogicalPosition::Center | ScrollLogicalPosition::Nearest => position,
        }
    }
}

/// How far to scroll.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ScrollAmount {
    /// An absolute scroll offset.
    To(Point<f64>),
    /// A delta to apply to the current scroll offset.
    By(Point<f64>),
}

/// What to do with scroll which the target cannot consume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScrollOverflow {
    /// Discard it: the scroll affects exactly one scroller.
    Clamp,
    /// Transfer it to the next scroller in the scroll chain (the parent node, and eventually
    /// the viewport).
    Chain,
}

/// Who initiated a scroll. Determines which overflow values count as scrollable:
/// `overflow: hidden` boxes have a scrolling box which programmatic scrolls apply to,
/// but no user scrolling mechanism.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScrollSource {
    /// A user-initiated scroll (wheel, touch, scrollbar drag, keyboard).
    User,
    /// A script-initiated scroll (`scrollTo` and friends, fragment navigation).
    Programmatic,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ScrollRequest {
    pub(crate) target: ScrollTarget,
    pub(crate) amount: ScrollAmount,
    pub(crate) overflow: ScrollOverflow,
    pub(crate) source: ScrollSource,
    pub(crate) behavior: ScrollBehavior,
    /// Whether to abort a smooth scroll in progress. Set for user-initiated scrolls, so that
    /// an animation does not fight the user's input for the rest of its duration.
    pub(crate) interrupt_animation: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FlingState {
    pub(crate) target: NodeId,
    pub(crate) last_seen_time: f64,
    pub(crate) x_velocity: f64,
    pub(crate) y_velocity: f64,
}

/// State driving a smooth (animated) scroll towards a target offset. Used for
/// fragment navigation (`#anchor` links) and programmatic smooth scrolling.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ScrollToState {
    /// The scroll offset at the start of the animation.
    pub(crate) start: Point<f64>,
    /// The scroll offset to animate towards.
    pub(crate) end: Point<f64>,
    /// The animation timeline (in milliseconds) at which the scroll started.
    pub(crate) start_time: f64,
    /// Total duration of the animation in milliseconds.
    pub(crate) duration: f64,
}

/// The scrolling animations a document is running.
///
/// A smooth scroll belongs to the scrolling box it moves, and separate boxes
/// animate independently: `scrollIntoView` on a nested scroller and the
/// viewport it finally reaches are two targets, and two `scrollIntoView` calls
/// on unrelated scrollers keep both animations
/// (<https://drafts.csswg.org/cssom-view/#scroll-an-element>). A fling is one
/// gesture chained through whichever scrollers lie under the pointer, so it has
/// a single slot of its own.
#[derive(Debug, Clone, Default)]
pub(crate) struct ScrollAnimations {
    /// One in-flight smooth scroll per scrolling box.
    smooth: HashMap<ScrollTarget, ScrollToState>,
    /// The fling a touch pan produced, if it is still moving.
    pub(crate) fling: Option<FlingState>,
}

impl ScrollAnimations {
    /// Whether any scroll still has to advance.
    pub(crate) fn is_animating(&self) -> bool {
        self.fling.is_some() || !self.smooth.is_empty()
    }

    /// Abort the smooth scroll of `target`, if it has one.
    pub(crate) fn abort_smooth(&mut self, target: ScrollTarget) {
        self.smooth.remove(&target);
    }

    /// Abort every scroll animation.
    pub(crate) fn clear(&mut self) {
        self.smooth.clear();
        self.fling = None;
    }
}

/// Cubic ease-in-out easing function, mapping a normalised time `t` in `[0, 1]`
/// to an eased progress value in `[0, 1]`. Used to give smooth scrolls a natural
/// acceleration/deceleration curve.
fn ease_in_out_cubic(t: f64) -> f64 {
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        let f = 2.0 * t - 2.0;
        1.0 + (f * f * f) / 2.0
    }
}

impl BaseDocument {
    /// Apply a scroll to the document, returning whether anything moved.
    ///
    /// This is the single scrolling primitive: user-initiated scrolls
    /// ([`BaseDocument::scroll_chain_by`]) and programmatic scrolls
    /// ([`BaseDocument::scroll_to`]) differ only in the [`ScrollRequest`] they build.
    pub(crate) fn scroll(
        &mut self,
        request: ScrollRequest,
        dispatch_event: &mut dyn FnMut(DomEvent),
    ) -> bool {
        let target = self.canonical_scroll_target(request.target);

        // A user-initiated scroll takes over the box it moves: a smooth scroll
        // already running there would fight it for the rest of its duration
        // (<https://drafts.csswg.org/cssom-view/#scrolling-box>).
        if request.interrupt_animation {
            self.scroll_animations.abort_smooth(target);
        }

        // Text inputs and sub-documents scroll their own content rather than an overflow
        // scrollport, so they only take part in the chained (user-initiated) path.
        if let (ScrollTarget::Node(node_id), ScrollAmount::By(delta), ScrollOverflow::Chain) =
            (target, request.amount, request.overflow)
        {
            if let Some(has_changed) =
                self.scroll_inner_content_by(node_id, delta, request, dispatch_event)
            {
                return has_changed;
            }
        }

        let include_hidden = request.source == ScrollSource::Programmatic;
        let (current, min, max) = self.scroll_state(target, include_hidden);
        let unclamped = match request.amount {
            ScrollAmount::To(to) => to,
            ScrollAmount::By(by) => Point {
                x: current.x + by.x,
                y: current.y + by.y,
            },
        };
        let end = Point {
            x: unclamped.x.clamp(min.x, max.x),
            y: unclamped.y.clamp(min.y, max.y),
        };

        if self.should_scroll_smoothly(target, request.behavior) {
            // A smooth scroll that cannot move the box performs no scroll and
            // fires no events; only an ongoing animation replaces itself
            // (<https://drafts.csswg.org/cssom-view/#scroll-an-element>, "If
            // position is the same as box's current scroll position").
            if end != current || self.scroll_animations.smooth.contains_key(&target) {
                self.start_scroll_animation(target, end);
            }
            return end != current;
        }

        let has_changed = self.write_scroll_offset(target, end, dispatch_event);

        // An instant scroll completes here. `write_scroll_offset` queued the
        // `scroll` entry, and the scroll steps fire `scrollend` after it. A
        // user scroll's momentum is not modelled, so only a programmatic one
        // has an end to report
        // (<https://drafts.csswg.org/cssom-view/#scrolling-events>).
        if has_changed && request.source == ScrollSource::Programmatic {
            self.note_scroll_ended(scroll_event_target(target));
        }

        // Transfer the delta the target could not consume to the next scroller in the chain.
        if request.overflow == ScrollOverflow::Chain {
            let remainder = Point {
                x: unclamped.x - end.x,
                y: unclamped.y - end.y,
            };
            if remainder != Point::ZERO {
                if let Some(next) = self.next_scroller_in_chain(target) {
                    let request = ScrollRequest {
                        target: next,
                        amount: ScrollAmount::By(remainder),
                        ..request
                    };
                    return has_changed | self.scroll(request, dispatch_event);
                }
            }
        }

        has_changed
    }

    /// Scroll a node which scrolls content of its own rather than an overflow scrollport
    /// (a text input or a sub-document), returning `None` if the node is not such a node.
    fn scroll_inner_content_by(
        &mut self,
        node_id: NodeId,
        delta: Point<f64>,
        request: ScrollRequest,
        dispatch_event: &mut dyn FnMut(DomEvent),
    ) -> Option<bool> {
        let node = self.nodes.get_mut(node_id)?;

        if let Some(mut sub_doc) = node.subdoc_mut().map(|doc| doc.inner_mut()) {
            let target = sub_doc
                .get_hover_node_id()
                .map_or(ScrollTarget::Viewport, ScrollTarget::Node);
            // TODO: propagate the remaining scroll to the outer document
            let request = ScrollRequest {
                target,
                amount: ScrollAmount::By(delta),
                ..request
            };
            return Some(sub_doc.scroll(request, dispatch_event));
        }

        // Single-line inputs scroll their text horizontally, multi-line inputs vertically.
        node.element_data()?.text_input_data()?;
        let content_box_width = node.final_layout().content_box_width();
        let content_box_height = node.final_layout().content_box_height();
        let input = node
            .element_data_mut()
            .and_then(|el| el.text_input_data_mut())
            .unwrap();

        // `TextInputData::scroll_by` takes (and returns the unconsumed part of) a delta in
        // the opposite direction to a scroll offset.
        let mut remainder = delta;
        if input.is_multiline {
            remainder.y =
                -(input.scroll_by(-delta.y as f32, content_box_width, content_box_height) as f64);
        } else {
            remainder.x =
                -(input.scroll_by(-delta.x as f32, content_box_width, content_box_height) as f64);
        }

        let has_changed = remainder != delta;

        if remainder != Point::ZERO {
            if let Some(next) = self.next_scroller_in_chain(ScrollTarget::Node(node_id)) {
                let request = ScrollRequest {
                    target: next,
                    amount: ScrollAmount::By(remainder),
                    ..request
                };
                return Some(has_changed | self.scroll(request, dispatch_event));
            }
        }

        Some(has_changed)
    }

    /// The scroller which unconsumed scroll is transferred to: the node's parent, or the
    /// viewport once the chain runs out of nodes.
    fn next_scroller_in_chain(&self, target: ScrollTarget) -> Option<ScrollTarget> {
        match target {
            ScrollTarget::Viewport => None,
            ScrollTarget::Node(node_id) => Some(
                self.nodes
                    .get(node_id)
                    .and_then(|node| node.parent)
                    .map_or(ScrollTarget::Viewport, ScrollTarget::Node),
            ),
        }
    }

    /// Resolve a scroll target to the scroller which actually moves: the root element scrolls
    /// the viewport, per the CSS overflow propagation rules.
    fn canonical_scroll_target(&self, target: ScrollTarget) -> ScrollTarget {
        match target {
            ScrollTarget::Node(node_id)
                if self.try_root_element().is_some_and(|el| el.id == node_id) =>
            {
                ScrollTarget::Viewport
            }
            target => target,
        }
    }

    /// Note that `target` scrolled, so the next scroll steps fire `scroll` at
    /// it. The list holds one entry per box, however often it moved
    /// (<https://drafts.csswg.org/cssom-view/#scrolling-events>).
    fn note_scrolled(&mut self, target: ScrollEventTarget) {
        if !self.pending_scrolls.contains(&target) {
            self.pending_scrolls.push(target);
        }
    }

    /// Note that `target` completed a scroll operation, so the next scroll
    /// steps fire `scrollend` at it after the `scroll` entries.
    fn note_scroll_ended(&mut self, target: ScrollEventTarget) {
        if !self.pending_scrollends.contains(&target) {
            self.pending_scrollends.push(target);
        }
    }

    /// Take the pending scroll events and empty the list: every `scroll`
    /// entry in the order the boxes moved, then every `scrollend` entry
    /// (<https://drafts.csswg.org/cssom-view/#run-the-scroll-steps>).
    pub fn take_pending_scroll_events(&mut self) -> Vec<PendingScrollEvent> {
        let mut events =
            Vec::with_capacity(self.pending_scrolls.len() + self.pending_scrollends.len());
        for target in std::mem::take(&mut self.pending_scrolls) {
            events.push(PendingScrollEvent {
                target,
                kind: ScrollEventKind::Scroll,
            });
        }
        for target in std::mem::take(&mut self.pending_scrollends) {
            events.push(PendingScrollEvent {
                target,
                kind: ScrollEventKind::ScrollEnd,
            });
        }
        events
    }

    /// Whether a smooth scroll or a fling is still running. The embedder gives
    /// a document with one a rendering opportunity, because a scroll animation
    /// advances at frames, not on the wall clock
    /// (<https://drafts.csswg.org/cssom-view/#scroll-an-element>).
    pub fn has_scroll_animation(&self) -> bool {
        self.scroll_animations.is_animating()
    }

    /// Write a scroll target's offset (which must already be clamped to its scrollable
    /// range), dispatching a `scroll` event and returning whether the offset changed.
    pub(crate) fn write_scroll_offset(
        &mut self,
        target: ScrollTarget,
        offset: Point<f64>,
        dispatch_event: &mut dyn FnMut(DomEvent),
    ) -> bool {
        match self.canonical_scroll_target(target) {
            ScrollTarget::Viewport => {
                let initial = self.viewport_scroll();
                self.set_viewport_scroll(offset);
                if offset == initial {
                    return false;
                }

                if let Some(root) = self.try_root_element() {
                    let root_id = root.id;
                    let layout = *root.final_layout();
                    let scale = self.viewport.scale() as f64;
                    let event = BlitzScrollEvent {
                        scroll_top: offset.y,
                        scroll_left: offset.x,
                        scroll_width: layout.size.width.max(root.scroll_width()) as i32,
                        scroll_height: layout.size.height.max(root.scroll_height()) as i32,
                        client_width: (self.viewport.window_size.0 as f64 / scale) as i32,
                        client_height: (self.viewport.window_size.1 as f64 / scale) as i32,
                    };
                    dispatch_event(DomEvent::new(root_id, DomEventData::Scroll(event)));
                }

                self.note_scrolled(ScrollEventTarget::Viewport);
                self.request_redraw();
                true
            }
            ScrollTarget::Node(node_id) => {
                let Some(node) = self.nodes.get_mut(node_id) else {
                    return false;
                };

                let initial = *node.scroll_offset();
                *node.scroll_offset_mut() = offset;
                if offset == initial {
                    return false;
                }

                let scroll_width = node.scroll_width();
                let scroll_height = node.scroll_height();
                let layout = *node.final_layout();
                self.nodes.bump_geometry_generation();
                let event = BlitzScrollEvent {
                    scroll_top: offset.y,
                    scroll_left: offset.x,
                    scroll_width: scroll_width as i32,
                    scroll_height: scroll_height as i32,
                    client_width: layout.size.width as i32,
                    client_height: layout.size.height as i32,
                };
                dispatch_event(DomEvent::new(node_id, DomEventData::Scroll(event)));

                self.note_scrolled(ScrollEventTarget::Element(node_id));
                self.show_scrollbars(node_id);
                self.request_redraw();
                true
            }
        }
    }

    pub fn scroll_node_by<F: FnMut(DomEvent)>(
        &mut self,
        node_id: NodeId,
        x: f64,
        y: f64,
        dispatch_event: F,
    ) {
        self.scroll_node_by_has_changed(node_id, x, y, dispatch_event);
    }

    /// Scroll a node by given x and y
    /// Will bubble scrolling up to parent node once it can no longer scroll further
    /// If we're already at the root node, bubbles scrolling up to the viewport
    pub fn scroll_node_by_has_changed<F: FnMut(DomEvent)>(
        &mut self,
        node_id: NodeId,
        x: f64,
        y: f64,
        mut dispatch_event: F,
    ) -> bool {
        self.scroll(
            ScrollRequest {
                target: ScrollTarget::Node(node_id),
                // A user-facing scroll delta moves the content, i.e. the opposite direction
                // to the scroll offset.
                amount: ScrollAmount::By(Point { x: -x, y: -y }),
                overflow: ScrollOverflow::Chain,
                source: ScrollSource::User,
                behavior: ScrollBehavior::Instant,
                interrupt_animation: false,
            },
            &mut dispatch_event,
        )
    }

    pub fn scroll_viewport_by(&mut self, x: f64, y: f64) {
        self.scroll_viewport_by_has_changed(x, y);
    }

    /// Scroll the viewport by the given values
    pub fn scroll_viewport_by_has_changed(&mut self, x: f64, y: f64) -> bool {
        self.scroll(
            ScrollRequest {
                target: ScrollTarget::Viewport,
                amount: ScrollAmount::By(Point { x: -x, y: -y }),
                overflow: ScrollOverflow::Clamp,
                source: ScrollSource::User,
                behavior: ScrollBehavior::Instant,
                interrupt_animation: false,
            },
            &mut |_| {},
        )
    }

    pub(crate) fn scroll_chain_by(
        &mut self,
        anchor_node_id: Option<NodeId>,
        scroll_x: f64,
        scroll_y: f64,
        dispatch_event: &mut dyn FnMut(DomEvent),
    ) -> bool {
        self.scroll(
            ScrollRequest {
                target: anchor_node_id.map_or(ScrollTarget::Viewport, ScrollTarget::Node),
                amount: ScrollAmount::By(Point {
                    x: -scroll_x,
                    y: -scroll_y,
                }),
                overflow: ScrollOverflow::Chain,
                source: ScrollSource::User,
                behavior: ScrollBehavior::Instant,
                // A user-initiated scroll aborts any smooth scroll in progress, so that the
                // two do not fight over the scroll offset for the rest of the animation.
                interrupt_animation: true,
            },
            dispatch_event,
        )
    }

    /// Duration (in milliseconds) of an animated scroll.
    const SMOOTH_SCROLL_DURATION_MS: f64 = 300.0;

    /// Returns the current, minimum and maximum scroll offsets for the given
    /// scroll target. `include_hidden` controls whether
    /// `overflow: hidden` axes count as scrollable (they do for programmatic scrolls,
    /// which apply to any scrolling box, but not for user scrolls).
    pub(crate) fn scroll_state(
        &self,
        target: ScrollTarget,
        include_hidden: bool,
    ) -> (Point<f64>, Point<f64>, Point<f64>) {
        match self.canonical_scroll_target(target) {
            ScrollTarget::Viewport => {
                // The viewport scrolls the root element's scrollable overflow, which includes
                // both the root element itself and any content which overflows it (e.g. when
                // the root element has a fixed height but its content is taller). A document
                // without a root element has no scrollable content.
                //
                // The root positioning pass lays out the boxes whose containing block is the
                // initial containing block after the root's own layout was produced, so their
                // extents are added here. A fixed box does not scroll with the document and
                // stays out
                // (<https://drafts.csswg.org/css-overflow-3/#scrollable>).
                let (content_width, content_height) = match self.try_root_element() {
                    Some(root) => {
                        let layout = root.final_layout();
                        let mut width = layout.size.width.max(root.scroll_width());
                        let mut height = layout.size.height.max(root.scroll_height());
                        for &child_id in root.hoisted_children.borrow().iter() {
                            let Some(child) = self.nodes.get(child_id) else {
                                continue;
                            };
                            if child
                                .primary_styles()
                                .is_some_and(|styles| styles.get_box().position == Position::Fixed)
                            {
                                continue;
                            }
                            let child_layout = child.final_layout();
                            width = width.max(child_layout.location.x + child_layout.size.width);
                            height = height.max(child_layout.location.y + child_layout.size.height);
                        }
                        (f64::from(width), f64::from(height))
                    }
                    None => (0.0, 0.0),
                };
                let scale = self.viewport.scale() as f64;
                let window_width = self.viewport.window_size.0 as f64 / scale;
                let window_height = self.viewport.window_size.1 as f64 / scale;
                let max = Point {
                    x: (content_width - window_width).max(0.0),
                    y: (content_height - window_height).max(0.0),
                };
                (self.viewport_scroll(), Point::ZERO, max)
            }
            ScrollTarget::Node(node_id) => {
                let Some(node) = self.nodes.get(node_id) else {
                    return (Point::ZERO, Point::ZERO, Point::ZERO);
                };

                // An axis with a non-scrolling overflow value has no scrollable range, even
                // when its content overflows.
                let scrollable = |overflow| match overflow {
                    Overflow::Scroll | Overflow::Auto => true,
                    Overflow::Hidden => include_hidden,
                    _ => false,
                };
                let (can_x_scroll, can_y_scroll) = node
                    .primary_styles()
                    .map(|styles| {
                        (
                            scrollable(styles.clone_overflow_x()),
                            scrollable(styles.clone_overflow_y()),
                        )
                    })
                    .unwrap_or((false, false));
                let (min, max) = node.scroll_range();
                let min = Point {
                    x: if can_x_scroll { min.x } else { 0.0 },
                    y: if can_y_scroll { min.y } else { 0.0 },
                };
                let max = Point {
                    x: match can_x_scroll {
                        true => max.x,
                        false => 0.0,
                    },
                    y: match can_y_scroll {
                        true => max.y,
                        false => 0.0,
                    },
                };
                (*node.scroll_offset(), min, max)
            }
        }
    }

    /// Start a smooth (animated) scroll towards the given absolute scroll offset. The
    /// animation is advanced each frame in [`BaseDocument::resolve_scroll_animation`].
    fn start_scroll_animation(&mut self, target: ScrollTarget, end: Point<f64>) {
        let start = self.scroll_state(target, true).0;

        // The animation starts at the timeline sample of the frame that is
        // running; a later resolve advances it.
        let start_time = self.animation_time_ms;

        // A smooth scroll the page asked for replaces a fling the user's touch
        // started, so only one animation drives the document at a time.
        self.scroll_animations.fling = None;
        self.scroll_animations.smooth.insert(
            target,
            ScrollToState {
                start,
                end,
                start_time,
                duration: Self::SMOOTH_SCROLL_DURATION_MS,
            },
        );

        // Ensure the frame loop runs so the animation is driven to completion.
        self.request_redraw();
    }

    fn should_scroll_smoothly(&self, target: ScrollTarget, behavior: ScrollBehavior) -> bool {
        match behavior {
            ScrollBehavior::Auto => {
                let styled_node = match target {
                    ScrollTarget::Node(node_id) => self.nodes.get(node_id),
                    ScrollTarget::Viewport => self.try_root_element(),
                };
                styled_node.is_some_and(|node| {
                    node.primary_styles().is_some_and(|style| {
                        style.clone_scroll_behavior()
                            == style::computed_values::scroll_behavior::T::Smooth
                    })
                })
            }
            ScrollBehavior::Instant => false,
            ScrollBehavior::Smooth => true,
        }
    }

    /// Scroll an element to the given absolute scroll offset in CSS pixels.
    ///
    /// Unlike a user-initiated scroll, a programmatic scroll targets exactly one scroller:
    /// scroll the element cannot consume is discarded rather than transferred to an ancestor.
    pub fn scroll_to(&mut self, node_id: NodeId, x: f64, y: f64, behavior: ScrollBehavior) {
        self.scroll_programmatically(node_id, ScrollAmount::To(Point { x, y }), behavior);
    }

    /// Scroll an element by the given relative offset in CSS pixels.
    pub fn scroll_by(&mut self, node_id: NodeId, x: f64, y: f64, behavior: ScrollBehavior) {
        self.scroll_programmatically(node_id, ScrollAmount::By(Point { x, y }), behavior);
    }

    fn scroll_programmatically(
        &mut self,
        node_id: NodeId,
        amount: ScrollAmount,
        behavior: ScrollBehavior,
    ) {
        if self.nodes.get(node_id).is_none() {
            return;
        }

        // TODO: dispatch `scroll` events for programmatic scrolls.
        self.scroll(
            ScrollRequest {
                target: ScrollTarget::Node(node_id),
                amount,
                overflow: ScrollOverflow::Clamp,
                source: ScrollSource::Programmatic,
                behavior,
                interrupt_animation: true,
            },
            &mut |_| {},
        );
    }

    fn aligned_scroll_offset(
        current: f64,
        viewport_size: f64,
        target_start: f64,
        target_size: f64,
        position: ScrollLogicalPosition,
    ) -> f64 {
        let target_end = target_start + target_size;
        match position {
            ScrollLogicalPosition::Start => target_start,
            ScrollLogicalPosition::Center => target_start - (viewport_size - target_size) / 2.0,
            ScrollLogicalPosition::End => target_end - viewport_size,
            ScrollLogicalPosition::Nearest => {
                let viewport_end = current + viewport_size;
                if (target_start >= current && target_end <= viewport_end)
                    || (target_start <= current && target_end >= viewport_end)
                {
                    current
                } else {
                    let start_offset = target_start;
                    let end_offset = target_end - viewport_size;
                    if (start_offset - current).abs() < (end_offset - current).abs() {
                        start_offset
                    } else {
                        end_offset
                    }
                }
            }
        }
    }

    /// Whether `id`'s `overflow` is applied to the viewport instead of to a
    /// scrolling box of its own
    /// (<https://drafts.csswg.org/css-overflow-3/#overflow-propagation>).
    ///
    /// The root element's overflow always belongs to the viewport. When the root
    /// is an HTML `html` element whose own overflow is `visible` on both axes,
    /// the first `body` child's overflow propagates instead, and that `body` has
    /// no scrolling box. Blitz applies neither value to the viewport scroll
    /// range yet (`docs/upstream.md`), but a scroll walk and the CSSOM View
    /// used-overflow questions both need the boxes the propagation removes.
    pub fn overflow_propagates_to_viewport(&self, id: NodeId) -> bool {
        let Some(root) = self.try_root_element() else {
            return false;
        };
        if id == root.id {
            return true;
        }
        let Some(node) = self.nodes.get(id) else {
            return false;
        };
        if node.parent != Some(root.id) || !node.data.is_element_with_tag_name(&local_name!("body"))
        {
            return false;
        }
        root.primary_styles().is_some_and(|styles| {
            styles.clone_overflow_x() == Overflow::Visible
                && styles.clone_overflow_y() == Overflow::Visible
        })
    }

    /// Scroll the element into view with the requested alignment in each axis
    /// (<https://drafts.csswg.org/cssom-view/#dom-element-scrollintoview>,
    /// <https://drafts.csswg.org/cssom-view/#scroll-a-target-into-view>).
    ///
    /// Every ancestor that establishes a scrolling box is scrolled from
    /// innermost to outermost, then the viewport, so a target inside a nested
    /// scroller moves that scroller and not just the page. `block`/`inline`
    /// are logical, so each box's writing mode decides which physical axis and
    /// which edge each one names. `container` caps the walk at the nearest
    /// ancestor scrolling box.
    pub fn scroll_into_view(
        &mut self,
        node_id: NodeId,
        behavior: ScrollBehavior,
        block: ScrollLogicalPosition,
        inline: ScrollLogicalPosition,
        container: ScrollIntoViewContainer,
    ) {
        let Some(target) = self.nodes.get(node_id) else {
            return;
        };
        if !target.has_boxes() {
            return;
        }
        let root = self.try_root_element().map(|root| root.id);
        // Only the scrolling boxes in the target's containing block chain can
        // move it: an absolutely positioned ancestor's containing block is the
        // initial containing block, so a scroller the target is laid out
        // outside of must not scroll (`scrollintoview-containingblock-chain.html`).
        let mut containers = Vec::new();
        // A viewport-fixed box does not move when the viewport scrolls, so the
        // chain ends at it: its own scroller may move its content, but the
        // viewport must not scroll (`scrollIntoView-fixed.html`).
        let mut chain_fixed_to_viewport = self
            .nodes
            .get(node_id)
            .is_some_and(|node| fixed_to_viewport(node, root));
        let mut current = node_id;
        while let Some(id) = self
            .nodes
            .get(current)
            .and_then(crate::Node::containing_block)
        {
            let Some(node) = self.nodes.get(id) else {
                break;
            };
            if establishes_scrolling_box(node) && !self.overflow_propagates_to_viewport(id) {
                containers.push(id);
            }
            if fixed_to_viewport(node, root) {
                chain_fixed_to_viewport = true;
                break;
            }
            current = id;
        }
        // The walk found them innermost first; `container: "nearest"` stops
        // after the first.
        let nearest = container == ScrollIntoViewContainer::Nearest;
        let scrolled_containers = !containers.is_empty();
        if nearest {
            containers.truncate(1);
        }
        // Each inner scroll moves the target for the outer alignment: the
        // scrolls start together, so the inner box's offset has not been
        // applied to layout when the outer box is measured. Carry the amount
        // the inner boxes will scroll forward instead
        // (<https://drafts.csswg.org/cssom-view/#scroll-a-target-into-view>).
        let mut pending = Point::ZERO;
        for container in containers {
            let target = ScrollTarget::Node(container);
            let before = self.scroll_state(target, true).0;
            self.scroll_target_in_container(node_id, container, behavior, block, inline, pending);
            // An instant scroll is already in the offsets the next measurement
            // reads; only an animation still owes layout the distance.
            if self
                .scroll_animations
                .smooth
                .contains_key(&self.canonical_scroll_target(target))
            {
                let after = self.eventual_scroll(target);
                pending.x += after.x - before.x;
                pending.y += after.y - before.y;
            }
        }
        // `container: "nearest"` aborts after the nearest scrolling box, so the
        // viewport only scrolls when the target has no ancestor scroller.
        if (!nearest || !scrolled_containers) && !chain_fixed_to_viewport {
            self.scroll_target_in_viewport(node_id, behavior, block, inline, root, pending);
        }
    }

    /// The offset `target` will have once the scroll started for it has
    /// finished: a smooth scroll's endpoint, or the offset already written by
    /// an instant one.
    fn eventual_scroll(&self, target: ScrollTarget) -> Point<f64> {
        match self
            .scroll_animations
            .smooth
            .get(&self.canonical_scroll_target(target))
        {
            Some(scroll_to) => scroll_to.end,
            None => self.scroll_state(target, true).0,
        }
    }

    /// Scroll one ancestor's scrolling box so `target_id` has the requested
    /// logical alignment. Positions are content coordinates: measured from the
    /// box's padding-box origin with its current scroll offset removed, which
    /// is what `aligned_scroll_offset` expects.
    fn scroll_target_in_container(
        &mut self,
        target_id: NodeId,
        container_id: NodeId,
        behavior: ScrollBehavior,
        block: ScrollLogicalPosition,
        inline: ScrollLogicalPosition,
        pending: Point<f64>,
    ) {
        let Some(target) = self.nodes.get(target_id) else {
            return;
        };
        let target_position = target.absolute_position(
            target.scroll_offset().x as f32,
            target.scroll_offset().y as f32,
        );
        let target_size = target.final_layout().size;
        let Some(container) = self.nodes.get(container_id) else {
            return;
        };
        let container_position = container.absolute_position(0.0, 0.0);
        let border = container.final_layout().border;
        let scroll = *container.scroll_offset();
        let client_width = container.client_width() as f64;
        let client_height = container.client_height() as f64;
        let writing_mode = container.writing_mode();
        let (x, y) = Self::aligned_offsets(
            (scroll.x, scroll.y),
            (client_width, client_height),
            (
                target_position.x as f64 - (container_position.x + border.left) as f64 - pending.x,
                target_position.y as f64 - (container_position.y + border.top) as f64 - pending.y,
            ),
            (target_size.width as f64, target_size.height as f64),
            LogicalAxis::block(writing_mode),
            LogicalAxis::inline(writing_mode),
            block,
            inline,
        );
        self.scroll_to(container_id, x, y, behavior);
    }

    /// Scroll the viewport so `target_id` has the requested logical alignment.
    fn scroll_target_in_viewport(
        &mut self,
        target_id: NodeId,
        behavior: ScrollBehavior,
        block: ScrollLogicalPosition,
        inline: ScrollLogicalPosition,
        root: Option<NodeId>,
        pending: Point<f64>,
    ) {
        let Some(root) = root else {
            return;
        };
        let Some(target) = self.nodes.get(target_id) else {
            return;
        };
        let target_position = target.absolute_position(
            target.scroll_offset().x as f32,
            target.scroll_offset().y as f32,
        );
        let target_size = target.final_layout().size;
        let writing_mode = self
            .nodes
            .get(root)
            .map_or_else(WritingMode::empty, crate::Node::writing_mode);
        let scale = self.viewport.scale() as f64;
        let viewport_width = self.viewport.window_size.0 as f64 / scale;
        let viewport_height = self.viewport.window_size.1 as f64 / scale;
        let scroll = self.viewport_scroll();
        let (x, y) = Self::aligned_offsets(
            (scroll.x, scroll.y),
            (viewport_width, viewport_height),
            (
                target_position.x as f64 - pending.x,
                target_position.y as f64 - pending.y,
            ),
            (target_size.width as f64, target_size.height as f64),
            LogicalAxis::block(writing_mode),
            LogicalAxis::inline(writing_mode),
            block,
            inline,
        );
        self.scroll_to(root, x, y, behavior);
    }

    /// Align a target on each physical axis from the logical block and inline
    /// positions of the box's writing mode.
    fn aligned_offsets(
        current: (f64, f64),
        viewport: (f64, f64),
        target: (f64, f64),
        target_size: (f64, f64),
        block_axis: LogicalAxis,
        inline_axis: LogicalAxis,
        block: ScrollLogicalPosition,
        inline: ScrollLogicalPosition,
    ) -> (f64, f64) {
        let block = block_axis.position(block);
        let inline = inline_axis.position(inline);
        if block_axis.horizontal {
            (
                Self::aligned_scroll_offset(current.0, viewport.0, target.0, target_size.0, block),
                Self::aligned_scroll_offset(current.1, viewport.1, target.1, target_size.1, inline),
            )
        } else {
            (
                Self::aligned_scroll_offset(current.0, viewport.0, target.0, target_size.0, inline),
                Self::aligned_scroll_offset(current.1, viewport.1, target.1, target_size.1, block),
            )
        }
    }

    /// Resolve a URL fragment (the `#...` part of a URL) to a scroll target.
    ///
    /// Returns `None` if the fragment matches no element and is not a top-of-document
    /// fragment. Otherwise returns `Some(target)`, where `target` is `Some(node_id)` for
    /// the element to scroll to, or `None` to scroll to the top of the document (matching
    /// browser behaviour for empty and `top` fragments).
    fn resolve_fragment_scroll_target(&self, fragment: &str) -> Option<Option<NodeId>> {
        // Fragments are percent-encoded in URLs (e.g. `%20`); decode before matching.
        let decoded = percent_encoding::percent_decode_str(fragment)
            .decode_utf8_lossy()
            .into_owned();

        if !decoded.is_empty() {
            if let Some(node_id) = self.get_fragment_target(&decoded) {
                return Some(Some(node_id));
            }
        }

        // An empty fragment, or the special "top" fragment when no matching element exists,
        // scrolls to the top of the document.
        if decoded.is_empty() || decoded.eq_ignore_ascii_case("top") {
            return Some(None);
        }

        None
    }

    fn scroll_to_fragment_with_behavior(
        &mut self,
        fragment: &str,
        behavior: ScrollBehavior,
    ) -> bool {
        match self.resolve_fragment_scroll_target(fragment) {
            Some(Some(node_id)) => {
                self.scroll_into_view(
                    node_id,
                    behavior,
                    ScrollLogicalPosition::Start,
                    ScrollLogicalPosition::Nearest,
                    ScrollIntoViewContainer::All,
                );
                true
            }
            Some(None) => {
                let root_id = self.root_element().id;
                self.scroll_to(root_id, 0.0, 0.0, behavior);
                true
            }
            None => false,
        }
    }

    /// Scroll to the element targeted by the given URL fragment (the `#...` part of a URL).
    ///
    /// An empty fragment (or a `top` fragment that matches no element) scrolls to the top
    /// of the document, matching browser behaviour. Returns `true` if a scroll target was
    /// found.
    pub fn scroll_to_fragment(&mut self, fragment: &str) -> bool {
        self.scroll_to_fragment_with_behavior(fragment, ScrollBehavior::Auto)
    }

    /// Like [`BaseDocument::scroll_to_fragment`], but animates the viewport towards the
    /// target instead of jumping instantly. Returns `true` if a scroll target was found.
    pub fn scroll_to_fragment_smooth(&mut self, fragment: &str) -> bool {
        self.scroll_to_fragment_with_behavior(fragment, ScrollBehavior::Smooth)
    }

    pub fn resolve_scroll_animation(&mut self) {
        if let Some(fling_state) = self.scroll_animations.fling.clone() {
            let time_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64 as f64;

            let time_diff_ms = time_ms - fling_state.last_seen_time;

            // 0.95 @ 60fps normalized to actual frame times
            let deceleration = 1.0 - ((0.05 / 16.66666) * time_diff_ms);

            let x_velocity = fling_state.x_velocity * deceleration;
            let y_velocity = fling_state.y_velocity * deceleration;
            let fling_state = FlingState {
                target: fling_state.target,
                last_seen_time: time_ms,
                x_velocity,
                y_velocity,
            };

            let dx = fling_state.x_velocity * time_diff_ms;
            let dy = fling_state.y_velocity * time_diff_ms;

            self.scroll_chain_by(Some(fling_state.target), dx, dy, &mut |_| {});
            // The state advanced above, so the stored copy keeps the reduced
            // velocity the stop test needs.
            self.scroll_animations.fling = (fling_state.x_velocity.abs() >= 0.1
                || fling_state.y_velocity.abs() >= 0.1)
                .then_some(fling_state);
        }

        // A smooth scroll runs on the document's animation timeline, the same
        // clock the frame that resolves it samples
        // (<https://drafts.csswg.org/cssom-view/#scroll-an-element>).
        let time_ms = self.animation_time_ms;
        // Every box animates independently; a finished one leaves the map and
        // the rest keep going.
        let targets: Vec<ScrollTarget> = self.scroll_animations.smooth.keys().copied().collect();
        for target in targets {
            let Some(scroll_to) = self.scroll_animations.smooth.get(&target).cloned() else {
                continue;
            };

            // Normalised progress through the animation, clamped to [0, 1].
            let progress = if scroll_to.duration <= 0.0 {
                1.0
            } else {
                ((time_ms - scroll_to.start_time) / scroll_to.duration).clamp(0.0, 1.0)
            };
            let eased = ease_in_out_cubic(progress);

            // Interpolate the target offset and move to it.
            let position = Point {
                x: scroll_to.start.x + (scroll_to.end.x - scroll_to.start.x) * eased,
                y: scroll_to.start.y + (scroll_to.end.y - scroll_to.start.y) * eased,
            };
            // TODO: dispatch `scroll` events for programmatic scrolls.
            self.write_scroll_offset(target, position, &mut |_| {});

            if progress >= 1.0 {
                // The smooth scroll has arrived: the next scroll steps report
                // its end after the final `scroll` entry
                // (<https://drafts.csswg.org/cssom-view/#scrolling-events>).
                self.note_scroll_ended(scroll_event_target(target));
                self.scroll_animations.smooth.remove(&target);
            }
        }
    }
}
