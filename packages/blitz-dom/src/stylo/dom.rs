//! Stylo's DOM traits, implemented for Blitz's nodes.

use std::ptr::NonNull;
use std::sync::atomic::Ordering;

use crate::layout::damage::compute_layout_damage;
use crate::node::Node;
use crate::node::NodeData;
use markup5ever::{LocalName, LocalNameStaticSet, Namespace, NamespaceStaticSet, local_name, ns};
use selectors::bloom::BLOOM_HASH_MASK;
use selectors::{
    OpaqueElement,
    attr::{AttrSelectorOperation, NamespaceConstraint},
    matching::{ElementSelectorFlags, MatchingContext, VisitedHandlingMode},
    sink::Push,
};
use style::CaseSensitivityExt;
use style::animation::AnimationSetKey;
use style::applicable_declarations::ApplicableDeclarationBlock;
use style::bloom::each_relevant_element_hash;
use style::data::{ElementDataMut, ElementDataRef};
use style::properties::ComputedValues;
use style::selector_parser::PseudoElement;
use style::selector_parser::RestyleDamage;
use style::stylesheets::scope_rule::ImplicitScopeRoot;
use style::values::AtomString;
use style::{
    context::{QuirksMode, SharedStyleContext},
    dom::{LayoutIterator, NodeInfo, OpaqueNode, TDocument, TElement, TNode, TShadowRoot},
    properties::PropertyDeclarationBlock,
    selector_parser::{NonTSPseudoClass, SelectorImpl},
    servo_arc::{Arc, ArcBorrow},
    shared_lock::{Locked, SharedRwLock},
    values::{AtomIdent, GenericAtomIdent},
};
use style_dom::ElementState;

/// A handle to a node that Servo's style traits are implemented against
///
/// Since BlitzNodes are not persistent (IE we don't keep the pointers around between frames), we choose to just implement
/// the tree structure in the nodes themselves, and temporarily give out pointers during the layout phase.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct BlitzNode<'a>(pub(crate) &'a Node);

impl std::ops::Deref for BlitzNode<'_> {
    type Target = Node;
    fn deref(&self) -> &Node {
        self.0
    }
}

impl std::fmt::Debug for BlitzNode<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl<'a> TDocument for BlitzNode<'a> {
    type ConcreteNode = BlitzNode<'a>;

    fn as_node(&self) -> Self::ConcreteNode {
        *self
    }

    fn is_html_document(&self) -> bool {
        true
    }

    fn quirks_mode(&self) -> QuirksMode {
        // The tree stores the document mode so every node reports the same
        // value the stylist was built with.
        self.tree().quirks_mode()
    }

    fn shared_lock(&self) -> &SharedRwLock {
        self.guard()
    }
}

impl NodeInfo for BlitzNode<'_> {
    fn is_element(&self) -> bool {
        Node::is_element(self)
    }

    fn is_text_node(&self) -> bool {
        Node::is_text_node(self)
    }
}

impl<'a> TShadowRoot for BlitzNode<'a> {
    type ConcreteNode = BlitzNode<'a>;

    fn as_node(&self) -> Self::ConcreteNode {
        *self
    }

    fn host(&self) -> <Self::ConcreteNode as TNode>::ConcreteElement {
        let NodeData::ShadowRoot { host, .. } = self.0.data else {
            panic!("host() on a node that is not a shadow root");
        };
        BlitzNode(self.0.with(host))
    }

    fn style_data<'b>(&self) -> Option<&'b style::stylist::CascadeData>
    where
        Self: 'b,
    {
        let NodeData::ShadowRoot { author_styles, .. } = &self.0.data else {
            return None;
        };
        Some(&author_styles.0.data)
    }
}

// components/styleaapper.rs:
impl<'a> TNode for BlitzNode<'a> {
    type ConcreteElement = BlitzNode<'a>;
    type ConcreteDocument = BlitzNode<'a>;
    type ConcreteShadowRoot = BlitzNode<'a>;

    fn parent_node(&self) -> Option<Self> {
        self.0.parent_node().map(BlitzNode)
    }

    fn first_child(&self) -> Option<Self> {
        self.children.first().map(|id| BlitzNode(self.0.with(*id)))
    }

    fn last_child(&self) -> Option<Self> {
        self.children.last().map(|id| BlitzNode(self.0.with(*id)))
    }

    fn prev_sibling(&self) -> Option<Self> {
        self.0.backward(1).map(BlitzNode)
    }

    fn next_sibling(&self) -> Option<Self> {
        self.0.forward(1).map(BlitzNode)
    }

    fn owner_doc(&self) -> Self::ConcreteDocument {
        BlitzNode(self.0.owner_doc())
    }

    fn is_in_document(&self) -> bool {
        true
    }

    // I think this is the same as parent_node only in the cases when the direct parent is not a real element, forcing us
    // to travel upwards
    //
    // For the sake of this demo, we're just going to return the parent node ann
    fn traversal_parent(&self) -> Option<Self::ConcreteElement> {
        self.0
            .flat_parent()
            .and_then(|id| BlitzNode(self.0.with(id)).as_element())
    }

    fn opaque(&self) -> OpaqueNode {
        Node::opaque(self)
    }

    fn debug_id(self) -> usize {
        self.id.as_u64() as usize
    }

    fn as_element(&self) -> Option<Self::ConcreteElement> {
        match self.data {
            NodeData::Element { .. } => Some(*self),
            _ => None,
        }
    }

    fn as_document(&self) -> Option<Self::ConcreteDocument> {
        match self.data {
            NodeData::Document(_) => Some(*self),
            _ => None,
        }
    }

    fn as_shadow_root(&self) -> Option<Self::ConcreteShadowRoot> {
        match self.data {
            NodeData::ShadowRoot { .. } => Some(*self),
            _ => None,
        }
    }
}

impl selectors::Element for BlitzNode<'_> {
    type Impl = SelectorImpl;

    fn opaque(&self) -> selectors::OpaqueElement {
        // This correctly uses a unique id for the OpaqueElement (unlike using a pointer to the "slot")
        // However, it makes it impossible for us to "rehydrate" the OpaqueElement back into an actual Element
        // which is required to implement the `implicit_scope_for_sheet_in_shadow_root` method below
        //
        // We should see if selectors will accept a PR that allows us to use 128bits for the OpaqueElement. Or
        // find some other solution that will enable "rehydration". This is required to enable and use the
        // Shadow DOM functionality in Stylo.
        let non_null =
            NonNull::new((self.id.as_u64() as usize).wrapping_add(1) as *mut ()).unwrap();
        OpaqueElement::from_ptr(non_null)
    }

    fn parent_element(&self) -> Option<Self> {
        self.0
            .parent
            .and_then(|id| BlitzNode(self.0.with(id)).as_element())
    }

    fn parent_node_is_shadow_root(&self) -> bool {
        self.parent_node()
            .is_some_and(|parent| parent.as_shadow_root().is_some())
    }

    fn containing_shadow_host(&self) -> Option<Self> {
        TElement::containing_shadow(self).map(|root| root.host())
    }

    fn is_pseudo_element(&self) -> bool {
        matches!(self.data, NodeData::AnonymousBlock(_))
    }

    // These methods are implemented naively since we only threaded real nodes and not fake nodes
    // we should try and use `find` instead of this foward/backward stuff since its ugly and slow
    fn prev_sibling_element(&self) -> Option<Self> {
        let mut n = 1;
        while let Some(node) = self.0.backward(n) {
            if node.is_element() {
                return Some(BlitzNode(node));
            }
            n += 1;
        }

        None
    }

    fn next_sibling_element(&self) -> Option<Self> {
        let mut n = 1;
        while let Some(node) = self.0.forward(n) {
            if node.is_element() {
                return Some(BlitzNode(node));
            }
            n += 1;
        }

        None
    }

    fn first_element_child(&self) -> Option<Self> {
        self.0.first_element_child().map(BlitzNode)
    }

    fn is_html_element_in_html_document(&self) -> bool {
        // Attribute-name case sensitivity depends on both the element's
        // namespace and the document's mode: an XHTML document's elements are
        // in the HTML namespace, but their attribute names stay case-sensitive.
        // The mode is stamped on each element so a detached element still
        // knows it
        // (<https://drafts.csswg.org/selectors-4/#attribute-case>).
        self.element_data()
            .is_some_and(|element| element.name.ns == ns!(html) && element.in_html_document)
    }

    fn has_local_name(&self, local_name: &LocalName) -> bool {
        self.data.is_element_with_tag_name(local_name)
    }

    fn has_namespace(&self, ns: &Namespace) -> bool {
        self.element_data().expect("Not an element").name.ns == *ns
    }

    fn is_same_type(&self, other: &Self) -> bool {
        self.local_name() == other.local_name() && self.namespace() == other.namespace()
    }

    fn attr_matches(
        &self,
        ns: &NamespaceConstraint<&GenericAtomIdent<NamespaceStaticSet>>,
        local_name: &GenericAtomIdent<LocalNameStaticSet>,
        operation: &AttrSelectorOperation<&AtomString>,
    ) -> bool {
        let Some(attrs) = self.data.attrs() else {
            return false;
        };
        attrs.iter().any(|attr| {
            attr.name.local == local_name.0
                && match ns {
                    NamespaceConstraint::Any => true,
                    NamespaceConstraint::Specific(ns) => attr.name.ns == ns.0,
                }
                && operation.eval_str(&attr.value)
        })
    }

    fn match_non_ts_pseudo_class(
        &self,
        pseudo_class: &<Self::Impl as selectors::SelectorImpl>::NonTSPseudoClass,
        _context: &mut MatchingContext<Self::Impl>,
    ) -> bool {
        match *pseudo_class {
            NonTSPseudoClass::Active => self.element_state().contains(ElementState::ACTIVE),
            NonTSPseudoClass::AnyLink => self
                .element_state()
                .intersects(ElementState::VISITED_OR_UNVISITED),
            NonTSPseudoClass::Checked => self.element_state().contains(ElementState::CHECKED),
            NonTSPseudoClass::Valid => false,
            NonTSPseudoClass::Invalid => false,
            NonTSPseudoClass::Defined => false,
            NonTSPseudoClass::Disabled => self.element_state().contains(ElementState::DISABLED),
            NonTSPseudoClass::Enabled => self.element_state().contains(ElementState::ENABLED),
            NonTSPseudoClass::Focus => self.element_state().contains(ElementState::FOCUS),
            NonTSPseudoClass::FocusWithin => {
                self.element_state().contains(ElementState::FOCUS_WITHIN)
            }
            NonTSPseudoClass::FocusVisible => {
                self.element_state().contains(ElementState::FOCUSRING)
            }
            NonTSPseudoClass::Fullscreen => false,
            NonTSPseudoClass::Hover => self.element_state().contains(ElementState::HOVER),
            NonTSPseudoClass::Indeterminate => {
                self.element_state().contains(ElementState::INDETERMINATE)
            }
            // The element's language comes from the nearest `lang`/`xml:lang`
            // ancestor (<https://drafts.csswg.org/selectors-4/#the-lang-pseudo>).
            NonTSPseudoClass::Lang(ref lang) => self.match_element_lang(None, lang),
            NonTSPseudoClass::CustomState(_) => false,
            NonTSPseudoClass::Link => self.element_state().contains(ElementState::UNVISITED),
            // The bit is maintained by `ElementData::sync_placeholder_shown`.
            // Stylo shares styles by `ElementState`, so matching has to read
            // the bit rather than recompute from attributes.
            // <https://drafts.csswg.org/selectors/#placeholder-shown-pseudo>
            NonTSPseudoClass::PlaceholderShown => self
                .element_state()
                .contains(ElementState::PLACEHOLDER_SHOWN),
            // The bits are maintained by `ElementData::sync_form_selector_state`
            // <https://html.spec.whatwg.org/multipage/semantics-other.html#selector-read-write>
            NonTSPseudoClass::ReadWrite => self.element_state().contains(ElementState::READWRITE),
            NonTSPseudoClass::ReadOnly => self.element_state().contains(ElementState::READONLY),
            NonTSPseudoClass::ServoNonZeroBorder => false,
            // The document sets the bit on the element its URL fragment names
            // (`BaseDocument::set_url_target`)
            // <https://drafts.csswg.org/selectors-4/#the-target-pseudo>
            NonTSPseudoClass::Target => self.element_state().contains(ElementState::URLTARGET),
            NonTSPseudoClass::Visited => false,
            NonTSPseudoClass::Autofill => false,
            NonTSPseudoClass::Default => false,

            NonTSPseudoClass::InRange => false,
            NonTSPseudoClass::Modal => false,
            NonTSPseudoClass::MozTopmostModal => false,
            NonTSPseudoClass::MozInert => self.element_state().contains(ElementState::INERT),
            NonTSPseudoClass::Open => self.element_state().contains(ElementState::OPEN),
            // The bits are maintained by `ElementData::sync_form_selector_state`
            // <https://html.spec.whatwg.org/multipage/semantics-other.html#selector-required>
            NonTSPseudoClass::Optional => self.element_state().contains(ElementState::OPTIONAL_),
            NonTSPseudoClass::OutOfRange => false,
            NonTSPseudoClass::PopoverOpen => false,
            NonTSPseudoClass::Required => self.element_state().contains(ElementState::REQUIRED),
            NonTSPseudoClass::UserInvalid => false,
            NonTSPseudoClass::UserValid => false,
            NonTSPseudoClass::MozMeterOptimum => false,
            NonTSPseudoClass::MozMeterSubOptimum => false,
            NonTSPseudoClass::MozMeterSubSubOptimum => false,
        }
    }

    fn match_pseudo_element(
        &self,
        pe: &PseudoElement,
        _context: &mut MatchingContext<Self::Impl>,
    ) -> bool {
        let pseudo = match self.try_stylo_element_data().and_then(|s| s.get()) {
            Some(el) => el
                .styles
                .get_primary()
                .and_then(|s| s.pseudo())
                .or(match &self.data {
                    NodeData::AnonymousBlock(_) => Some(PseudoElement::ServoAnonymousBox),
                    _ => None,
                }),
            None => None,
        };

        pseudo.is_some_and(|psuedo| psuedo == *pe)
    }

    fn apply_selector_flags(&self, flags: ElementSelectorFlags) {
        // Handle flags that apply to the element.
        let self_flags = flags.for_self();
        if !self_flags.is_empty() {
            self.selector_flags()
                .set(self.selector_flags().get() | self_flags);
        }

        // Handle flags that apply to the parent.
        let parent_flags = flags.for_parent();
        if !parent_flags.is_empty() {
            if let Some(parent) = self.parent_node() {
                parent
                    .selector_flags()
                    .set(parent.selector_flags().get() | parent_flags);
            }
        }
    }

    fn is_link(&self) -> bool {
        self.data.is_element_with_tag_name(&local_name!("a"))
    }

    fn is_html_slot_element(&self) -> bool {
        self.element_data()
            .is_some_and(|el| el.name.local == local_name!("slot") && el.name.ns == ns!(html))
    }

    // https://dom.spec.whatwg.org/#find-a-slot
    fn assigned_slot(&self) -> Option<Self> {
        let parent = selectors::Element::parent_element(self)?;
        let shadow = parent.shadow_root()?;
        let desired = self.0.attr(local_name!("slot")).unwrap_or("");
        let mut stack: Vec<blitz_traits::node_id::NodeId> =
            shadow.0.children.iter().rev().copied().collect();
        while let Some(id) = stack.pop() {
            let node = self.0.with(id);
            if let Some(el) = node.element_data() {
                if el.name.local == local_name!("slot") && el.name.ns == ns!(html) {
                    let slot_name = el.attr(local_name!("name")).unwrap_or("");
                    if slot_name == desired {
                        return Some(BlitzNode(node));
                    }
                }
            }
            stack.extend(node.children.iter().rev().copied());
        }
        None
    }

    fn has_id(
        &self,
        id: &<Self::Impl as selectors::SelectorImpl>::Identifier,
        case_sensitivity: selectors::attr::CaseSensitivity,
    ) -> bool {
        self.element_data()
            .and_then(|data| data.id.as_ref())
            .map(|id_attr| case_sensitivity.eq_atom(id_attr, id))
            .unwrap_or(false)
    }

    fn has_class(
        &self,
        search_name: &<Self::Impl as selectors::SelectorImpl>::Identifier,
        case_sensitivity: selectors::attr::CaseSensitivity,
    ) -> bool {
        self.element_data().is_some_and(|data| {
            data.classes
                .iter()
                .any(|class| case_sensitivity.eq_atom(class, search_name))
        })
    }

    fn imported_part(
        &self,
        _name: &<Self::Impl as selectors::SelectorImpl>::Identifier,
    ) -> Option<<Self::Impl as selectors::SelectorImpl>::Identifier> {
        None
    }

    fn is_part(&self, name: &<Self::Impl as selectors::SelectorImpl>::Identifier) -> bool {
        let target: &str = name.as_ref();
        self.0
            .attr(local_name!("part"))
            .is_some_and(|attr| attr.split_ascii_whitespace().any(|token| token == target))
    }

    fn is_empty(&self) -> bool {
        // Comments, processing instructions, and other non-content nodes do
        // not affect emptiness; only elements and content nodes with data of a
        // non-zero length do
        // (<https://drafts.csswg.org/selectors-4/#the-empty-pseudo>).
        self.dom_children().all(|child| match &child.data {
            NodeData::Element(_) | NodeData::AnonymousBlock(_) => false,
            NodeData::Text(text) => text.content.is_empty(),
            NodeData::CDataSection { contents } => contents.is_empty(),
            _ => true,
        })
    }

    fn is_root(&self) -> bool {
        // `:root` matches the document element only: the top of a detached
        // tree or a shadow tree is not the root of a document
        // (<https://drafts.csswg.org/selectors-4/#the-root-pseudo>).
        self.parent_node()
            .is_some_and(|parent| matches!(parent.data, NodeData::Document(_)))
    }

    fn has_custom_state(
        &self,
        _name: &<Self::Impl as selectors::SelectorImpl>::Identifier,
    ) -> bool {
        false
    }

    fn add_element_unique_hashes(&self, filter: &mut selectors::bloom::BloomFilter) -> bool {
        each_relevant_element_hash(*self, |hash| filter.insert_hash(hash & BLOOM_HASH_MASK));
        true
    }
}

/// The language of `element`, inherited from the nearest ancestor with a
/// `lang` or `xml:lang` attribute. An inherited empty value means the language
/// is unknown
/// (<https://drafts.csswg.org/selectors-4/#the-lang-pseudo>).
fn inherited_lang(element: &BlitzNode<'_>) -> Option<style::selector_parser::AttrValue> {
    let mut current = Some(*element);
    while let Some(node) = current {
        if let Some(lang) = node.lang_attr() {
            return Some(lang);
        }
        current = TNode::parent_element(&node);
    }
    None
}

/// Whether `tag` matches the language range `range`: a case-insensitive exact
/// match, a prefix that ends on a subtag boundary, or the `*` wildcard
/// (<https://drafts.csswg.org/selectors-4/#the-lang-pseudo>).
fn language_tag_matches(tag: &str, range: &str) -> bool {
    if range == "*" {
        return !tag.is_empty();
    }
    tag.eq_ignore_ascii_case(range)
        || (tag.len() > range.len()
            && tag.is_char_boundary(range.len())
            && tag.as_bytes().get(range.len()) == Some(&b'-')
            && tag[..range.len()].eq_ignore_ascii_case(range))
}

impl<'a> TElement for BlitzNode<'a> {
    type ConcreteNode = BlitzNode<'a>;

    type TraversalChildrenIterator = Traverser<'a>;

    fn as_node(&self) -> Self::ConcreteNode {
        *self
    }

    fn implicit_scope_for_sheet_in_shadow_root(
        opaque_host: OpaqueElement,
        _sheet_index: usize,
    ) -> Option<ImplicitScopeRoot> {
        Some(ImplicitScopeRoot::InShadowTree(opaque_host))
    }

    fn traversal_children(&self) -> style::dom::LayoutIterator<Self::TraversalChildrenIterator> {
        LayoutIterator(Traverser {
            // dom: self.tree(),
            parent: *self,
            child_index: 0,
        })
    }

    fn is_html_element(&self) -> bool {
        self.is_element()
    }

    // not implemented.....
    fn is_mathml_element(&self) -> bool {
        false
    }

    // need to check the namespace
    fn is_svg_element(&self) -> bool {
        false
    }

    fn style_attribute(&self) -> Option<ArcBorrow<'_, Locked<PropertyDeclarationBlock>>> {
        self.element_data()
            .expect("Not an element")
            .style_attribute
            .as_ref()
            .map(|f| f.borrow_arc())
    }

    fn state(&self) -> ElementState {
        *self.element_state()
    }

    fn has_part_attr(&self) -> bool {
        self.0.attr(local_name!("part")).is_some()
    }

    fn each_part<F>(&self, mut callback: F)
    where
        F: FnMut(&style::values::AtomIdent),
    {
        if let Some(attr) = self.0.attr(local_name!("part")) {
            for token in attr.split_ascii_whitespace() {
                let atom = style::Atom::from(token);
                callback(AtomIdent::cast(&atom));
            }
        }
    }

    fn exports_any_part(&self) -> bool {
        false
    }

    fn id(&self) -> Option<&style::Atom> {
        self.element_data().and_then(|data| data.id.as_ref())
    }

    fn each_class<F>(&self, mut callback: F)
    where
        F: FnMut(&style::values::AtomIdent),
    {
        if let Some(data) = self.element_data() {
            for class in &data.classes {
                callback(AtomIdent::cast(class));
            }
        }
    }

    fn each_attr_name<F>(&self, mut callback: F)
    where
        F: FnMut(&style::LocalName),
    {
        if let Some(attrs) = self.data.attrs() {
            for attr in attrs.iter() {
                callback(GenericAtomIdent::cast(&attr.name.local));
            }
        }
    }

    fn has_dirty_descendants(&self) -> bool {
        Node::has_dirty_descendants(self)
    }

    fn has_snapshot(&self) -> bool {
        Node::has_snapshot(self)
    }

    fn handled_snapshot(&self) -> bool {
        self.snapshot_handled().load(Ordering::SeqCst)
    }

    unsafe fn set_handled_snapshot(&self) {
        self.snapshot_handled().store(true, Ordering::SeqCst);
    }

    // Stylo calls this on elements it is already traversing (or has already
    // reached via invalidation), so the ancestor chain must not be re-marked:
    // ancestors visited earlier in the preorder traversal have had their bits
    // cleared, and re-flagging them leaves stale `dirty_descendants` bits that
    // break the early-out invariant of `Node::mark_ancestors_dirty` (a set bit
    // implies all ancestors are set).
    unsafe fn set_dirty_descendants(&self) {
        Node::set_dirty_descendants(self);
    }

    unsafe fn unset_dirty_descendants(&self) {
        Node::unset_dirty_descendants(self);
    }

    fn store_children_to_process(&self, _n: isize) {
        unimplemented!()
    }

    fn did_process_child(&self) -> isize {
        unimplemented!()
    }

    unsafe fn ensure_data(&self) -> ElementDataMut<'_> {
        // SAFETY: stylo traversal has exclusive access to nodes
        unsafe { self.stylo_element_data().ensure_init() }
    }

    unsafe fn clear_data(&self) {
        // SAFETY: stylo traversal has exclusive access to nodes
        unsafe { self.stylo_element_data().clear() }
    }

    fn has_data(&self) -> bool {
        self.try_stylo_element_data().is_some_and(|s| s.has_data())
    }

    fn borrow_data(&self) -> Option<ElementDataRef<'_>> {
        self.try_stylo_element_data().and_then(|s| s.get())
    }

    fn mutate_data(&self) -> Option<ElementDataMut<'_>> {
        self.try_stylo_element_data()
            .and_then(|s| unsafe { s.unsafe_stylo_only_mut() })
    }

    fn skip_item_display_fixup(&self) -> bool {
        false
    }

    fn may_have_animations(&self) -> bool {
        self.element_data()
            .is_some_and(|data| data.may_have_animations.load(Ordering::Relaxed))
    }

    fn set_may_have_animations(&self) {
        if let Some(data) = self.element_data() {
            data.may_have_animations.store(true, Ordering::Relaxed);
        }
    }

    fn has_animations(&self, context: &SharedStyleContext) -> bool {
        if !self.may_have_animations() {
            return false;
        }
        let key = AnimationSetKey::new_for_non_pseudo(TNode::opaque(&TElement::as_node(self)));
        context
            .animations
            .has_active_animations_or_transitions(&key)
    }

    fn has_css_animations(
        &self,
        context: &SharedStyleContext,
        pseudo_element: Option<PseudoElement>,
    ) -> bool {
        if !self.may_have_animations() {
            return false;
        }
        let key = AnimationSetKey::new(TNode::opaque(&TElement::as_node(self)), pseudo_element);
        context.animations.has_active_animations(&key)
    }

    fn has_css_transitions(
        &self,
        context: &SharedStyleContext,
        pseudo_element: Option<PseudoElement>,
    ) -> bool {
        if !self.may_have_animations() {
            return false;
        }
        let key = AnimationSetKey::new(TNode::opaque(&TElement::as_node(self)), pseudo_element);
        context.animations.has_active_transitions(&key)
    }

    fn animation_rule(
        &self,
        context: &SharedStyleContext,
    ) -> Option<Arc<Locked<PropertyDeclarationBlock>>> {
        let opaque = TNode::opaque(&TElement::as_node(self));
        context.animations.get_animation_declarations(
            &AnimationSetKey::new_for_non_pseudo(opaque),
            context.current_time_for_animations,
            self.guard(),
        )
    }

    fn transition_rule(
        &self,
        context: &SharedStyleContext,
    ) -> Option<Arc<Locked<PropertyDeclarationBlock>>> {
        let opaque = TNode::opaque(&TElement::as_node(self));
        context.animations.get_transition_declarations(
            &AnimationSetKey::new_for_non_pseudo(opaque),
            context.current_time_for_animations,
            self.guard(),
        )
    }

    fn shadow_root(&self) -> Option<<Self::ConcreteNode as TNode>::ConcreteShadowRoot> {
        let id = self.0.data.downcast_element()?.shadow_root?;
        Some(BlitzNode(self.0.with(id)))
    }

    fn containing_shadow(&self) -> Option<<Self::ConcreteNode as TNode>::ConcreteShadowRoot> {
        let mut current = self.parent_node();
        while let Some(node) = current {
            if node.as_shadow_root().is_some() {
                return Some(node);
            }
            current = node.parent_node();
        }
        None
    }

    fn inheritance_parent(&self) -> Option<Self> {
        TElement::traversal_parent(self)
    }

    fn get_attr(&self, attr: &style::LocalName, ns: &style::Namespace) -> Option<String> {
        // TODO: case-insensitive matching for HTML-ns attrs
        self.attrs()?
            .iter()
            .find(|a| a.name.local == attr.0 && a.name.ns == ns.0)
            .map(|a| a.value.clone())
    }

    fn lang_attr(&self) -> Option<style::selector_parser::AttrValue> {
        // `xml:lang` wins over `lang`, and in HTML the `lang` attribute is in
        // the null namespace
        // (<https://html.spec.whatwg.org/multipage/dom.html#the-lang-and-xml:lang-attributes>).
        let attrs = self.attrs()?;
        attrs
            .iter()
            .find(|attr| attr.name.local == local_name!("lang") && attr.name.ns == ns!(xml))
            .or_else(|| {
                attrs
                    .iter()
                    .find(|attr| attr.name.local == local_name!("lang") && attr.name.ns.is_empty())
            })
            .map(|attr| style::selector_parser::AttrValue::from(attr.value.as_str()))
    }

    fn match_element_lang(
        &self,
        override_lang: Option<Option<style::selector_parser::AttrValue>>,
        value: &style::selector_parser::Lang,
    ) -> bool {
        // The element's language is its nearest `lang`/`xml:lang` ancestor
        // value; an inherited empty value means the language is unknown and
        // stops the walk
        // (<https://drafts.csswg.org/selectors-4/#the-lang-pseudo>).
        let lang = match override_lang {
            Some(lang) => lang,
            None => inherited_lang(self),
        };
        lang.is_some_and(|lang| language_tag_matches(lang.as_ref(), value))
    }

    fn is_html_document_body_element(&self) -> bool {
        // Check node is a <body> element
        let is_body_element = self.data.is_element_with_tag_name(&local_name!("body"));

        // If it isn't then return early
        if !is_body_element {
            return false;
        }

        // If it is then check if it is a child of the root (<html>) element
        let root_element = self.0.owner_doc().first_element_child().unwrap();
        root_element.children.contains(&self.id)
    }

    fn synthesize_presentational_hints_for_legacy_attributes<'h, V>(
        &self,
        _visited_handling: VisitedHandlingMode,
        hints: &mut V,
    ) where
        V: Push<ApplicableDeclarationBlock<'h>>,
        Self: 'h,
    {
        super::presentational_hints::synthesize(self.0, hints);
    }

    fn local_name(&self) -> &LocalName {
        &self.element_data().expect("Not an element").name.local
    }

    fn namespace(&self) -> &Namespace {
        &self.element_data().expect("Not an element").name.ns
    }

    fn query_container_size(
        &self,
        _display: &style::values::specified::Display,
    ) -> euclid::default::Size2D<Option<app_units::Au>> {
        // FIXME: Implement container queries. For now this effectively disables them without panicking.
        Default::default()
    }

    fn each_custom_state<F>(&self, _callback: F)
    where
        F: FnMut(&AtomIdent),
    {
        todo!()
    }

    fn has_selector_flags(&self, flags: ElementSelectorFlags) -> bool {
        self.selector_flags().get().contains(flags)
    }

    fn relative_selector_search_direction(&self) -> ElementSelectorFlags {
        let flags = self.selector_flags().get();
        if flags.contains(ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR_SIBLING)
        {
            ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR_SIBLING
        } else if flags.contains(ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR)
        {
            ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR
        } else if flags.contains(ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_SIBLING) {
            ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_SIBLING
        } else {
            ElementSelectorFlags::empty()
        }
    }

    fn compute_layout_damage(old: &ComputedValues, new: &ComputedValues) -> RestyleDamage {
        compute_layout_damage(old, new)
        // ALL_DAMAGE
    }

    // fn update_animations(
    //     &self,
    //     before_change_style: Option<Arc<ComputedValues>>,
    //     tasks: style::context::UpdateAnimationsTasks,
    // ) {
    //     todo!()
    // }

    // fn process_post_animation(&self, tasks: style::context::PostAnimationTasks) {
    //     todo!()
    // }

    // fn needs_transitions_update(
    //     &self,
    //     before_change_style: &ComputedValues,
    //     after_change_style: &ComputedValues,
    // ) -> bool {
    //     todo!()
    // }
}

pub(crate) struct Traverser<'a> {
    // dom: &'a Slab<Node>,
    parent: BlitzNode<'a>,
    child_index: usize,
}

impl<'a> Iterator for Traverser<'a> {
    type Item = BlitzNode<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let flat = self.parent.flat_children();
        let node_id = if let Some(id) = flat.get(self.child_index) {
            *id
        } else if self
            .parent
            .element_data()
            .is_some_and(|el| el.shadow_root.is_some())
        {
            *self.parent.children.get(self.child_index - flat.len())?
        } else {
            return None;
        };
        let node = self.parent.0.with(node_id);

        self.child_index += 1;

        Some(BlitzNode(node))
    }
}

impl std::hash::Hash for BlitzNode<'_> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_u64(self.id.as_u64())
    }
}
