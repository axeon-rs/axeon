use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct BufferPool {
    inner: Arc<Mutex<BufferPoolInner>>,
}

struct BufferPoolInner {
    buffers: Vec<Vec<u8>>,
    size: usize,
}

impl BufferPool {
    pub fn new(size: usize) -> Self {
        BufferPool {
            inner: Arc::new(Mutex::new(BufferPoolInner {
                buffers: Vec::new(),
                size,
            })),
        }
    }

    pub fn get(&self) -> Vec<u8> {
        let mut inner = self.inner.lock().unwrap();
        inner.buffers.pop().unwrap_or_else(|| Vec::with_capacity(inner.size))
    }
    
    pub fn put(&self, mut buffer: Vec<u8>) {
        let mut inner = self.inner.lock().unwrap();
        buffer.clear();
        inner.buffers.push(buffer);
    }
}

impl Default for BufferPool {
    fn default() -> Self {
        Self::new(8192) // Default buffer size of 8KB
    }
}