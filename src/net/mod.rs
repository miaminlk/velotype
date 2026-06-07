//! HTTP client integration used by remote image loading.

use std::ffi::{c_void, CString};
use std::slice::from_raw_parts;
use std::sync::{Arc, LazyLock};
use std::thread;
use gpui::App;
use gpui::http_client::{self, AsyncBody, HttpClient};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};

macro_rules! net_log {
	($($arg:tt)*) => {
		#[cfg(debug_assertions)]
		{
			let msg = format!($($arg)*);
			println!("{}", msg);
			#[cfg(windows)]
			unsafe
			{
				if let Ok(c_str) = std::ffi::CString::new(msg)
				{
					windows_sys::Win32::System::Diagnostics::Debug::OutputDebugStringA(c_str.as_ptr() as _);
				}
			}
		}
	};
}

static mut CURL_HD: *mut c_void = std::ptr::null_mut();
static mut CURL_EASY_SETOPT_PTR: usize = 0;

type CurlCbFn = Option<unsafe extern "C" fn(buffer: *const u8, size: usize, nitems: usize, userdata: usize) -> usize>;

unsafe extern "C" fn curl_write_callback(buffer: *const u8, size: usize, nitems: usize, userdata: usize) -> usize {
	let total_size = size * nitems;
	if userdata != 0 && total_size > 0
	{
		unsafe
		{
			let tv = &mut *(userdata as *mut Vec<u8>);
			tv.extend_from_slice(from_raw_parts(buffer, total_size));
		}
	}
	total_size
}

static CURL_EASY_INIT: LazyLock<extern "C" fn() -> usize> = LazyLock::new(|| unsafe {
	let path = CString::new(r"D:\float\OneDrive\diatom\conf\dll\curl\libcurl.dll").unwrap();
	CURL_HD = LoadLibraryA(path.as_ptr() as _);

	CURL_EASY_SETOPT_PTR = GetProcAddress(CURL_HD, "curl_easy_setopt\0".as_ptr() as _).expect("curl_easy_setopt not found") as usize;
	let curl_global_init = std::mem::transmute::<_, extern "C" fn(i32) -> i32>(GetProcAddress(CURL_HD, "curl_global_init\0".as_ptr() as _).expect("curl_global_init not found"));
	curl_global_init(3);

	std::mem::transmute::<_, extern "C" fn() -> usize>(GetProcAddress(CURL_HD, "curl_easy_init\0".as_ptr() as _).expect("curl_easy_init not found"))
});

static CURL_EASY_SETOPT: LazyLock<extern "C" fn(usize, i32, usize) -> i32> = LazyLock::new(|| unsafe {
	std::mem::transmute::<_, extern "C" fn(usize, i32, usize) -> i32>(CURL_EASY_SETOPT_PTR)
});

static CURL_EASY_SETOPT_FN: LazyLock<extern "C" fn(usize, i32, CurlCbFn) -> i32> = LazyLock::new(|| unsafe {
	std::mem::transmute::<_, extern "C" fn(usize, i32, CurlCbFn) -> i32>(CURL_EASY_SETOPT_PTR)
});

static CURL_EASY_PERFORM: LazyLock<extern "C" fn(usize) -> i32> = LazyLock::new(|| unsafe {
	std::mem::transmute::<_, extern "C" fn(usize) -> i32>(GetProcAddress(CURL_HD, "curl_easy_perform\0".as_ptr() as _).expect("curl_easy_perform not found"))
});

static CURL_EASY_CLEANUP: LazyLock<extern "C" fn(usize) -> i32> = LazyLock::new(|| unsafe {
	std::mem::transmute::<_, extern "C" fn(usize) -> i32>(GetProcAddress(CURL_HD, "curl_easy_cleanup\0".as_ptr() as _).expect("curl_easy_cleanup not found"))
});

static PROXY_URL: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

#[allow(dead_code)]
pub(crate) fn set_proxy(proxy: String) {
	if let Ok(mut guard) = PROXY_URL.lock()
	{
		*guard = proxy;
	}
}

#[allow(dead_code)]
pub(crate) fn get_proxy() -> String {
	if let Ok(guard) = PROXY_URL.lock()
	{
		guard.clone()
	}
	else
	{
		String::new()
	}
}

pub(crate) fn install_http_client(cx: &mut App) {
	cx.set_http_client(Arc::new(CurlTransportHttpClient {}));
}

pub(crate) fn is_remote_image_source(source: &str) -> bool {
	source.starts_with("http://") || source.starts_with("https://")
}

struct CurlTransportHttpClient {}

impl HttpClient for CurlTransportHttpClient {
	fn type_name(&self) -> &'static str {
		"velotype_curl_transport_http_client"
	}

	fn user_agent(&self) -> Option<&http_client::http::HeaderValue> {
		None
	}

	fn send(
		&self,
		request: http_client::Request<AsyncBody>,
	) -> futures::future::BoxFuture<'static, anyhow::Result<http_client::Response<AsyncBody>>> {
		let (parts, _) = request.into_parts();
		let url = parts.uri.to_string();
		net_log!("CurlHttpClient::send: url = {}", url);
		let (tx, rx) = futures::channel::oneshot::channel();

		thread::spawn(move || {
			let res = unsafe { Self::download(&url) };
			let response = match res
			{
				Some(bytes) =>
				{
					let response_builder = http_client::Response::builder()
						.status(http_client::StatusCode::OK)
						.version(http_client::http::Version::HTTP_11);
					Ok(response_builder.body(AsyncBody::from(bytes)).unwrap())
				}
				None =>
				{
					Err(anyhow::anyhow!("libcurl failed to download image from: {}", url))
				}
			};
			let _ = tx.send(response);
		});

		use futures::FutureExt;
		async move {
			rx.await
				.map_err(|_| anyhow::anyhow!("libcurl HTTP worker dropped before responding"))?
		}
		.boxed()
	}

	fn proxy(&self) -> Option<&http_client::Url> {
		None
	}
}

impl CurlTransportHttpClient {
	unsafe fn download(url: &str) -> Option<Vec<u8>> {
		net_log!("CurlHttpClient::download: start downloading {}", url);
		let hd = (*CURL_EASY_INIT)();
		if hd == 0
		{
			net_log!("CurlHttpClient::download: curl_easy_init failed");
			return None;
		}

		let url_cstr = CString::new(url).ok()?;

		(*CURL_EASY_SETOPT)(hd, 10002, url_cstr.as_ptr() as _);
		(*CURL_EASY_SETOPT)(hd, 155, 10000); // CURLOPT_TIMEOUT_MS = 155, 10 seconds
		(*CURL_EASY_SETOPT)(hd, 156, 10000); // CURLOPT_CONNECTTIMEOUT_MS = 156, 10 seconds
		(*CURL_EASY_SETOPT)(hd, 64, 0);       // CURLOPT_SSL_VERIFYPEER = 64
		(*CURL_EASY_SETOPT)(hd, 81, 0);       // CURLOPT_SSL_VERIFYHOST = 81
		(*CURL_EASY_SETOPT_FN)(hd, 20011, Some(curl_write_callback)); // CURLOPT_WRITEFUNCTION = 20011

		let proxy = get_proxy();
		let _proxy_cstr = if !proxy.is_empty()
		{
			net_log!("CurlHttpClient::download: using proxy = {}", proxy);
			let c = CString::new(proxy).ok()?;
			(*CURL_EASY_SETOPT)(hd, 10004, c.as_ptr() as _);
			Some(c)
		}
		else
		{
			net_log!("CurlHttpClient::download: no proxy set");
			None
		};

		let mut rv: Vec<u8> = Vec::new();
		(*CURL_EASY_SETOPT)(hd, 10001, &mut rv as *mut _ as _); // CURLOPT_WRITEDATA = 10001

		let perform_res = (*CURL_EASY_PERFORM)(hd);
		net_log!("CurlHttpClient::download: curl_easy_perform result code = {}", perform_res);
		(*CURL_EASY_CLEANUP)(hd);

		if perform_res == 0 && !rv.is_empty()
		{
			net_log!("CurlHttpClient::download: download succeeded, bytes count = {}", rv.len());
			Some(rv)
		}
		else
		{
			net_log!("CurlHttpClient::download: download failed or empty data, perform_res = {}, empty = {}", perform_res, rv.is_empty());
			None
		}
	}
}
