# HTTP Client Redirection Details (libcurl.dll Integration)

This document provides a detailed mapping of all HTTP network operations within Velotype. Its goal is to guide the refactoring of network dependencies to substitute `reqwest` and `gpui_http_client` with `libcurl.dll` or host-provided FFI callbacks.

---

## 1. Overview of Network Operations

There is exactly **one** entry point in the Velotype codebase that performs HTTP requests:

1. **Remote Image Loading Client** ([`src/net/mod.rs`](file:///d:/float/OneDrive/ONE/velotype/src/net/mod.rs))
   - Wrapped inside the GPUI asset system (`ImageAssetLoader` and `ImageCache`).
   - Dynamically triggered whenever a Markdown document contains a remote image reference.

---

## 2. Remote Image Loading Client

### Call Context & Lifecycle
- GPUI sets a global HTTP client using `cx.set_http_client(Arc::new(client))`.
- When an image element with a `http(s)://` source is rendered:
  1. `ImageAssetLoader::load` gets the client via `cx.http_client()`.
  2. It calls `client.get(uri, ...)` which is translated into a `gpui_http_client::Request<AsyncBody>` internally.
  3. The request is dispatched to `HttpClient::send(&self, request)`.
  4. The implementation must return a `BoxFuture<'static, Result<Response<AsyncBody>, Error>>`.

### Request Data Structure (Input)
When intercepting the request in `HttpClient::send`, the input `Request<AsyncBody>` contains:

| Field | Rust Type | Description / Sample Value |
| :--- | :--- | :--- |
| **Method** | `http::Method` | Almost always `GET` (for images). |
| **URI** | `http::Uri` | The full image URL (e.g. `https://pbs.twimg.com/media/HGguQvubo...`). |
| **Headers** | `http::HeaderMap` | Key-Value headers list containing: <br>- `User-Agent`<br>- `Accept` (typically `image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8`)<br>- `Cache-Control` (typically `no-cache`)<br>- `Pragma` (typically `no-cache`) |
| **Body** | `AsyncBody` | The request body stream. For image `GET` requests, this is always empty. |

### Response Data Structure (Expected Output)
The return value must be mapped back into `gpui_http_client::Response<AsyncBody>`, containing:

| Field | Rust Type | Description |
| :--- | :--- | :--- |
| **Status Code** | `http::StatusCode` | The HTTP response status (e.g., `200 OK`, `404 Not Found`). |
| **HTTP Version** | `http::Version` | The protocol version (e.g., `HTTP/1.1`, `HTTP/2`). |
| **Headers** | `http::HeaderMap` | Response headers returned by the server (e.g. `Content-Type`, `Content-Length`). |
| **Body** | `AsyncBody` | The downloaded binary image payload (buffered into a `Vec<u8>` first). |

---

## 3. Integration Blueprint for `libcurl.dll` or Host FFI Callback

To decouple Velotype from heavy Rust asynchronous network crates (`reqwest`, `hyper`, `tokio`), we can transition to a lightweight FFI bridge.

### Option A: Dynamic `libcurl.dll` Loading inside Velotype
Velotype loads the host's `libcurl.dll` at runtime and wraps it:
```rust
// Pseudocode for libcurl dynamic binding wrapper
struct LibCurl {
    lib: libloading::Library,
    easy_init: unsafe extern "C" fn() -> *mut c_void,
    easy_setopt: unsafe extern "C" fn(*mut c_void, u32, ...) -> i32,
    easy_perform: unsafe extern "C" fn(*mut c_void) -> i32,
    easy_cleanup: unsafe extern "C" fn(*mut c_void),
}
```
* **Pros**: Transparent to the host process; self-contained inside `Velotype.dll`.
* **Cons**: Still requires shipping or locating a valid `libcurl.dll` on the host machine.

### Option B: Host-Provided FFI Callback (Recommended for DLL integration)
Since Velotype is compiled as a DLL, the host process (e.g., `ahk.exe`) can register a request handler callback during initialization:

```rust
// The C-compatible function pointer signature for HTTP dispatching
pub type HostHttpCallback = unsafe extern "C" fn(
    url: *const c_char,
    method: *const c_char,
    headers_json: *const c_char,
    response_status: *mut i32,
    response_body_ptr: *mut *mut u8,
    response_body_len: *mut usize,
) -> i32;

static HOST_HTTP_CALLBACK: OnceLock<HostHttpCallback> = OnceLock::new();

#[no_mangle]
pub unsafe extern "C" fn register_http_handler(callback: HostHttpCallback) {
    let _ = HOST_HTTP_CALLBACK.set(callback);
}
```
* **Pros**:
  - **Zero network library dependencies inside `Velotype.dll`**: Completely removes `reqwest`, `native-tls`, `openssl`, etc.
  - Reduces DLL size by several megabytes.
  - Automatically respects the host's networking sandbox, proxy settings (like Proxifier), and certificate stores without any extra logic.
* **Cons**: Requires the host script/application to implement the HTTP fetch callback.
