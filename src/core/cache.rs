#[derive(Debug)]
struct Entry<K, V> {
    key: K,
    value: V,
    cost: usize,
    touched: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertResult {
    Inserted,
    Replaced,
    RejectedOversize,
}

/// A small, byte-budgeted LRU optimized for thumbnail counts in the tens.
/// Linear lookup avoids the capacity overhead and duplicate key storage of a
/// hash map while keeping the cache's worst-case memory explicit.
#[derive(Debug)]
pub struct TinyLru<K, V> {
    entries: Vec<Entry<K, V>>,
    used_bytes: usize,
    budget_bytes: usize,
    clock: u64,
}

impl<K: Eq, V> TinyLru<K, V> {
    #[must_use]
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            entries: Vec::new(),
            used_bytes: 0,
            budget_bytes,
            clock: 0,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub const fn used_bytes(&self) -> usize {
        self.used_bytes
    }

    #[must_use]
    pub const fn budget_bytes(&self) -> usize {
        self.budget_bytes
    }

    pub fn get(&mut self, key: &K) -> Option<&V> {
        let index = self.entries.iter().position(|entry| &entry.key == key)?;
        self.clock = self.clock.wrapping_add(1);
        self.entries[index].touched = self.clock;
        Some(&self.entries[index].value)
    }

    #[must_use]
    pub fn peek(&self, key: &K) -> Option<&V> {
        self.entries
            .iter()
            .find(|entry| &entry.key == key)
            .map(|entry| &entry.value)
    }

    pub fn insert(&mut self, key: K, value: V, cost: usize) -> InsertResult {
        if cost > self.budget_bytes {
            return InsertResult::RejectedOversize;
        }

        self.clock = self.clock.wrapping_add(1);
        let mut result = InsertResult::Inserted;
        if let Some(index) = self.entries.iter().position(|entry| entry.key == key) {
            let old = self.entries.swap_remove(index);
            self.used_bytes = self.used_bytes.saturating_sub(old.cost);
            result = InsertResult::Replaced;
        }

        self.entries.push(Entry {
            key,
            value,
            cost,
            touched: self.clock,
        });
        self.used_bytes = self.used_bytes.saturating_add(cost);
        self.evict_to_budget();
        result
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&K) -> bool) {
        let mut index = 0;
        while index < self.entries.len() {
            if keep(&self.entries[index].key) {
                index += 1;
            } else {
                let removed = self.entries.swap_remove(index);
                self.used_bytes = self.used_bytes.saturating_sub(removed.cost);
            }
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.used_bytes = 0;
    }

    fn evict_to_budget(&mut self) {
        while self.used_bytes > self.budget_bytes {
            let Some((index, _)) = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.touched)
            else {
                break;
            };
            let removed = self.entries.swap_remove(index);
            self.used_bytes = self.used_bytes.saturating_sub(removed.cost);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_least_recently_used_by_bytes() {
        let mut cache = TinyLru::new(10);
        cache.insert(1, "one", 4);
        cache.insert(2, "two", 4);
        assert_eq!(cache.get(&1), Some(&"one"));
        cache.insert(3, "three", 4);
        assert_eq!(cache.peek(&2), None);
        assert_eq!(cache.peek(&1), Some(&"one"));
        assert_eq!(cache.peek(&3), Some(&"three"));
        assert!(cache.used_bytes() <= cache.budget_bytes());
    }

    #[test]
    fn replacement_and_retain_keep_accounting_exact() {
        let mut cache = TinyLru::new(100);
        assert_eq!(cache.insert(1, 10, 40), InsertResult::Inserted);
        assert_eq!(cache.insert(1, 20, 12), InsertResult::Replaced);
        cache.insert(2, 30, 15);
        cache.retain(|key| *key == 2);
        assert_eq!(cache.used_bytes(), 15);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn stress_never_exceeds_budget() {
        let mut cache = TinyLru::new(8 * 1024);
        let mut random = 0x9E37_79B9_u32;
        for _ in 0..250_000 {
            random ^= random << 13;
            random ^= random >> 17;
            random ^= random << 5;
            let key = random % 128;
            let cost = (random as usize % 1024) + 1;
            cache.insert(key, random, cost);
            if random & 3 == 0 {
                let _ = cache.get(&key);
            }
            assert!(cache.used_bytes() <= cache.budget_bytes());
        }
    }
}
