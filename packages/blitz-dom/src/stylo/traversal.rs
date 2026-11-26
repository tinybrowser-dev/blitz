//! Running Stylo's style traversal over a document.

use blitz_traits::node_id::NodeId;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use crate::StyleThreading;
use crate::animation_events::{AnimationEventKind, PendingAnimationEvent};
use crate::node::Node;
use style::animation::AnimationState;
use style::animation::KeyframesIterationState;
use style::global_style_data::STYLE_THREAD_POOL;
use style::invalidation::element::restyle_hints::RestyleHint;
use style::properties::ComputedValues;
use style::selector_parser::PseudoElement;
use style::selector_parser::RestyleDamage;
use style::{
    Atom,
    context::{
        RegisteredSpeculativePainter, RegisteredSpeculativePainters, SharedStyleContext,
        StyleContext, ThreadLocalStyleContext,
    },
    dom::{TElement, TNode},
    global_style_data::GLOBAL_STYLE_DATA,
    servo_arc::Arc,
    shared_lock::StylesheetGuards,
    stylist::RuleInclusion,
    thread_state::ThreadState,
    traversal::{DomTraversal, recalc_style_at, resolve_style},
    traversal_flags::TraversalFlags,
};
use style_traits::ToCss as _;

use super::dom::BlitzNode;

/// Serialises `Parallel` style traversals across documents. Stylo's per-worker
/// caches (`StyleSharingCache`, `StyleBloom`) live in native thread-locals that
/// a traversal borrows mutably for its whole duration, so two traversals in
/// flight on the shared pool at once panic with `already mutably borrowed`
/// (<https://github.com/DioxusLabs/blitz/issues/430>).
static PARALLEL_TRAVERSAL_LOCK: Mutex<()> = Mutex::new(());

impl crate::document::BaseDocument {
    pub fn resolve_stylist(&mut self, now: f64) {
        style::thread_state::enter(ThreadState::LAYOUT);

        let guard = &self.guard;
        let guards = StylesheetGuards {
            author: &guard.read(),
            ua_or_user: &guard.read(),
        };

        let root = BlitzNode(self.nodes[self.root_node_id].first_element_child().unwrap());

        self.stylist
            .flush(&guards)
            .process_style(root, Some(&self.snapshots));

        // Mark actively animating nodes as dirty
        let mut sets = self.animations.sets.write();
        for (key, set) in sets.iter_mut() {
            let node_id = NodeId::from_u64(key.node.id() as u64);

            // Drop animations belonging to nodes that are no longer in the
            // document. A removed element is never restyled, so it would never
            // get a chance to cancel its own animations; an infinite animation
            // would then keep `has_active_animations` set forever and force a
            // redraw every frame. Emptying the set here lets the `retain` below
            // discard it so the flag can clear on this same pass.
            let in_document = self
                .nodes
                .get(node_id)
                .is_some_and(|node| node.flags.is_in_document());
            if !in_document {
                set.animations.clear();
                set.transitions.clear();
                continue;
            }

            self.nodes[node_id].set_restyle_hint(RestyleHint::RESTYLE_SELF);

            let pseudo_element = pseudo_element_css(key.pseudo_element.as_ref());
            for animation in set.animations.iter_mut() {
                if animation.state == AnimationState::Pending && animation.started_at <= now {
                    animation.state = AnimationState::Running;
                    // The delay is excluded from `elapsedTime` unless it is
                    // negative, in which case the animation begins partway
                    // through its active interval
                    // (<https://drafts.csswg.org/css-animations-1/#dom-animationevent-elapsedtime>).
                    self.pending_animation_events.push(PendingAnimationEvent {
                        target: node_id,
                        kind: AnimationEventKind::AnimationStart,
                        name: animation.name.to_string(),
                        pseudo_element: pseudo_element.clone(),
                        elapsed_time: (now - animation.started_at).max(0.0),
                    });
                }
                if animation.iterate_if_necessary(now) {
                    // The event reports the time at which the completed
                    // iteration ended, counted from the start of the active
                    // interval
                    // (<https://drafts.csswg.org/css-animations-1/#eventdef-animationevent-animationiteration>).
                    let iterations = match animation.iteration_state {
                        KeyframesIterationState::Infinite(current) => current,
                        KeyframesIterationState::Finite(current, _) => current,
                    };
                    self.pending_animation_events.push(PendingAnimationEvent {
                        target: node_id,
                        kind: AnimationEventKind::AnimationIteration,
                        name: animation.name.to_string(),
                        pseudo_element: pseudo_element.clone(),
                        elapsed_time: iterations * animation.duration,
                    });
                }

                if animation.state == AnimationState::Running && animation.has_ended(now) {
                    animation.state = AnimationState::Finished;
                    // The end of the last iteration reports the animation's
                    // active duration
                    // (<https://drafts.csswg.org/css-animations-1/#eventdef-animationevent-animationend>).
                    let iterations = match animation.iteration_state {
                        KeyframesIterationState::Infinite(_) => 1.0,
                        KeyframesIterationState::Finite(_, max) => max,
                    };
                    self.pending_animation_events.push(PendingAnimationEvent {
                        target: node_id,
                        kind: AnimationEventKind::AnimationEnd,
                        name: animation.name.to_string(),
                        pseudo_element: pseudo_element.clone(),
                        elapsed_time: iterations * animation.duration,
                    });
                }
            }

            for transition in set.transitions.iter_mut() {
                if transition.state == AnimationState::Pending && transition.start_time <= now {
                    transition.state = AnimationState::Running;
                    self.pending_animation_events.push(PendingAnimationEvent {
                        target: node_id,
                        kind: AnimationEventKind::TransitionStart,
                        name: transition.property_animation.property_id().to_css_string(),
                        pseudo_element: pseudo_element.clone(),
                        elapsed_time: 0.0,
                    });
                }
                if transition.state == AnimationState::Running && transition.has_ended(now) {
                    transition.state = AnimationState::Finished;
                    self.pending_animation_events.push(PendingAnimationEvent {
                        target: node_id,
                        kind: AnimationEventKind::TransitionEnd,
                        name: transition.property_animation.property_id().to_css_string(),
                        pseudo_element: pseudo_element.clone(),
                        elapsed_time: transition.property_animation.duration,
                    });
                }
            }
        }
        drop(sets);

        // Build the style context used by the style traversal
        let context = SharedStyleContext {
            traversal_flags: TraversalFlags::empty(),
            stylist: &self.stylist,
            options: GLOBAL_STYLE_DATA.options.clone(),
            guards,
            visited_styles_enabled: false,
            animations: self.animations.clone(),
            current_time_for_animations: now,
            snapshot_map: &self.snapshots,
            registered_speculative_painters: &RegisteredPaintersImpl,
        };

        // components/layout_2020/lib.rs:983
        let root = BlitzNode(self.root_element());
        // dbg!(root);
        let token = RecalcStyle::pre_traverse(root, &context);

        let mut nodes_needing_style_image_flush = Vec::new();
        if token.should_traverse() {
            // Style the elements, resolving their data
            let mut traverser = RecalcStyle::new(context);
            // `Sequential` bypasses Stylo's global pool. See `StyleThreading`.
            let parallel = matches!(self.style_threading, StyleThreading::Parallel);
            let _traversal_lock = parallel.then(|| {
                PARALLEL_TRAVERSAL_LOCK
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
            });
            let pool_guard = parallel.then(|| STYLE_THREAD_POOL.pool());
            let rayon_pool = pool_guard.as_ref().and_then(|g| g.as_ref());
            style::driver::traverse_dom(&traverser, token, rayon_pool);
            nodes_needing_style_image_flush =
                std::mem::take(traverser.nodes_needing_style_image_flush.get_mut().unwrap());
            // A subtree that becomes `display: none` skips the out-of-flow pass.
            // Clear its hoisted lists so rounding won't visit boxes from the
            // last visible layout that have lost their styles or been removed.
            for node_id in traverser.newly_hidden_nodes.into_inner().unwrap() {
                self.clear_subtree_hoisted_children(node_id);
            }
        }
        self.pending_style_image_nodes
            .extend(nodes_needing_style_image_flush);

        for opaque in self.snapshots.keys() {
            let id = NodeId::from_u64(opaque.id() as u64);
            if let Some(node) = self.nodes.get_mut(id) {
                node.set_has_snapshot(false);
            }
        }
        self.snapshots.clear();

        let mut sets = self.animations.sets.write();
        for (key, set) in sets.iter_mut() {
            let node_id = NodeId::from_u64(key.node.id() as u64);
            let pseudo_element = pseudo_element_css(key.pseudo_element.as_ref());
            // A transition that appeared during this traversal starts now: the
            // tick above runs before style computation, so the creation is
            // still marked new here
            // (<https://drafts.csswg.org/css-transitions-1/#eventdef-transitionevent-transitionrun>).
            for transition in &set.transitions {
                if transition.is_new {
                    self.pending_animation_events.push(PendingAnimationEvent {
                        target: node_id,
                        kind: AnimationEventKind::TransitionRun,
                        name: transition.property_animation.property_id().to_css_string(),
                        pseudo_element: pseudo_element.clone(),
                        elapsed_time: 0.0,
                    });
                }
            }
            // A cancellation is recorded by the style change above and removed
            // below, so this is the last chance to report it
            // (<https://drafts.csswg.org/css-animations-2/#eventdef-animationevent-animationcancel>).
            for animation in &set.animations {
                if animation.state == AnimationState::Canceled {
                    self.pending_animation_events.push(PendingAnimationEvent {
                        target: node_id,
                        kind: AnimationEventKind::AnimationCancel,
                        name: animation.name.to_string(),
                        pseudo_element: pseudo_element.clone(),
                        elapsed_time: (now - animation.started_at).max(0.0),
                    });
                }
            }
            for transition in &set.transitions {
                if transition.state == AnimationState::Canceled {
                    self.pending_animation_events.push(PendingAnimationEvent {
                        target: node_id,
                        kind: AnimationEventKind::TransitionCancel,
                        name: transition.property_animation.property_id().to_css_string(),
                        pseudo_element: pseudo_element.clone(),
                        elapsed_time: (now - transition.start_time).max(0.0),
                    });
                }
            }
            set.clear_canceled_animations();
            for animation in set.animations.iter_mut() {
                animation.is_new = false;
            }
            for transition in set.transitions.iter_mut() {
                transition.is_new = false;
            }
        }
        let mut finished = Vec::new();
        sets.retain(|key, state| {
            let keep = !state.is_empty();
            if !keep {
                finished.push(key.node);
            }
            keep
        });
        // Clear `may_have_animations` on elements whose last set (including
        // their pseudo-elements' sets) was just removed.
        for node in finished {
            if sets.keys().any(|key| key.node == node) {
                continue;
            }
            let node_id = NodeId::from_u64(node.id() as u64);
            if let Some(data) = self.nodes.get(node_id).and_then(|node| node.element_data()) {
                data.may_have_animations.store(false, Ordering::Relaxed);
            }
        }
        self.has_active_animations = sets.values().any(|state| state.needs_animation_ticks());

        // Maybe run garbage collection. Stylo has internal to determine whether to run or not.
        self.stylist.rule_tree().maybe_gc();

        style::thread_state::exit(ThreadState::LAYOUT);
    }

    /// Compute the style of an element which the regular style traversal
    /// skipped because it lives inside a `display: none` subtree.
    ///
    /// The style traversal culls `display: none` subtrees, so such elements
    /// never have styles stored on them, but `getComputedStyle()` must still
    /// return their computed values. This resolves the style on demand
    /// (styling any unstyled ancestors along the way) without storing it on the
    /// node. Returns `None` for non-elements and elements outside the document.
    pub fn resolve_undisplayed_style(&self, node_id: NodeId) -> Option<Arc<ComputedValues>> {
        self.resolve_undisplayed_styles(node_id)?
            .get_primary()
            .cloned()
    }

    /// [`Self::resolve_undisplayed_style`] for one of an element's eager
    /// pseudo-elements, for `getComputedStyle()`. A pseudo-element the style
    /// engine would not generate has no style of its own
    /// (<https://drafts.csswg.org/cssom/#dom-window-getcomputedstyle>).
    pub fn resolve_undisplayed_pseudo_style(
        &self,
        node_id: NodeId,
        pseudo: PseudoElement,
    ) -> Option<Arc<ComputedValues>> {
        self.resolve_undisplayed_styles(node_id)?
            .pseudos
            .get(&pseudo)
            .cloned()
    }

    /// The primary and eager pseudo-element styles of an element the regular
    /// style traversal skipped because it lives inside a `display: none`
    /// subtree.
    fn resolve_undisplayed_styles(&self, node_id: NodeId) -> Option<style::data::ElementStyles> {
        let node = self.nodes.get(node_id)?;
        if !node.is_element() || !node.flags.is_in_document() {
            return None;
        }

        style::thread_state::enter(ThreadState::LAYOUT);

        let guard = &self.guard;
        let guards = StylesheetGuards {
            author: &guard.read(),
            ua_or_user: &guard.read(),
        };
        let shared = SharedStyleContext {
            traversal_flags: TraversalFlags::empty(),
            stylist: &self.stylist,
            options: GLOBAL_STYLE_DATA.options.clone(),
            guards,
            visited_styles_enabled: false,
            animations: self.animations.clone(),
            current_time_for_animations: 0.0,
            snapshot_map: &self.snapshots,
            registered_speculative_painters: &RegisteredPaintersImpl,
        };
        // The thread-local context's sequential task list asserts the LAYOUT state when dropped
        let styles = {
            let mut thread_local = ThreadLocalStyleContext::new();
            let mut context = StyleContext {
                shared: &shared,
                thread_local: &mut thread_local,
            };
            resolve_style(
                &mut context,
                BlitzNode(node),
                RuleInclusion::All,
                None,
                None,
            )
        };

        style::thread_state::exit(ThreadState::LAYOUT);

        Some(styles)
    }
}

/// Handle custom painters like images for layouting
///
/// todo: actually implement this
pub struct RegisteredPaintersImpl;
impl RegisteredSpeculativePainters for RegisteredPaintersImpl {
    fn get(&self, _name: &Atom) -> Option<&dyn RegisteredSpeculativePainter> {
        None
    }
}

pub struct RecalcStyle<'a> {
    context: SharedStyleContext<'a>,
    /// Nodes whose `background-image`/`mask-image` layers need flushing to
    /// dedicated storage on the node (see `flush_image_layers_from_style`)
    /// because their style changed during this traversal.
    nodes_needing_style_image_flush: Mutex<Vec<NodeId>>,
    newly_hidden_nodes: Mutex<Vec<NodeId>>,
}

impl<'a> RecalcStyle<'a> {
    pub fn new(context: SharedStyleContext<'a>) -> Self {
        RecalcStyle {
            context,
            nodes_needing_style_image_flush: Mutex::new(Vec::new()),
            newly_hidden_nodes: Mutex::new(Vec::new()),
        }
    }
}

#[allow(unsafe_code)]
impl<'dom> DomTraversal<BlitzNode<'dom>> for RecalcStyle<'_> {
    fn process_preorder<F: FnMut(BlitzNode<'dom>)>(
        &self,
        context: &mut StyleContext<BlitzNode<'dom>>,
        node: BlitzNode<'dom>,
        note_child: F,
    ) {
        if let Some(el) = node.as_element() {
            // let mut data = el.mutate_data().unwrap();
            let mut data = unsafe { el.ensure_data() };
            let was_displayed =
                data.styles.get_primary().is_some() && !data.styles.is_display_none();
            recalc_style_at(self, context, el, &mut data, note_child);

            if was_displayed && data.styles.is_display_none() {
                self.newly_hidden_nodes.lock().unwrap().push(el.id);
            }
            sync_pseudo_element_styles(
                el.0,
                &data,
                &self.nodes_needing_style_image_flush,
                &self.newly_hidden_nodes,
            );
            if let Some(style) = data.styles.get_primary() {
                el.record_out_of_flow(style);
            }

            if !data.damage.is_empty() {
                // Mark the ancestor chain so that the damage propagation pass
                // visits this element.
                el.mark_damaged();

                if data
                    .styles
                    .get_primary()
                    .is_some_and(|style| needs_style_image_flush(el.0, style))
                {
                    self.nodes_needing_style_image_flush
                        .lock()
                        .unwrap()
                        .push(el.id);
                }
            }

            // Gets set later on
            el.0.unset_dirty_descendants();
        }
    }

    #[inline]
    fn needs_postorder_traversal() -> bool {
        false
    }

    fn process_postorder(
        &self,
        _style_context: &mut StyleContext<BlitzNode<'dom>>,
        _node: BlitzNode<'dom>,
    ) {
        panic!("this should never be called")
    }

    #[inline]
    fn shared_context(&self) -> &SharedStyleContext<'_> {
        &self.context
    }
}

/// Flush updated pseudo-element (`::before`/`::after`) styles from the owning
/// element's freshly-computed stylo data to the pseudo-element's anonymous node.
///
/// Pseudo-element styles are normally flushed to the pseudo-element's node
/// during box construction (see `flush_pseudo_elements`), but in incremental
/// mode box construction only runs for nodes with construction damage.
/// Pseudo-element style changes which don't require reconstruction (e.g.
/// animations/transitions of repaint- or relayout-only properties) must still
/// be flushed to the pseudo-element's node - along with the damage they imply -
/// so that layout and paint see the new style.
///
/// This runs during the style traversal, immediately after the owning element
/// has been restyled, because that is where the old and new pseudo styles can
/// be diffed precisely (the pseudo node's stored primary style is the "old"
/// style; the owner's just-computed pseudo styles are the "new" ones).
///
/// SAFETY: the style traversal has exclusive access to the tree, and each
/// pseudo-element node is only ever accessed from its owning element's
/// `process_preorder` (pseudo nodes are not part of the DOM child list and are
/// not themselves visited by the traversal), so mutating the pseudo node's
/// stylo data here cannot race with any other traversal thread.
#[allow(unsafe_code)]
fn sync_pseudo_element_styles(
    el: &Node,
    data: &style::data::ElementData,
    nodes_needing_style_image_flush: &Mutex<Vec<NodeId>>,
    newly_hidden_nodes: &Mutex<Vec<NodeId>>,
) {
    let before_node_id = el.before();
    let after_node_id = el.after();
    if before_node_id.is_none() && after_node_id.is_none() {
        return;
    }

    // Note: yes these are kinda backwards (see `flush_pseudo_elements`)
    let pseudos = data.styles.pseudos.as_array();
    let before_style = pseudos[1].clone();
    let after_style = pseudos[0].clone();

    // Creation and removal of pseudo-elements is handled during box construction
    // (Stylo generates construction damage for those cases), so only the case
    // where the pseudo-element both was and remains present is handled here.
    for (pe_node_id, pe_style) in [(before_node_id, before_style), (after_node_id, after_style)] {
        let (Some(pe_node_id), Some(pe_style)) = (pe_node_id, pe_style) else {
            continue;
        };
        let pe_node = el.with(pe_node_id);
        let Some(stylo_data) = pe_node.try_stylo_element_data() else {
            continue;
        };
        let mut pe_data = match unsafe { stylo_data.unsafe_stylo_only_mut() } {
            Some(data) => data,
            None => continue,
        };
        let Some(old_style) = pe_data.styles.primary.clone() else {
            continue;
        };
        if std::ptr::eq(&*old_style, &*pe_style) {
            continue;
        }

        let diff = RestyleDamage::compute_style_difference::<BlitzNode>(&old_style, &pe_style);
        if !old_style.get_box().get_display().is_none()
            && pe_style.get_box().get_display().is_none()
        {
            newly_hidden_nodes.lock().unwrap().push(pe_node_id);
        }
        if !diff.damage.is_empty() {
            pe_data.damage.insert(diff.damage);
            pe_node.mark_damaged();

            if needs_style_image_flush(pe_node, &pe_style) {
                nodes_needing_style_image_flush
                    .lock()
                    .unwrap()
                    .push(pe_node_id);
            }
        }
        pe_node.record_out_of_flow(&pe_style);
        pe_data.styles.primary = Some(pe_style);
        pe_data.set_restyled();
    }
}

/// Whether a node's `background-image`/`mask-image` layers need flushing to
/// dedicated storage on the node: the URLs referenced by the new style differ
/// from the image data currently stored on the node.
fn needs_style_image_flush(node: &Node, style: &ComputedValues) -> bool {
    use crate::node::ImageResourceData;
    use style::url::ComputedUrl;
    use style::values::computed::image::Image;

    fn out_of_sync(style_images: &[Image], stored: &[Option<ImageResourceData>]) -> bool {
        if style_images.len() != stored.len() {
            // Differing lengths only matter if either side references an image
            // (a style list of `none` layers and empty storage are in sync).
            return style_images
                .iter()
                .any(|image| matches!(image, Image::Url(_)))
                || stored.iter().any(Option::is_some);
        }
        std::iter::zip(style_images, stored).any(|(style_image, stored)| {
            match (style_image, stored) {
                (Image::Url(ComputedUrl::Valid(url)), Some(data)) => **url != *data.url,
                (Image::Url(ComputedUrl::Valid(_)), None) => true,
                (_, Some(_)) => true,
                (_, None) => false,
            }
        })
    }

    node.data.downcast_element().is_some_and(|el| {
        out_of_sync(
            &style.get_background().background_image.0,
            &el.background_images,
        ) || out_of_sync(&style.get_svg().mask_image.0, &el.mask_images)
            || content_out_of_sync(style, &el.content_images)
    })
}

/// Whether the URLs a `content` value names differ from the images currently
/// stored on the node, in the same shape as the layer-list comparison above.
fn content_out_of_sync(
    style: &ComputedValues,
    stored: &[Option<crate::node::ImageResourceData>],
) -> bool {
    let urls = crate::layout::damage::content_urls(style);
    if urls.len() != stored.len() {
        return urls.iter().any(Option::is_some) || stored.iter().any(Option::is_some);
    }
    std::iter::zip(urls, stored).any(|(url, stored)| match (url, stored) {
        (Some(url), Some(data)) => *url != *data.url,
        (Some(_), None) => true,
        (None, stored) => stored.is_some(),
    })
}

/// The `pseudoElement` a lifecycle event reports for an animation set key: an
/// empty string for the element itself, else the pseudo-element's CSS
/// selector
/// (<https://drafts.csswg.org/css-animations-1/#dom-animationevent-pseudoelement>).
fn pseudo_element_css(pseudo: Option<&PseudoElement>) -> String {
    let Some(pseudo) = pseudo else {
        return String::new();
    };
    let mut css = String::new();
    // `PseudoElement` implements `cssparser::ToCss`, whose `to_css_string`
    // helper the `style_traits` trait does not provide for a foreign type.
    cssparser::ToCss::to_css(pseudo, &mut css).expect("writing to a String cannot fail");
    css
}
