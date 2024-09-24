use quick_cache::Lifecycle;

pub struct ExpiryAwareLifecycle<K, V> {
    _kv: std::marker::PhantomData<(K, V)>,
}

impl<K, V> Lifecycle<K, V> for ExpiryAwareLifecycle<K, V> {
    type RequestState = Option<(K, V)>;

    #[inline]
    fn begin_request(&self) -> Self::RequestState {
        None
    }

    #[inline]
    fn on_evict(&self, state: &mut Self::RequestState, key: K, val: V) {
        *state = Some((key, val));
    }
}
