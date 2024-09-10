use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use pin_project_lite::pin_project;
use tower::{util::Oneshot, ServiceExt};

/// A Tower service converted into a Hyper service.
#[derive(Debug, Copy, Clone)]
pub struct TowerToHyperService<S> {
    service: S,
}

impl<S> TowerToHyperService<S> {
    /// Create a new `TowerToHyperService` from a Tower service.
    pub fn new(tower_service: S) -> Self {
        Self { service: tower_service }
    }
}

impl<S, R> hyper::service::Service<R> for TowerToHyperService<S>
where
    S: tower::Service<R> + Clone,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = TowerToHyperServiceFuture<S, R>;

    fn call(&self, req: R) -> Self::Future {
        TowerToHyperServiceFuture {
            future: self.service.clone().oneshot(req),
        }
    }
}

pin_project! {
    /// Response future for [`TowerToHyperService`].
    pub struct TowerToHyperServiceFuture<S, R>
    where
        S: tower::Service<R>,
    {
        #[pin]
        future: Oneshot<S, R>,
    }
}

impl<S, R> Future for TowerToHyperServiceFuture<S, R>
where
    S: tower::Service<R>,
{
    type Output = Result<S::Response, S::Error>;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().future.poll(cx)
    }
}
