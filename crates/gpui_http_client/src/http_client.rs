pub use anyhow::{Result, anyhow};
use crate::async_body::AsyncBody;
use crate::async_body::reqwest;
use derive_more::Deref;
use http::HeaderValue;
pub use http::{self, Method, Request, Response, StatusCode, Uri, request::Builder};

use futures::{
	FutureExt as _,
	future::{self, BoxFuture},
};
use parking_lot::Mutex;
use std::{any::type_name, sync::Arc};
pub use url::Url;

#[derive(Default, Debug, Clone, PartialEq, Eq, Hash)]
pub enum RedirectPolicy {
	#[default]
	NoFollow,
	FollowLimit(u32),
	FollowAll,
}

pub struct FollowRedirects(pub bool);

pub trait HttpRequestExt {
	fn follow_redirects(self, follow: RedirectPolicy) -> Self;
}

impl HttpRequestExt for http::request::Builder {
	fn follow_redirects(self, follow: RedirectPolicy) -> Self {
		self.extension(follow)
	}
}

pub trait HttpClient: 'static + Send + Sync {
	fn type_name(&self) -> &'static str;

	fn user_agent(&self) -> Option<&HeaderValue>;

	fn send(
		&self,
		req: http::Request<AsyncBody>,
	) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>>;

	fn get(
		&self,
		uri: &str,
		body: AsyncBody,
		follow_redirects: bool,
	) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
		let request = Builder::new()
			.uri(uri)
			.follow_redirects(if follow_redirects
			{
				RedirectPolicy::FollowAll
			}
			else
			{
				RedirectPolicy::NoFollow
			})
			.body(body);

		match request
		{
			Ok(request) => self.send(request),
			Err(e) => Box::pin(async move { Err(e.into()) }),
		}
	}

	fn post_json(
		&self,
		uri: &str,
		body: AsyncBody,
	) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
		let request = Builder::new()
			.uri(uri)
			.method(Method::POST)
			.header("Content-Type", "application/json")
			.body(body);

		match request
		{
			Ok(request) => self.send(request),
			Err(e) => Box::pin(async move { Err(e.into()) }),
		}
	}

	fn proxy(&self) -> Option<&Url>;

	fn as_fake(&self) -> &FakeHttpClient {
		panic!("as_fake stub called");
	}

	fn send_multipart_form<'a>(
		&'a self,
		_url: &str,
		_request: reqwest::multipart::Form,
	) -> BoxFuture<'a, anyhow::Result<Response<AsyncBody>>> {
		future::ready(Err(anyhow!("not implemented in stub"))).boxed()
	}
}

#[derive(Deref)]
pub struct HttpClientWithProxy {
	#[deref]
	client: Arc<dyn HttpClient>,
	proxy: Option<Url>,
}

impl HttpClientWithProxy {
	pub fn new(client: Arc<dyn HttpClient>, proxy_url: Option<String>) -> Self {
		let proxy_url = proxy_url.and_then(|proxy| proxy.parse().ok());
		Self::new_url(client, proxy_url)
	}
	pub fn new_url(client: Arc<dyn HttpClient>, proxy_url: Option<Url>) -> Self {
		Self {
			client,
			proxy: proxy_url,
		}
	}
}

impl HttpClient for HttpClientWithProxy {
	fn send(
		&self,
		req: Request<AsyncBody>,
	) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
		self.client.send(req)
	}

	fn user_agent(&self) -> Option<&HeaderValue> {
		self.client.user_agent()
	}

	fn proxy(&self) -> Option<&Url> {
		self.proxy.as_ref()
	}

	fn type_name(&self) -> &'static str {
		self.client.type_name()
	}

	fn as_fake(&self) -> &FakeHttpClient {
		self.client.as_fake()
	}

	fn send_multipart_form<'a>(
		&'a self,
		url: &str,
		form: reqwest::multipart::Form,
	) -> BoxFuture<'a, anyhow::Result<Response<AsyncBody>>> {
		self.client.send_multipart_form(url, form)
	}
}

pub struct HttpClientWithUrl {
	base_url: Mutex<String>,
	client: HttpClientWithProxy,
}

impl std::ops::Deref for HttpClientWithUrl {
	type Target = HttpClientWithProxy;

	fn deref(&self) -> &Self::Target {
		&self.client
	}
}

impl HttpClientWithUrl {
	pub fn new(
		client: Arc<dyn HttpClient>,
		base_url: impl Into<String>,
		proxy_url: Option<String>,
	) -> Self {
		let client = HttpClientWithProxy::new(client, proxy_url);
		Self {
			base_url: Mutex::new(base_url.into()),
			client,
		}
	}

	pub fn new_url(
		client: Arc<dyn HttpClient>,
		base_url: impl Into<String>,
		proxy_url: Option<Url>,
	) -> Self {
		let client = HttpClientWithProxy::new_url(client, proxy_url);
		Self {
			base_url: Mutex::new(base_url.into()),
			client,
		}
	}

	pub fn base_url(&self) -> String {
		self.base_url.lock().clone()
	}

	pub fn set_base_url(&self, base_url: impl Into<String>) {
		*self.base_url.lock() = base_url.into();
	}

	pub fn build_url(&self, path: &str) -> String {
		format!("{}{}", self.base_url(), path)
	}
}

impl HttpClient for HttpClientWithUrl {
	fn send(
		&self,
		req: Request<AsyncBody>,
	) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
		self.client.send(req)
	}

	fn user_agent(&self) -> Option<&HeaderValue> {
		self.client.user_agent()
	}

	fn proxy(&self) -> Option<&Url> {
		self.client.proxy.as_ref()
	}

	fn type_name(&self) -> &'static str {
		self.client.type_name()
	}

	fn as_fake(&self) -> &FakeHttpClient {
		self.client.as_fake()
	}

	fn send_multipart_form<'a>(
		&'a self,
		url: &str,
		request: reqwest::multipart::Form,
	) -> BoxFuture<'a, anyhow::Result<Response<AsyncBody>>> {
		self.client.send_multipart_form(url, request)
	}
}

pub struct BlockedHttpClient;

impl BlockedHttpClient {
	pub fn new() -> Self {
		BlockedHttpClient
	}
}

impl HttpClient for BlockedHttpClient {
	fn send(
		&self,
		_req: Request<AsyncBody>,
	) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
		Box::pin(async {
			Err(std::io::Error::new(
				std::io::ErrorKind::PermissionDenied,
				"BlockedHttpClient disallowed request",
			)
			.into())
		})
	}

	fn user_agent(&self) -> Option<&HeaderValue> {
		None
	}

	fn proxy(&self) -> Option<&Url> {
		None
	}

	fn type_name(&self) -> &'static str {
		type_name::<Self>()
	}
}

pub struct FakeHttpClient;
impl FakeHttpClient {
	pub fn with_404_response() -> Arc<HttpClientWithUrl> {
		panic!("FakeHttpClient stub called");
	}
	pub fn with_200_response() -> Arc<HttpClientWithUrl> {
		panic!("FakeHttpClient stub called");
	}
}
