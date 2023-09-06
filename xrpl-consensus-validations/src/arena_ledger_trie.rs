use std::cmp::Ordering;
use std::collections::hash_map::Entry;
use std::collections::HashMap;

use generational_arena::{Arena, Index};

use xrpl_consensus_core::{Ledger, LedgerIndex};

use crate::ledger_trie::LedgerTrie;
use crate::span::{Span, SpanTip};

pub struct Node<T: Ledger> {
    idx: Index,
    span: Span<T>,
    tip_support: u32,
    branch_support: u32,
    children: Vec<Index>,
    parent: Option<Index>,
}

impl<T: Ledger> Node<T> {
    pub fn new(idx: Index, ledger: T) -> Self {
        Node {
            idx,
            span: Span::from(ledger),
            tip_support: 1,
            branch_support: 1,
            children: vec![],
            parent: None,
        }
    }

    pub fn with_index(idx: Index) -> Self {
        Node {
            idx,
            span: Span::default(),
            tip_support: 0,
            branch_support: 0,
            children: vec![],
            parent: None,
        }
    }

    pub fn from_span(span: Span<T>, idx: Index) -> Self {
        Node {
            idx,
            span,
            tip_support: 0,
            branch_support: 0,
            children: vec![],
            parent: None,
        }
    }
}

pub struct ArenaLedgerTrie<T: Ledger> {
    root: Index,
    arena: Arena<Node<T>>,
    seq_support: HashMap<LedgerIndex, u32>,
}

impl<T: Ledger> LedgerTrie<T> for ArenaLedgerTrie<T> {
    fn insert(&mut self, ledger: &T, count: Option<u32>) {
        // Find the ID of the node with the longest common ancestry with `ledger`
        // and the sequence of the first ledger difference
        let (loc_idx, diff_seq) = self._find(ledger);

        let mut inc_node_idx = Some(loc_idx);

        // Insert a new, basically empty, Node and also get a mutable reference to both the loc node
        // and new node we inserted.
        // We have to do it this way because we need a mutable reference to both, but
        // cannot cannot call self.arena.get_mut twice without having two simultaneous
        // mutable borrows of self.arena, which would break Rust's ownership rules.
        let (loc, new_node) = self._add_empty_and_get(loc_idx);

        let loc_idx = loc.idx;
        // loc->span has the longest common prefix with Span{ledger} of all
        // existing nodes in the trie. The optional<Span>'s below represent
        // the possible common suffixes between loc->span and Span{ledger}.
        //
        // loc->span
        //  a b c  | d e f
        //  prefix | oldSuffix
        //
        // Span{ledger}
        //  a b c  | g h i
        //  prefix | newSuffix
        let prefix = loc.span.before(diff_seq);
        let old_suffix = loc.span.after(diff_seq);
        let new_suffix = Span::from(ledger.clone()).after(diff_seq);

        if let Some(old_suffix) = old_suffix {
            // Have
            //   abcdef -> ....
            // Inserting
            //   abc
            // Becomes
            //   abc -> def -> ...

            // Set new_node's span to old_suffix and take tip_support and branch_support
            // from loc so that new_node takes over loc. new_node will be loc's child.
            new_node.span = old_suffix;
            new_node.tip_support = loc.tip_support;
            new_node.branch_support = loc.branch_support;
            new_node.parent = Some(loc.idx);

            // Replace loc's children Vec with an empty vector because we will move
            // loc's children into new_node's children. However, we need to clone
            // the children Vec into new_node.children because we later need to
            // iterate through the children, get a mutable reference to the Node
            // the child Index points to and update each child Node's parent idx to
            // point to new_node. If we simply moved loc.children into new_node.children,
            // we'd need to keep the mutable reference to new_node alive which would
            // prevent us from getting mutable references to each child Node.
            let loc_children = std::mem::replace(&mut loc.children, vec![]);
            new_node.children = loc_children.clone();

            // loc truncates to prefix and new_node is its child
            loc.span = prefix.unwrap();
            loc.children.push(new_node.idx);
            loc.tip_support = 0;

            let new_node_idx = new_node.idx;
            // Update each child node's parent field to point to new_node.
            loc_children.iter()
                .for_each(|child_idx| {
                    self.arena.get_mut(*child_idx).unwrap().parent = Some(new_node_idx)
                })
        }

        if let Some(new_suffix) = new_suffix {
            // Have
            //  abc -> ...
            // Inserting
            //  abcdef-> ...
            // Becomes
            //  abc -> ...
            //     \-> def

            // Insert a new, basically empty, Node and save its Index.
            let new_node_idx = self.arena.insert_with(|idx| {
                let new_node = Node::with_index(idx);
                new_node
            });

            // Unfortunately we need to get loc and create a new node again here because the mutable
            // borrow of self.arena created on the initial call to get2_mut can't outlive
            // the mutable borrow of arena when we update the children nodes.
            let (loc, new_node) = self._add_empty_and_get(loc_idx);
            new_node.span = new_suffix;
            new_node.parent = Some(loc_idx);
            inc_node_idx = Some(new_node.idx);
            loc.children.push(new_node.idx);
        }

        // Update branch support all the way up the trie
        let count = count.unwrap_or(1);
        self.arena.get_mut(inc_node_idx.unwrap()).unwrap().tip_support += 1;
        while inc_node_idx.is_some() {
            let inc_node = self.arena.get_mut(inc_node_idx.unwrap()).unwrap();
            inc_node.branch_support += count;
            inc_node_idx = inc_node.parent;
        }

        // Update seq support by adding count, or insert a new entry
        match self.seq_support.entry(ledger.seq()) {
            Entry::Occupied(mut entry) => {
                *entry.get_mut() += count;
            }
            Entry::Vacant(entry) => {
                entry.insert(count);
            }
        }
    }

    fn get_preferred(&self, largest_issued: LedgerIndex) -> Option<SpanTip<T>> {
        if self.empty() {
            return None;
        }

        let mut curr = self.arena.get(self.root);
        let mut done = false;

        let mut uncommitted: u32 = 0;

        let mut uncommitted_it = self.seq_support.iter();
        while curr.is_some() && !done {
            // Within a single span, the preferred by branch strategy is simply
            // to continue along the span as long as the branch support of
            // the next ledger exceeds the uncommitted support for that ledger.

            {
                // Add any initial uncommitted support prior for ledgers
                // earlier than nextSeq or earlier than largestIssued
                let mut next_seq = curr.unwrap().span.start() + 1;
                while let Some((seq, support)) = uncommitted_it.next() {
                    if *seq < std::cmp::max(next_seq, largest_issued) {
                        uncommitted += support;
                    } else {
                        break;
                    }
                }

                // Advance next_seq along the span
                while next_seq < curr.unwrap().span.end() &&
                    curr.unwrap().branch_support > uncommitted {
                    // Jump to the next seq_support change.
                    if let Some((seq, support)) = uncommitted_it.next() {
                        if *seq < curr.unwrap().span.end() {
                            next_seq = seq + 1;
                            uncommitted += support;
                        }
                    } else {
                        // Otherwise we jump to the end of the span
                        next_seq = curr.unwrap().span.end();
                    }
                }

                // We did not consume the entire span, so we have found the
                // preferred ledger
                if next_seq < curr.unwrap().span.end() {
                    return Some(curr.unwrap().span.before(next_seq)?.tip());
                }
            }

            // We have reached the end of the current span, so we need to
            // find the best child
            let mut margin = 0u32;
            let mut best: Option<&Node<T>> = None;
            if curr.unwrap().children.len() == 1 {
                best = Some(self.arena.get(*curr.unwrap().children.get(0).unwrap()).unwrap());
                margin = best?.branch_support;
            } else if !curr.unwrap().children.is_empty() { // Children length > 1
                // Sort placing children with largest branch support in the front,
                // breaking ties with the span's starting ID

                // NOTE: In C++, they sort the actual node's children vector.
                //  In rust, we can't get a mutable reference to curr because then
                //  we'd have a mutable reference to self.arena at the same time as having
                //  a shared reference to self.arena. Therefore, this code sorts a temporary
                //  clone of curr.children but does not update curr.children
                let mut children_to_sort = curr.unwrap().children[2..].to_vec();
                children_to_sort
                    .sort_by(|&index1, &index2| {
                        let node1 = self.arena.get(index1).unwrap();
                        let node2 = self.arena.get(index2).unwrap();
                        let cmp = node1.branch_support.cmp(&node2.branch_support);
                        match cmp {
                            Ordering::Equal => {
                                node1.span.start_id().cmp(&node2.span.start_id())
                            }
                            _ => cmp
                        }
                    });

                let first_child = self.arena.get(*children_to_sort.get(0).unwrap()).unwrap();
                let second_child = self.arena.get(*children_to_sort.get(1).unwrap()).unwrap();
                best = Some(first_child);
                margin = first_child.branch_support - second_child.branch_support;

                // If best holds the tie-breaker, gets one larger margin
                // since the second best needs additional branchSupport
                // to overcome the tie
                if best.unwrap().span.start_id() > second_child.span.start_id() {
                    margin += 1;
                }
            }

            // If the best child has margin exceeding the uncommitted support,
            // continue from that child, otherwise we are done
            if best.is_some() && ((margin > uncommitted) || (uncommitted == 0)) {
                curr = best;
            } else {
                done = true;
            }
        }

        return Some(curr.unwrap().span.tip())
    }

    fn tip_support(&self, ledger: &T) -> u32 {
        match self._find_by_ledger_id(ledger.id(), None) {
            None => 0,
            Some(loc) => {
                self.arena.get(loc).unwrap().tip_support
            }
        }
    }

    fn branch_support(&self, ledger: &T) -> u32 {
        let loc = self._find_by_ledger_id(ledger.id(), None);
        let loc_node = match loc {
            None => {
                let (l, diff_seq) = self._find(ledger);
                let loc_node = self.arena.get(l).unwrap();
                if !diff_seq > ledger.seq() && ledger.seq() < loc_node.span.end() {
                    Some(loc_node);
                }
                None
            }
            Some(l) => {
                Some(self.arena.get(l).unwrap())
            }
        };

        loc_node.map_or_else(
            || 0,
            |loc| loc.branch_support
        )

    }
}

impl<T: Ledger> ArenaLedgerTrie<T> {

    pub fn new() -> Self {
        let mut arena = Arena::new();
        let root = arena.insert_with(|idx| Node::with_index(idx));
        ArenaLedgerTrie {
            root,
            arena,
            seq_support: Default::default(),
        }
    }

    fn _add_empty_and_get(&mut self, loc_idx: Index) -> (&mut Node<T>, &mut Node<T>) {
        let new_node_idx = self.arena.insert_with(|idx| {
            let new_node = Node::with_index(idx);
            new_node
        });

        let (loc, new_node) = self.arena.get2_mut(loc_idx, new_node_idx);

        (loc.unwrap(), new_node.unwrap())
    }

    fn _find_by_ledger_id(&self, ledger_id: T::IdType, parent: Option<&Index>) -> Option<Index> {
        let parent = match parent {
            None => self.root,
            Some(p) => *p
        };

        let parent_node = self.arena.get(parent).unwrap();
        if ledger_id == parent_node.span.tip().id() {
            return Some(parent);
        }

        for child in &parent_node.children {
            let cl = self._find_by_ledger_id(ledger_id, Some(&child));
            if cl.is_some() {
                return cl;
            }
        }

        None

    }
    /// Find the node in the trie that represents the longest common ancestry
    /// with the given ledger.
    ///
    /// # Return
    /// A tuple of the found node's `Index` and the `LedgerIndex` of the first
    /// ledger difference.
    fn _find(&self, ledger: &T) -> (Index, LedgerIndex) {
        // Root is always defined and is in common with all ledgers
        let mut curr = self.arena.get(self.root).unwrap();

        let mut pos = curr.span.diff(ledger);

        let mut done = false;

        // Continue searching for a better span as long as the current position
        // matches the entire span
        while !done && pos == curr.span.end() {
            done = true;

            for child_idx in &curr.children {
                let child = self.arena.get(*child_idx).unwrap();
                let child_pos = child.span.diff(ledger);

                if child_pos > pos {
                    done = false;
                    pos = child_pos;
                    curr = child;
                    break;
                }
            }
        }

        (curr.idx, pos)
    }


    pub fn empty(&self) -> bool {
        return self.arena.get(self.root).unwrap().branch_support == 0;
    }
}

#[cfg(test)]
mod tests {
    use crate::arena_ledger_trie::ArenaLedgerTrie;
    use crate::ledger_trie::LedgerTrie;
    use crate::test_utils::ledgers::{LedgerHistoryHelper, SimulatedLedger};

    #[test]
    fn test_insert_single_entry() {
        let (mut trie, mut h) = setup();
        let ledger = h.get_or_create("abc");
        trie.insert(&ledger, None);
        assert_eq!(trie.tip_support(&ledger), 1);
        assert_eq!(trie.branch_support(&ledger), 1);

        trie.insert(&ledger, None);
        assert_eq!(trie.tip_support(&ledger), 2);
        assert_eq!(trie.branch_support(&ledger), 2);
    }

    #[test]
    fn test_insert_suffix_of_existing() {
        let (mut trie, mut h) = setup();
        let abc = h.get_or_create("abc");
        trie.insert(&abc, None);

        // extend with no siblings
        let abcd = h.get_or_create("abcd");
        trie.insert(&abcd, None);
        assert_eq!(trie.tip_support(&abc), 1);
        assert_eq!(trie.branch_support(&abc), 2);
        assert_eq!(trie.tip_support(&abcd), 1);
        assert_eq!(trie.branch_support(&abcd), 1);

        // extend with existing sibling
        let abce = h.get_or_create("abce");
        trie.insert(&abce, None);
        assert_eq!(trie.tip_support(&abc), 1);
        assert_eq!(trie.branch_support(&abc), 3);
        assert_eq!(trie.tip_support(&abcd), 1);
        assert_eq!(trie.branch_support(&abcd), 1);
        assert_eq!(trie.tip_support(&abce), 1);
        assert_eq!(trie.branch_support(&abce), 1);
    }

    #[test]
    fn test_insert_uncommitted_of_existing_node() {
        let (mut trie, mut h) = setup();
        let abcd = h.get_or_create("abcd");
        trie.insert(&abcd, None);

        // uncommitted with no siblings
        let abcdf = h.get_or_create("abcdf");
        trie.insert(&abcdf, None);
        assert_eq!(trie.tip_support(&abcd), 1);
        assert_eq!(trie.branch_support(&abcd), 2);
        assert_eq!(trie.tip_support(&abcdf), 1);
        assert_eq!(trie.branch_support(&abcdf), 1);

        // uncommitted with existing child
        let abc = h.get_or_create("abc");
        trie.insert(&abc, None);
        assert_eq!(trie.tip_support(&abc), 1);
        assert_eq!(trie.branch_support(&abc), 3);
        assert_eq!(trie.tip_support(&abcd), 1);
        assert_eq!(trie.branch_support(&abcd), 2);
        assert_eq!(trie.tip_support(&abcdf), 1);
        assert_eq!(trie.branch_support(&abcdf), 1);
    }

    #[test]
    fn test_insert_suffix_and_uncommitted_existing_node() {
        let (mut trie, mut h) = setup();
        let abcd = h.get_or_create("abcd");
        trie.insert(&abcd, None);
        let abce = h.get_or_create("abce");
        trie.insert(&abce, None);

        let abc = h.get_or_create("abc");
        assert_eq!(trie.tip_support(&abc), 0);
        assert_eq!(trie.branch_support(&abc), 2);
        assert_eq!(trie.tip_support(&abcd), 1);
        assert_eq!(trie.branch_support(&abcd), 1);
        assert_eq!(trie.tip_support(&abce), 1);
        assert_eq!(trie.branch_support(&abce), 1);
    }

    fn setup() -> (ArenaLedgerTrie<SimulatedLedger>, LedgerHistoryHelper) {
        let mut trie = ArenaLedgerTrie::new();
        let mut h = LedgerHistoryHelper::new();
        (trie, h)
    }
}