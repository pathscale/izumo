use std::sync::{Arc, OnceLock, RwLock};

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct PageSurface {
    pub tab_id: u64,
    pub generation: u64,
    pub owner_document_id: u64,
    pub mount_node_id: u64,
    pub document_id: u64,
}

#[derive(Clone, Copy)]
pub enum SurfaceEvent {
    Visibility { page: PageSurface, visible: bool },
    Painted { page: PageSurface },
}

type Provider = dyn Fn() -> Vec<PageSurface> + Send + Sync;
type Observer = dyn Fn(Arc<SurfaceEvent>) + Send + Sync;
static PROVIDER: OnceLock<RwLock<Option<Arc<Provider>>>> = OnceLock::new();
static OBSERVER: OnceLock<RwLock<Option<Arc<Observer>>>> = OnceLock::new();

pub fn set_page_surface_hooks(
    provider: impl Fn() -> Vec<PageSurface> + Send + Sync + 'static,
    observer: impl Fn(Arc<SurfaceEvent>) + Send + Sync + 'static,
) {
    *PROVIDER.get_or_init(|| RwLock::new(None)).write().unwrap() = Some(Arc::new(provider));
    *OBSERVER.get_or_init(|| RwLock::new(None)).write().unwrap() = Some(Arc::new(observer));
}

pub(crate) fn pages() -> Vec<PageSurface> {
    let provider = PROVIDER
        .get_or_init(|| RwLock::new(None))
        .read()
        .unwrap()
        .clone();
    provider.map_or_else(Vec::new, |provider| provider())
}

pub(crate) fn publish(event: SurfaceEvent) {
    let observer = OBSERVER
        .get_or_init(|| RwLock::new(None))
        .read()
        .unwrap()
        .clone();
    if let Some(observer) = observer {
        observer(Arc::new(event));
    }
}
