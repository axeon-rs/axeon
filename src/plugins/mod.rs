use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Default, Debug, Clone)]
pub struct Plugins {
    data: Arc<HashMap<TypeId, Box<dyn Any + Send + Sync>>>,
}

impl Plugins {
    pub fn new() -> Self {
        Self {
            data: Arc::new(HashMap::new()),
        }
    }

    pub(crate) fn insert<T: 'static + Send + Sync>(&mut self, value: T) {
        Arc::get_mut(&mut self.data)
            .expect("Cannot modify state after application start")
            .insert(TypeId::of::<T>(), Box::new(value));
    }

    pub fn get<T: 'static>(&self) -> Option<&T> {
        self.data
            .get(&TypeId::of::<T>())
            .and_then(|boxed| boxed.downcast_ref())
    }
}