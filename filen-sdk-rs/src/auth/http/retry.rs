use std::sync::Arc;

use futures::future::{self, Map};
use tower::{
	Layer, Service,
	retry::{Policy, Retry, RetryLayer},
};

use super::tower_wasm_time::tps_budget::TpsBudget;

#[derive(Clone, Debug)]
pub(crate) struct RetryPolicy {
	budget: Arc<TpsBudget>,
}

impl RetryPolicy {
	pub(crate) fn new(budget: TpsBudget) -> Self {
		Self {
			budget: Arc::new(budget),
		}
	}
}

#[derive(Clone, Debug)]
pub(crate) enum RetryError<E> {
	Retry(E),
	NoRetry(E),
}

impl<E> RetryError<E> {
	pub(crate) fn into_inner(self) -> E {
		match self {
			RetryError::Retry(e) => e,
			RetryError::NoRetry(e) => e,
		}
	}
}

#[derive(Clone)]
pub(crate) struct RetryMapLayer {
	inner: RetryLayer<RetryPolicy>,
}

impl RetryMapLayer {
	pub(crate) fn new(policy: RetryPolicy) -> Self {
		Self {
			inner: RetryLayer::new(policy),
		}
	}
}

impl<S> Layer<S> for RetryMapLayer {
	type Service = RetryMapService<S>;

	fn layer(&self, service: S) -> Self::Service {
		RetryMapService {
			inner: self.inner.layer(service),
		}
	}
}

#[derive(Clone)]
pub(crate) struct RetryMapService<S> {
	inner: Retry<RetryPolicy, S>,
}

impl<S, Req, Res, E> Service<Req> for RetryMapService<S>
where
	S: Service<Req, Response = Res, Error = RetryError<E>> + Clone,
	Req: Clone,
	E: std::fmt::Debug,
{
	type Response = Res;
	type Error = E;
	type Future = Map<
		<Retry<RetryPolicy, S> as Service<Req>>::Future,
		fn(Result<Res, RetryError<E>>) -> Result<Res, E>,
	>;

	fn poll_ready(
		&mut self,
		cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<Result<(), Self::Error>> {
		self.inner.poll_ready(cx).map_err(|e| e.into_inner())
	}

	fn call(&mut self, req: Req) -> Self::Future {
		futures::FutureExt::map(self.inner.call(req), |res| res.map_err(|e| e.into_inner()))
	}
}

impl<Req, Res, E> Policy<Req, Res, RetryError<E>> for RetryPolicy
where
	E: std::fmt::Debug,
	Req: Clone,
{
	type Future = future::Ready<()>;

	fn retry(
		&mut self,
		_req: &mut Req,
		result: &mut Result<Res, RetryError<E>>,
	) -> Option<Self::Future> {
		match result {
			Ok(_) => {
				self.budget.deposit();
				None
			}
			Err(RetryError::Retry(_)) => {
				if self.budget.withdraw() {
					Some(future::ready(()))
				} else {
					None
				}
			}
			Err(RetryError::NoRetry(_)) => None,
		}
	}

	fn clone_request(&mut self, req: &Req) -> Option<Req> {
		Some(req.clone())
	}
}
