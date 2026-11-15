//! CSS animation and transition events.
//!
//! Blitz ticks CSS animations and transitions on the document's animation
//! timeline. Page script only learns about the lifecycle through the DOM
//! events those ticks imply, so this module records every change the tick
//! observes and the embedder fires it in the document's realm
//! (<https://drafts.csswg.org/css-animations-1/#events>,
//! <https://drafts.csswg.org/css-transitions-1/#transition-events>).

use blitz_traits::node_id::NodeId;

/// The kind of lifecycle event, which names the DOM event type it becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimationEventKind {
    /// `animationstart`: the animation left its delay phase and began running
    /// (<https://drafts.csswg.org/css-animations-1/#eventdef-animationevent-animationstart>).
    AnimationStart,
    /// `animationiteration`: an iteration boundary was crossed, except at the
    /// end of the last iteration
    /// (<https://drafts.csswg.org/css-animations-1/#eventdef-animationevent-animationiteration>).
    AnimationIteration,
    /// `animationend`: the last iteration finished
    /// (<https://drafts.csswg.org/css-animations-1/#eventdef-animationevent-animationend>).
    AnimationEnd,
    /// `animationcancel`: the animation was canceled before it finished
    /// (<https://drafts.csswg.org/css-animations-2/#eventdef-animationevent-animationcancel>).
    AnimationCancel,
    /// `transitionrun`: the transition was created, before its delay
    /// (<https://drafts.csswg.org/css-transitions-1/#eventdef-transitionevent-transitionrun>).
    TransitionRun,
    /// `transitionstart`: the transition left its delay phase and began
    /// running
    /// (<https://drafts.csswg.org/css-transitions-1/#eventdef-transitionevent-transitionstart>).
    TransitionStart,
    /// `transitionend`: the transition finished
    /// (<https://drafts.csswg.org/css-transitions-1/#eventdef-transitionevent-transitionend>).
    TransitionEnd,
    /// `transitioncancel`: the transition was canceled before it finished
    /// (<https://drafts.csswg.org/css-transitions-1/#eventdef-transitionevent-transitioncancel>).
    TransitionCancel,
}

impl AnimationEventKind {
    /// The DOM event type this lifecycle change fires.
    pub fn event_type(self) -> &'static str {
        match self {
            Self::AnimationStart => "animationstart",
            Self::AnimationIteration => "animationiteration",
            Self::AnimationEnd => "animationend",
            Self::AnimationCancel => "animationcancel",
            Self::TransitionRun => "transitionrun",
            Self::TransitionStart => "transitionstart",
            Self::TransitionEnd => "transitionend",
            Self::TransitionCancel => "transitioncancel",
        }
    }

    /// Whether the event comes from a CSS transition rather than an animation,
    /// which picks the `TransitionEvent` interface in the realm.
    pub fn is_transition(self) -> bool {
        matches!(
            self,
            Self::TransitionRun
                | Self::TransitionStart
                | Self::TransitionEnd
                | Self::TransitionCancel
        )
    }
}

/// One recorded lifecycle event, in the order the tick observed it.
#[derive(Debug, Clone)]
pub struct PendingAnimationEvent {
    /// The element the event targets. An animation on a pseudo-element
    /// targets its originating element
    /// (<https://drafts.csswg.org/css-animations-1/#event-animationevent>).
    pub target: NodeId,
    /// Which lifecycle change happened.
    pub kind: AnimationEventKind,
    /// `animation-name` for an animation event, `property-name` for a
    /// transition event.
    pub name: String,
    /// The `pseudoElement` attribute: the empty string for an animation on the
    /// element itself, otherwise its CSS selector such as `"::before"`.
    pub pseudo_element: String,
    /// The `elapsedTime` attribute, in seconds
    /// (<https://drafts.csswg.org/css-animations-1/#dom-animationevent-elapsedtime>).
    pub elapsed_time: f64,
}

impl crate::document::BaseDocument {
    /// Take the animation and transition events the last resolve observed and
    /// empty the list. An event is reported once, by the resolve whose tick
    /// made the state change
    /// (<https://drafts.csswg.org/css-animations-1/#events>).
    pub fn take_pending_animation_events(&mut self) -> Vec<PendingAnimationEvent> {
        std::mem::take(&mut self.pending_animation_events)
    }
}

