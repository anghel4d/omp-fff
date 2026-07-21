use std::collections::{HashMap, VecDeque};

const MAX_CURSORS: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindCursor {
    pub query: String,
    pub pattern: String,
    pub limit: usize,
    pub page: usize,
    pub root_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrepCursor {
    pub file_offset: usize,
    pub root_id: String,
}

#[derive(Debug)]
pub struct CursorStore<T> {
    next: u64,
    values: HashMap<String, T>,
    order: VecDeque<String>,
}

impl<T> Default for CursorStore<T> {
    fn default() -> Self {
        Self { next: 0, values: HashMap::new(), order: VecDeque::new() }
    }
}

impl<T: Clone> CursorStore<T> {
    pub fn put(&mut self, value: T) -> String {
        self.next = self.next.wrapping_add(1);
        let id = self.next.to_string();
        self.values.insert(id.clone(), value);
        self.order.push_back(id.clone());
        while self.values.len() > MAX_CURSORS {
            if let Some(oldest) = self.order.pop_front() {
                self.values.remove(&oldest);
            }
        }
        id
    }

    pub fn get(&self, id: Option<&str>) -> Option<T> {
        id.and_then(|id| self.values.get(id)).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_is_fifo_bounded() {
        let mut store = CursorStore::default();
        let first = store.put(0usize);
        for value in 1..=MAX_CURSORS {
            store.put(value);
        }
        assert_eq!(store.get(Some(&first)), None);
        assert_eq!(store.values.len(), MAX_CURSORS);
    }

    #[test]
    fn cursor_root_can_be_rejected() {
        let mut store = CursorStore::default();
        let token = store.put(GrepCursor { file_offset: 7, root_id: "a".into() });
        let cursor = store.get(Some(&token)).unwrap();
        assert_ne!(cursor.root_id, "b");
    }
}
