//! Browser-style navigation history over (file, line) locations.

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loc {
    pub path: PathBuf,
    pub line: Option<usize>,
}

#[derive(Debug, Default)]
pub struct History {
    stack: Vec<Loc>,
    index: usize, // current position within `stack`
}

impl History {
    /// Record a new location, truncating any forward entries.
    pub fn push(&mut self, loc: Loc) {
        if let Some(current) = self.stack.get(self.index)
            && *current == loc
        {
            return;
        }
        self.stack.truncate(self.index + 1); // no-op when stack is empty
        self.stack.push(loc);
        self.index = self.stack.len() - 1;
    }

    pub fn back(&mut self) -> Option<Loc> {
        if !self.can_back() {
            return None;
        }
        self.index -= 1;
        self.stack.get(self.index).cloned()
    }

    pub fn forward(&mut self) -> Option<Loc> {
        if !self.can_forward() {
            return None;
        }
        self.index += 1;
        self.stack.get(self.index).cloned()
    }

    pub fn can_back(&self) -> bool {
        !self.stack.is_empty() && self.index > 0
    }

    pub fn can_forward(&self) -> bool {
        !self.stack.is_empty() && self.index + 1 < self.stack.len()
    }

    pub fn clear(&mut self) {
        self.stack.clear();
        self.index = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(name: &str, line: Option<usize>) -> Loc {
        Loc {
            path: PathBuf::from(name),
            line,
        }
    }

    #[test]
    fn back_and_forward() {
        let mut h = History::default();
        assert!(!h.can_back() && !h.can_forward());
        h.push(loc("a", None));
        h.push(loc("b", Some(10)));
        h.push(loc("c", None));
        assert!(h.can_back() && !h.can_forward());
        assert_eq!(h.back(), Some(loc("b", Some(10))));
        assert_eq!(h.back(), Some(loc("a", None)));
        assert_eq!(h.back(), None);
        assert_eq!(h.forward(), Some(loc("b", Some(10))));
        assert!(h.can_forward());
    }

    #[test]
    fn push_truncates_forward_entries() {
        let mut h = History::default();
        h.push(loc("a", None));
        h.push(loc("b", None));
        h.back();
        h.push(loc("c", None));
        assert!(!h.can_forward());
        assert_eq!(h.back(), Some(loc("a", None)));
    }

    #[test]
    fn push_dedupes_current_location() {
        let mut h = History::default();
        h.push(loc("a", Some(1)));
        h.push(loc("a", Some(1)));
        assert!(!h.can_back());
    }
}
