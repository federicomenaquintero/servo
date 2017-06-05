/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Code related to the style sharing cache, an optimization that allows similar
//! nodes to share style without having to run selector matching twice.
//!
//! The basic setup is as follows.  We have an LRU cache of style sharing
//! candidates.  When we try to style a target element, we first check whether
//! we can quickly determine that styles match something in this cache, and if
//! so we just use the cached style information.  This check is done with a
//! StyleBloom filter set up for the target element, which may not be a correct
//! state for the cached candidate element if they're cousins instead of
//! siblings.
//!
//! The complicated part is determining that styles match.  This is subject to
//! the following constraints:
//!
//! 1) The target and candidate must be inheriting the same styles.
//! 2) The target and candidate must have exactly the same rules matching them.
//! 3) The target and candidate must have exactly the same non-selector-based
//!    style information (inline styles, presentation hints).
//! 4) The target and candidate must have exactly the same rules matching their
//!    pseudo-elements, because an element's style data points to the style
//!    data for its pseudo-elements.
//!
//! These constraints are satisfied in the following ways:
//!
//! * We check that the parents of the target and the candidate have the same
//!   computed style.  This addresses constraint 1.
//!
//! * We check that the target and candidate have the same inline style and
//!   presentation hint declarations.  This addresses constraint 3.
//!
//! * We ensure that elements that have pseudo-element styles are not inserted
//!   into the cache.  This partially addresses constraint 4.
//!
//! * We ensure that a target matches a candidate only if they have the same
//!   matching result for all selectors that target either elements or the
//!   originating elements of pseudo-elements.  This addresses the second half
//!   of constraint 4 (because it prevents a target that has pseudo-element
//!   styles from matching any candidate) as well as constraint 2.
//!
//! The actual checks that ensure that elements match the same rules are
//! conceptually split up into two pieces.  First, we do various checks on
//! elements that make sure that the set of possible rules in all selector maps
//! in the stylist (for normal styling and for pseudo-elements) that might match
//! the two elements is the same.  For example, we enforce that the target and
//! candidate must have the same localname and namespace.  Second, we have a
//! selector map of "revalidation selectors" that the stylist maintains that we
//! actually match against the target and candidate and then check whether the
//! two sets of results were the same.  Due to the up-front selector map checks,
//! we know that the target and candidate will be matched against the same exact
//! set of revalidation selectors, so the match result arrays can be compared
//! directly.
//!
//! It's very important that a selector be added to the set of revalidation
//! selectors any time there are two elements that could pass all the up-front
//! checks but match differently against some ComplexSelector in the selector.
//! If that happens, then they can have descendants that might themselves pass
//! the up-front checks but would have different matching results for the
//! selector in question.  In this case, "descendants" includes pseudo-elements,
//! so there is a single selector map of revalidation selectors that includes
//! both selectors targeting element and selectors targeting pseudo-elements.
//! This relies on matching an element against a pseudo-element-targeting
//! selector being a sensible operation that will effectively check whether that
//! element is a matching originating element for the selector.

use Atom;
use bit_vec::BitVec;
use bloom::StyleBloom;
use cache::{LRUCache, LRUCacheMutIterator};
use context::{SelectorFlagsMap, SharedStyleContext, StyleContext};
use data::{ComputedStyle, ElementData, ElementStyles};
use dom::{TElement, SendElement};
use matching::{ChildCascadeRequirement, MatchMethods};
use properties::ComputedValues;
use selectors::matching::{ElementSelectorFlags, VisitedHandlingMode, StyleRelations};
use smallvec::SmallVec;
use std::mem;
use std::ops::Deref;
use stylist::{ApplicableDeclarationBlock, Stylist};

mod checks;

/// The amount of nodes that the style sharing candidate cache should hold at
/// most.
pub const STYLE_SHARING_CANDIDATE_CACHE_SIZE: usize = 8;

/// Controls whether the style sharing cache is used.
#[derive(Clone, Copy, PartialEq)]
pub enum StyleSharingBehavior {
    /// Style sharing allowed.
    Allow,
    /// Style sharing disallowed.
    Disallow,
}

/// Some data we want to avoid recomputing all the time while trying to share
/// style.
#[derive(Debug, Default)]
pub struct ValidationData {
    /// The class list of this element.
    ///
    /// TODO(emilio): See if it's worth to sort them, or doing something else in
    /// a similar fashion as what Boris is doing for the ID attribute.
    class_list: Option<SmallVec<[Atom; 5]>>,

    /// The list of presentational attributes of the element.
    pres_hints: Option<SmallVec<[ApplicableDeclarationBlock; 5]>>,

    /// The cached result of matching this entry against the revalidation
    /// selectors.
    revalidation_match_results: Option<BitVec>,
}

impl ValidationData {
    /// Move the cached data to a new instance, and return it.
    pub fn take(&mut self) -> Self {
        mem::replace(self, Self::default())
    }

    /// Get or compute the list of presentational attributes associated with
    /// this element.
    pub fn pres_hints<E>(&mut self, element: E) -> &[ApplicableDeclarationBlock]
        where E: TElement,
    {
        if self.pres_hints.is_none() {
            let mut pres_hints = SmallVec::new();
            element.synthesize_presentational_hints_for_legacy_attributes(
                VisitedHandlingMode::AllLinksUnvisited,
                &mut pres_hints
            );
            self.pres_hints = Some(pres_hints);
        }
        &*self.pres_hints.as_ref().unwrap()
    }

    /// Get or compute the class-list associated with this element.
    pub fn class_list<E>(&mut self, element: E) -> &[Atom]
        where E: TElement,
    {
        if self.class_list.is_none() {
            let mut class_list = SmallVec::<[Atom; 5]>::new();
            element.each_class(|c| class_list.push(c.clone()));
            // Assuming there are a reasonable number of classes (we use the
            // inline capacity as "reasonable number"), sort them to so that
            // we don't mistakenly reject sharing candidates when one element
            // has "foo bar" and the other has "bar foo".
            if !class_list.spilled() {
                class_list.sort_by(|a, b| a.get_hash().cmp(&b.get_hash()));
            }
            self.class_list = Some(class_list);
        }
        &*self.class_list.as_ref().unwrap()
    }

    /// Computes the revalidation results if needed, and returns it.
    /// Inline so we know at compile time what bloom_known_valid is.
    #[inline]
    fn revalidation_match_results<E, F>(
        &mut self,
        element: E,
        stylist: &Stylist,
        bloom: &StyleBloom<E>,
        bloom_known_valid: bool,
        flags_setter: &mut F
    ) -> &BitVec
        where E: TElement,
              F: FnMut(&E, ElementSelectorFlags),
    {
        if self.revalidation_match_results.is_none() {
            // The bloom filter may already be set up for our element.
            // If it is, use it.  If not, we must be in a candidate
            // (i.e. something in the cache), and the element is one
            // of our cousins, not a sibling.  In that case, we'll
            // just do revalidation selector matching without a bloom
            // filter, to avoid thrashing the filter.
            let bloom_to_use = if bloom_known_valid {
                debug_assert_eq!(bloom.current_parent(),
                                 element.parent_element());
                Some(bloom.filter())
            } else {
                if bloom.current_parent() == element.parent_element() {
                    Some(bloom.filter())
                } else {
                    None
                }
            };
            self.revalidation_match_results =
                Some(stylist.match_revalidation_selectors(&element,
                                                          bloom_to_use,
                                                          flags_setter));
        }

        self.revalidation_match_results.as_ref().unwrap()
    }
}

/// Information regarding a style sharing candidate, that is, an entry in the
/// style sharing cache.
///
/// Note that this information is stored in TLS and cleared after the traversal,
/// and once here, the style information of the element is immutable, so it's
/// safe to access.
#[derive(Debug)]
pub struct StyleSharingCandidate<E: TElement> {
    /// The element. We use SendElement here so that the cache may live in
    /// ScopedTLS.
    element: SendElement<E>,
    validation_data: ValidationData,
}

impl<E: TElement> Deref for StyleSharingCandidate<E> {
    type Target = E;

    fn deref(&self) -> &Self::Target {
        &self.element
    }
}


impl<E: TElement> StyleSharingCandidate<E> {
    /// Get the classlist of this candidate.
    fn class_list(&mut self) -> &[Atom] {
        self.validation_data.class_list(*self.element)
    }

    /// Get the pres hints of this candidate.
    fn pres_hints(&mut self) -> &[ApplicableDeclarationBlock] {
        self.validation_data.pres_hints(*self.element)
    }

    /// Compute the bit vector of revalidation selector match results
    /// for this candidate.
    fn revalidation_match_results(
        &mut self,
        stylist: &Stylist,
        bloom: &StyleBloom<E>,
    ) -> &BitVec {
        self.validation_data.revalidation_match_results(
            *self.element,
            stylist,
            bloom,
            /* bloom_known_valid = */ false,
            &mut |_, _| {})
    }
}

impl<E: TElement> PartialEq<StyleSharingCandidate<E>> for StyleSharingCandidate<E> {
    fn eq(&self, other: &Self) -> bool {
        self.element == other.element
    }
}

/// An element we want to test against the style sharing cache.
pub struct StyleSharingTarget<E: TElement> {
    element: E,
    validation_data: ValidationData,
}

impl<E: TElement> Deref for StyleSharingTarget<E> {
    type Target = E;

    fn deref(&self) -> &Self::Target {
        &self.element
    }
}

impl<E: TElement> StyleSharingTarget<E> {
    /// Trivially construct a new StyleSharingTarget to test against the cache.
    pub fn new(element: E) -> Self {
        Self {
            element: element,
            validation_data: ValidationData::default(),
        }
    }

    fn class_list(&mut self) -> &[Atom] {
        self.validation_data.class_list(self.element)
    }

    /// Get the pres hints of this candidate.
    fn pres_hints(&mut self) -> &[ApplicableDeclarationBlock] {
        self.validation_data.pres_hints(self.element)
    }

    fn revalidation_match_results(
        &mut self,
        stylist: &Stylist,
        bloom: &StyleBloom<E>,
        selector_flags_map: &mut SelectorFlagsMap<E>
    ) -> &BitVec {
        // It's important to set the selector flags. Otherwise, if we succeed in
        // sharing the style, we may not set the slow selector flags for the
        // right elements (which may not necessarily be |element|), causing
        // missed restyles after future DOM mutations.
        //
        // Gecko's test_bug534804.html exercises this. A minimal testcase is:
        // <style> #e:empty + span { ... } </style>
        // <span id="e">
        //   <span></span>
        // </span>
        // <span></span>
        //
        // The style sharing cache will get a hit for the second span. When the
        // child span is subsequently removed from the DOM, missing selector
        // flags would cause us to miss the restyle on the second span.
        let element = self.element;
        let mut set_selector_flags = |el: &E, flags: ElementSelectorFlags| {
            element.apply_selector_flags(selector_flags_map, el, flags);
        };

        self.validation_data.revalidation_match_results(
            self.element,
            stylist,
            bloom,
            /* bloom_known_valid = */ true,
            &mut set_selector_flags)
    }

    /// Attempts to share a style with another node.
    pub fn share_style_if_possible(
        mut self,
        context: &mut StyleContext<E>,
        data: &mut ElementData)
        -> StyleSharingResult
    {
        let shared_context = &context.shared;
        let selector_flags_map = &mut context.thread_local.selector_flags;
        let bloom_filter = &context.thread_local.bloom_filter;

        debug_assert_eq!(bloom_filter.current_parent(),
                         self.element.parent_element());

        let result = context.thread_local
            .style_sharing_candidate_cache
            .share_style_if_possible(shared_context,
                                     selector_flags_map,
                                     bloom_filter,
                                     &mut self,
                                     data);


        context.thread_local.current_element_info.as_mut().unwrap().validation_data =
            self.validation_data.take();
        result
    }
}

/// A cache miss result.
#[derive(Clone, Debug)]
pub enum CacheMiss {
    /// The parents don't match.
    Parent,
    /// One element was NAC, while the other wasn't.
    NativeAnonymousContent,
    /// The local name of the element and the candidate don't match.
    LocalName,
    /// The namespace of the element and the candidate don't match.
    Namespace,
    /// One of the element or the candidate was a link, but the other one
    /// wasn't.
    Link,
    /// The element and the candidate match different kind of rules. This can
    /// only happen in Gecko.
    UserAndAuthorRules,
    /// The element and the candidate are in a different state.
    State,
    /// The element had an id attribute, which qualifies for a unique style.
    IdAttr,
    /// The element had a style attribute, which qualifies for a unique style.
    StyleAttr,
    /// The element and the candidate class names didn't match.
    Class,
    /// The presentation hints didn't match.
    PresHints,
    /// The element and the candidate didn't match the same set of revalidation
    /// selectors.
    Revalidation,
}

/// The results of attempting to share a style.
pub enum StyleSharingResult {
    /// We didn't find anybody to share the style with.
    CannotShare,
    /// The node's style can be shared. The integer specifies the index in the
    /// LRU cache that was hit and the damage that was done. The
    /// `ChildCascadeRequirement` indicates whether style changes due to using
    /// the shared style mean we need to recascade to children.
    StyleWasShared(usize, ChildCascadeRequirement),
}

/// An LRU cache of the last few nodes seen, so that we can aggressively try to
/// reuse their styles.
///
/// Note that this cache is flushed every time we steal work from the queue, so
/// storing nodes here temporarily is safe.
pub struct StyleSharingCandidateCache<E: TElement> {
    cache: LRUCache<[StyleSharingCandidate<E>; STYLE_SHARING_CANDIDATE_CACHE_SIZE + 1]>,
}

impl<E: TElement> StyleSharingCandidateCache<E> {
    /// Create a new style sharing candidate cache.
    pub fn new() -> Self {
        StyleSharingCandidateCache {
            cache: LRUCache::new(),
        }
    }

    /// Returns the number of entries in the cache.
    pub fn num_entries(&self) -> usize {
        self.cache.num_entries()
    }

    fn iter_mut(&mut self) -> LRUCacheMutIterator<StyleSharingCandidate<E>> {
        self.cache.iter_mut()
    }

    /// Tries to insert an element in the style sharing cache.
    ///
    /// Fails if we know it should never be in the cache.
    pub fn insert_if_possible(&mut self,
                              element: &E,
                              style: &ComputedValues,
                              relations: StyleRelations,
                              mut validation_data: ValidationData) {
        use selectors::matching::AFFECTED_BY_PRESENTATIONAL_HINTS;

        let parent = match element.parent_element() {
            Some(element) => element,
            None => {
                debug!("Failing to insert to the cache: no parent element");
                return;
            }
        };

        if element.is_native_anonymous() {
            debug!("Failing to insert into the cache: NAC");
            return;
        }

        // These are things we don't check in the candidate match because they
        // are either uncommon or expensive.
        if !checks::relations_are_shareable(&relations) {
            debug!("Failing to insert to the cache: {:?}", relations);
            return;
        }

        let box_style = style.get_box();
        if box_style.specifies_transitions() {
            debug!("Failing to insert to the cache: transitions");
            return;
        }

        if box_style.specifies_animations() {
            debug!("Failing to insert to the cache: animations");
            return;
        }

        // Take advantage of the information we've learned during
        // selector-matching.
        if !relations.intersects(AFFECTED_BY_PRESENTATIONAL_HINTS) {
            debug_assert!(validation_data.pres_hints.as_ref().map_or(true, |v| v.is_empty()));
            validation_data.pres_hints = Some(SmallVec::new());
        }

        debug!("Inserting into cache: {:?} with parent {:?}", element, parent);

        self.cache.insert(StyleSharingCandidate {
            element: unsafe { SendElement::new(*element) },
            validation_data: validation_data,
        });
    }

    /// Touch a given index in the style sharing candidate cache.
    pub fn touch(&mut self, index: usize) {
        self.cache.touch(index);
    }

    /// Clear the style sharing candidate cache.
    pub fn clear(&mut self) {
        self.cache.evict_all()
    }

    /// Attempts to share a style with another node.
    fn share_style_if_possible(
        &mut self,
        shared_context: &SharedStyleContext,
        selector_flags_map: &mut SelectorFlagsMap<E>,
        bloom_filter: &StyleBloom<E>,
        target: &mut StyleSharingTarget<E>,
        data: &mut ElementData
    ) -> StyleSharingResult {
        if shared_context.options.disable_style_sharing_cache {
            debug!("{:?} Cannot share style: style sharing cache disabled",
                   target.element);
            return StyleSharingResult::CannotShare
        }

        if target.parent_element().is_none() {
            debug!("{:?} Cannot share style: element has no parent",
                   target.element);
            return StyleSharingResult::CannotShare
        }

        if target.is_native_anonymous() {
            debug!("{:?} Cannot share style: NAC", target.element);
            return StyleSharingResult::CannotShare;
        }

        let mut should_clear_cache = false;
        for (i, candidate) in self.iter_mut().enumerate() {
            let sharing_result =
                Self::test_candidate(
                    target,
                    candidate,
                    &shared_context,
                    bloom_filter,
                    selector_flags_map
                );

            match sharing_result {
                Ok(shared_style) => {
                    // Yay, cache hit. Share the style.

                    // Accumulate restyle damage.
                    debug_assert_eq!(data.has_styles(), data.has_restyle());
                    let old_values = data.get_styles_mut()
                                         .and_then(|s| s.primary.values.take());
                    let child_cascade_requirement =
                        target.accumulate_damage(
                            &shared_context,
                            data.get_restyle_mut(),
                            old_values.as_ref().map(|v| &**v),
                            shared_style.values(),
                            None
                        );

                    // We never put elements with pseudo style into the style
                    // sharing cache, so we can just mint an ElementStyles
                    // directly here.
                    //
                    // See https://bugzilla.mozilla.org/show_bug.cgi?id=1329361
                    let styles = ElementStyles::new(shared_style);
                    data.set_styles(styles);

                    return StyleSharingResult::StyleWasShared(i, child_cascade_requirement)
                }
                Err(miss) => {
                    debug!("Cache miss: {:?}", miss);

                    // Cache miss, let's see what kind of failure to decide
                    // whether we keep trying or not.
                    match miss {
                        // Cache miss because of parent, clear the candidate cache.
                        CacheMiss::Parent => {
                            should_clear_cache = true;
                            break;
                        },
                        // Too expensive failure, give up, we don't want another
                        // one of these.
                        CacheMiss::PresHints |
                        CacheMiss::Revalidation => break,
                        _ => {}
                    }
                }
            }
        }

        debug!("{:?} Cannot share style: {} cache entries", target.element,
               self.cache.num_entries());

        if should_clear_cache {
            self.clear();
        }

        StyleSharingResult::CannotShare
    }

    fn test_candidate(target: &mut StyleSharingTarget<E>,
                      candidate: &mut StyleSharingCandidate<E>,
                      shared: &SharedStyleContext,
                      bloom: &StyleBloom<E>,
                      selector_flags_map: &mut SelectorFlagsMap<E>)
                      -> Result<ComputedStyle, CacheMiss> {
        macro_rules! miss {
            ($miss: ident) => {
                return Err(CacheMiss::$miss);
            }
        }

        // Check that we have the same parent, or at least the same pointer
        // identity for parent computed style. The latter check allows us to
        // share style between cousins if the parents shared style.
        let parent = target.parent_element();
        let candidate_parent = candidate.element.parent_element();
        if parent != candidate_parent &&
           !checks::same_computed_values(parent, candidate_parent) {
            miss!(Parent)
        }

        if target.is_native_anonymous() {
            debug_assert!(!candidate.element.is_native_anonymous(),
                          "Why inserting NAC into the cache?");
            miss!(NativeAnonymousContent)
        }

        if *target.get_local_name() != *candidate.element.get_local_name() {
            miss!(LocalName)
        }

        if *target.get_namespace() != *candidate.element.get_namespace() {
            miss!(Namespace)
        }

        if target.is_link() != candidate.element.is_link() {
            miss!(Link)
        }

        if target.matches_user_and_author_rules() !=
            candidate.element.matches_user_and_author_rules() {
            miss!(UserAndAuthorRules)
        }

        if !checks::have_same_state_ignoring_visitedness(target.element, candidate) {
            miss!(State)
        }

        let element_id = target.element.get_id();
        let candidate_id = candidate.element.get_id();
        if element_id != candidate_id {
            // It's possible that there are no styles for either id.
            if checks::may_have_rules_for_ids(shared, element_id.as_ref(),
                                              candidate_id.as_ref()) {
                miss!(IdAttr)
            }
        }

        if !checks::have_same_style_attribute(target, candidate) {
            miss!(StyleAttr)
        }

        if !checks::have_same_class(target, candidate) {
            miss!(Class)
        }

        if !checks::have_same_presentational_hints(target, candidate) {
            miss!(PresHints)
        }

        if !checks::revalidate(target, candidate, shared, bloom,
                               selector_flags_map) {
            miss!(Revalidation)
        }

        let data = candidate.element.borrow_data().unwrap();
        debug_assert!(target.has_current_styles(&data));

        debug!("Sharing style between {:?} and {:?}",
               target.element, candidate.element);
        Ok(data.styles().primary.clone())
    }
}