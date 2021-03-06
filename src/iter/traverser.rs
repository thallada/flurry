use crate::{BinEntry, Node, Table};
use crossbeam::epoch::{Guard, Shared};
use std::sync::atomic::Ordering;

#[derive(Debug)]
pub(crate) struct NodeIter<'g, K, V> {
    /// Current table; update if resized
    table: Option<&'g Table<K, V>>,

    stack: Option<Box<TableStack<'g, K, V>>>,
    spare: Option<Box<TableStack<'g, K, V>>>,

    /// The last bin entry iterated over
    prev: Option<&'g Node<K, V>>,

    /// Index of bin to use next
    index: usize,

    /// Current index of initial table
    base_index: usize,

    /// Index bound for inital table
    base_limit: usize,

    /// Initial table size
    base_size: usize,

    guard: &'g Guard,
}

impl<'g, K, V> NodeIter<'g, K, V> {
    pub(crate) fn new(table: Shared<'g, Table<K, V>>, guard: &'g Guard) -> Self {
        let (table, len) = if table.is_null() {
            (None, 0)
        } else {
            // safety: flurry guarantees that a table read under a guard is never dropped or moved
            // until after that guard is dropped.
            let table = unsafe { table.deref() };
            (Some(table), table.bins.len())
        };

        Self {
            table,
            stack: None,
            spare: None,
            prev: None,
            base_size: len,
            base_index: 0,
            index: 0,
            base_limit: len,
            guard,
        }
    }

    fn push_state(&mut self, t: &'g Table<K, V>, i: usize, n: usize) {
        let mut s = self.spare.take();
        if let Some(ref mut s) = s {
            self.spare = s.next.take();
        }

        let target = TableStack {
            table: t,
            length: n,
            index: i,
            next: self.stack.take(),
        };

        self.stack = if let Some(mut s) = s {
            *s = target;
            Some(s)
        } else {
            Some(Box::new(target))
        };
    }

    fn recover_state(&mut self, mut n: usize) {
        while let Some(ref mut s) = self.stack {
            if self.index + s.length < n {
                // if we haven't checked the high "side" of this bucket,
                // then do _not_ pop the stack frame,
                // and instead moveon to that bin.
                self.index += s.length;
                break;
            }

            // we _are_ popping the stack
            let mut s = self.stack.take().expect("while let Some");
            n = s.length;
            self.index = s.index;
            self.table = Some(s.table);
            self.stack = s.next.take();

            // save stack frame for re-use
            s.next = self.spare.take();
            self.spare = Some(s);
        }

        if self.stack.is_none() {
            // move to next "part" of the top-level bin in the largest table
            self.index += self.base_size;
            if self.index >= n {
                // we've gone past the last part of this top-level bin,
                // so move to the _next_ top-level bin.
                self.base_index += 1;
                self.index = self.base_index;
            }
        }
    }
}

impl<'g, K, V> Iterator for NodeIter<'g, K, V> {
    type Item = &'g Node<K, V>;
    fn next(&mut self) -> Option<Self::Item> {
        let mut e = None;
        if let Some(prev) = self.prev {
            let next = prev.next.load(Ordering::SeqCst, self.guard);
            if !next.is_null() {
                // safety: flurry does not drop or move until after guard drop
                e = Some(
                    unsafe { next.deref() }
                        .as_node()
                        .expect("only Nodes follow a Node"),
                )
            }
        }

        loop {
            if let Some(e) = e {
                self.prev = Some(e);
                return Some(e);
            }

            // safety: flurry does not drop or move until after guard drop
            if self.base_index >= self.base_limit
                || self.table.is_none()
                || self.table.as_ref().unwrap().bins.len() <= self.index
            {
                self.prev = None;
                return None;
            }

            let t = self.table.expect("is_none in if above");
            let i = self.index;
            let n = t.bins.len();
            let bin = t.bin(i, self.guard);
            if !bin.is_null() {
                // safety: flurry does not drop or move until after guard drop
                let bin = unsafe { bin.deref() };
                match bin {
                    BinEntry::Moved(next_table) => {
                        // recurse down into the target table
                        // safety: same argument as for following Moved in BinEntry::find
                        self.table = Some(unsafe { &**next_table });
                        self.prev = None;
                        // make sure we can get back "up" to where we're at
                        self.push_state(t, i, n);
                        continue;
                    }
                    BinEntry::Node(node) => {
                        e = Some(node);
                    }
                }
            }

            if self.stack.is_some() {
                self.recover_state(n);
            } else {
                self.index = i + self.base_size;
                if self.index >= n {
                    self.base_index += 1;
                    self.index = self.base_index;
                }
            }
        }
    }
}

#[derive(Debug)]
struct TableStack<'g, K, V> {
    length: usize,
    index: usize,
    table: &'g Table<K, V>,
    next: Option<Box<TableStack<'g, K, V>>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Table;
    use crossbeam::epoch::{self, Atomic, Owned};
    use parking_lot::Mutex;

    #[test]
    fn iter_new() {
        let guard = epoch::pin();
        let iter = NodeIter::<usize, usize>::new(Shared::null(), &guard);
        assert_eq!(iter.count(), 0);
    }

    #[test]
    fn iter_empty() {
        let table = Owned::new(Table::<usize, usize> {
            bins: vec![Atomic::null(); 16].into_boxed_slice(),
        });

        let guard = epoch::pin();
        let table = table.into_shared(&guard);
        let iter = NodeIter::new(table, &guard);
        assert_eq!(iter.count(), 0);

        // safety: nothing holds on to references into the table any more
        let mut t = unsafe { table.into_owned() };
        t.drop_bins();
    }

    #[test]
    fn iter_simple() {
        let mut bins = vec![Atomic::null(); 16];
        bins[8] = Atomic::new(BinEntry::Node(Node {
            hash: 0,
            key: 0usize,
            value: Atomic::new(0usize),
            next: Atomic::null(),
            lock: Mutex::new(()),
        }));

        let table = Owned::new(Table {
            bins: bins.into_boxed_slice(),
        });

        let guard = epoch::pin();
        let table = table.into_shared(&guard);
        {
            let mut iter = NodeIter::new(table, &guard);
            let e = iter.next().unwrap();
            assert_eq!(e.key, 0);
            assert!(iter.next().is_none());
        }

        // safety: nothing holds on to references into the table any more
        let mut t = unsafe { table.into_owned() };
        t.drop_bins();
    }

    #[test]
    fn iter_fw() {
        // construct the forwarded-to table
        let mut deep_bins = vec![Atomic::null(); 16];
        deep_bins[8] = Atomic::new(BinEntry::Node(Node {
            hash: 0,
            key: 0usize,
            value: Atomic::new(0usize),
            next: Atomic::null(),
            lock: Mutex::new(()),
        }));
        let mut deep_table = Owned::new(Table {
            bins: deep_bins.into_boxed_slice(),
        });

        // construct the forwarded-from table
        let mut bins = vec![Atomic::null(); 16];
        for bin in &mut bins[8..] {
            *bin = Atomic::new(BinEntry::Moved(&*deep_table as *const _));
        }
        let table = Owned::new(Table::<usize, usize> {
            bins: bins.into_boxed_slice(),
        });

        let guard = epoch::pin();
        let table = table.into_shared(&guard);
        {
            let mut iter = NodeIter::new(table, &guard);
            let e = iter.next().unwrap();
            assert_eq!(e.key, 0);
            assert!(iter.next().is_none());
        }

        // safety: nothing holds on to references into the table any more
        let mut t = unsafe { table.into_owned() };
        t.drop_bins();
        deep_table.drop_bins();
    }
}
