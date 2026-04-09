use std::sync::Arc;

pub struct ThreadPool {
    thread_pool_size: usize,
    max_queue_length: usize,
}

impl ThreadPool {
    pub fn new(thread_pool_size: usize) -> Self {
        Self { thread_pool_size, max_queue_length: 1024 }
    }
}
