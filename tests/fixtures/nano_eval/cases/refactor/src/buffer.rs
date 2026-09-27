pub struct Buffer {
    capacity: usize,
}

impl Buffer {
    pub fn new(capacity: usize) -> Self {
        Self { capacity }
    }

    pub fn size_hint(&self) -> usize {
        self.capacity
    }
}
