# 完整实施计划报告：使用 imgpix.dll 替换 image 库 (JXL 剪贴板版本)

本报告针对 Velotype 项目中图片解码与处理逻辑进行重构设计。目标是**完全移除标准的 `image` 依赖库**，改用自用的 `imgpix.dll` 动态链接库进行高效的 `BGRA` 图片解码，并利用 `imgpix.dll` 导出的 **JXL (JPEG XL) 无损编码** API 实现剪贴板的图片数据转换与粘贴，从而实现最大程度的 Windows 本地化与轻量化。

---

## 1. 现状分析与重构范围

目前 `image` 库在 `crates/gpui` 中主要被用于以下三个地方：

1. **[`crates/gpui/src/assets.rs`](file:///d:/float/OneDrive/ONE/velotype/crates/gpui/src/assets.rs)**：
   * `RenderImage` 内部的 `data` 成员使用 `SmallVec<[Frame; 1]>`，其中 `Frame` 和 `Delay` 来自 `image` 库。
2. **[`crates/gpui/src/elements/img.rs`](file:///d:/float/OneDrive/ONE/velotype/crates/gpui/src/elements/img.rs)**：
   * 在 `ImageCache::load` 中，使用 `image::guess_format` 探测格式，使用 `GifDecoder`、`WebPDecoder` 或 `load_from_memory_with_format` 解码普通位图。
   * 解码出 `RGBA8` 后，在 CPU 侧遍历并手动执行 `pixel.swap(0, 2)` 转换为 `BGRA8`。
3. **[`crates/gpui/src/platform/windows/clipboard.rs`](file:///d:/float/OneDrive/ONE/velotype/crates/gpui/src/platform/windows/clipboard.rs)**：
   * 在 `convert_image_to_png_format` 中使用 `image::load_from_memory_with_format` 解码剪贴板数据，并使用 `image::write_to` 重新编码为 PNG 字节流。

---

## 2. 详细实施步骤

### 步骤一：在 Windows 平台中引入 `imgpix.dll` 动态加载模块
新建文件 [`crates/gpui/src/platform/windows/imgpix.rs`](file:///d:/float/OneDrive/ONE/velotype/crates/gpui/src/platform/windows/imgpix.rs)，利用 Windows 的 `LoadLibraryW` 和 `GetProcAddress` 动态加载 `imgpix.dll`。除了原有的解码 API，新增 JXL 无损编码 API。

```rust
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::OnceLock;
use windows::core::PCWSTR;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

type ImgpixDecBgraFn = unsafe extern "C" fn(
	data: *const u8,
	data_len: usize,
	out_pixels: *mut *mut c_void,
	out_w: *mut i32,
	out_h: *mut i32,
	out_type: *mut i32,
	out_err: *mut *mut i8,
	threads: i32,
) -> i32;

// 假设 imgpix 导出的 JXL 无损编码 API 签名
type ImgpixEncJxlFn = unsafe extern "C" fn(
	bgra: *const u8,
	width: i32,
	height: i32,
	out_data: *mut *mut c_void,
	out_data_len: *mut usize,
) -> i32;

type ImgpixFreeFn = unsafe extern "C" fn(p: *mut c_void);

struct ImgpixLib {
	dec_bgra: ImgpixDecBgraFn,
	enc_jxl: ImgpixEncJxlFn,
	free: ImgpixFreeFn,
}

static LIB: OnceLock<Option<ImgpixLib>> = OnceLock::new();

fn get_lib() -> Option<&'static ImgpixLib> {
	LIB.get_or_init(|| {
		unsafe {
			let paths = [
				PathBuf::from("imgpix.dll"),
				PathBuf::from("D:\\GIT\\imgpix\\imgpix.dll"),
			];
			for path in &paths {
				if path.exists() {
					let wide_path: Vec<u16> = path
						.to_string_lossy()
						.encode_utf16()
						.chain(std::iter::once(0))
						.collect();
					let handle = LoadLibraryW(PCWSTR(wide_path.as_ptr()));
					if let Ok(lib) = handle {
						let dec_bgra = std::mem::transmute(GetProcAddress(lib, windows::core::s!("imgpix_dec_bgra")));
						let enc_jxl = std::mem::transmute(GetProcAddress(lib, windows::core::s!("imgpix_enc_jxl")));
						let free = std::mem::transmute(GetProcAddress(lib, windows::core::s!("imgpix_free")));
						return Some(ImgpixLib { dec_bgra, enc_jxl, free });
					}
				}
			}
			None
		}
	}).as_ref()
}

pub fn decode_to_bgra(data: &[u8]) -> Result<(Vec<u8>, u32, u32), String> {
	let lib = get_lib().ok_or_else(|| {
		"imgpix.dll not found".to_string()
	})?;
	let mut out_pixels: *mut c_void = std::ptr::null_mut();
	let mut w: i32 = 0;
	let mut h: i32 = 0;
	let mut img_type: i32 = 0;
	let mut out_err: *mut i8 = std::ptr::null_mut();

	unsafe {
		let ret = (lib.dec_bgra)(
			data.as_ptr(),
			data.len(),
			&mut out_pixels,
			&mut w,
			&mut h,
			&mut img_type,
			&mut out_err,
			0,
		);
		if ret != 0 {
			let err_str = if !out_err.is_null() {
				let s = std::ffi::CStr::from_ptr(out_err).to_string_lossy().into_owned();
				(lib.free)(out_err as *mut c_void);
				s
			} else {
				format!("decode failed with code {}", ret)
			};
			return Err(err_str);
		}
		let len = (w * h * 4) as usize;
		let pixels = std::slice::from_raw_parts(out_pixels as *const u8, len).to_vec();
		(lib.free)(out_pixels);
		Ok((pixels, w as u32, h as u32))
	}
}

pub fn encode_to_jxl(bgra: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
	let lib = get_lib().ok_or_else(|| {
		"imgpix.dll not found".to_string()
	})?;
	let mut out_data: *mut c_void = std::ptr::null_mut();
	let mut out_data_len: usize = 0;

	unsafe {
		let ret = (lib.enc_jxl)(
			bgra.as_ptr(),
			width as i32,
			height as i32,
			&mut out_data,
			&mut out_data_len,
		);
		if ret != 0 {
			return Err(format!("jxl encode failed with code {}", ret));
		}
		let bytes = std::slice::from_raw_parts(out_data as *const u8, out_data_len).to_vec();
		(lib.free)(out_data);
		Ok(bytes)
	}
}
```

---

### 步骤二：重构 `RenderImage` 与 `Frame` 数据结构
为了完全剥离 `image` 依赖，在 [`crates/gpui/src/assets.rs`](file:///d:/float/OneDrive/ONE/velotype/crates/gpui/src/assets.rs) 中自定义 `Frame` 和 `Delay` 结构体：

```rust
pub struct Delay {
	numerator: u32,
	denominator: u32,
}

impl Delay {
	pub fn from_numer_denom(numerator: u32, denominator: u32) -> Self {
		Self { numerator, denominator }
	}
}

pub struct Frame {
	buffer: Vec<u8>,
	width: u32,
	height: u32,
	delay: Delay,
}

impl Frame {
	pub fn new(buffer: Vec<u8>, width: u32, height: u32) -> Self {
		Self {
			buffer,
			width,
			height,
			delay: Delay::from_numer_denom(0, 1),
		}
	}

	pub fn buffer(&self) -> &[u8] {
		&self.buffer
	}

	pub fn width(&self) -> u32 {
		self.width
	}

	pub fn height(&self) -> u32 {
		self.height
	}

	pub fn delay(&self) -> Delay {
		self.delay
	}
}
```

修改 `RenderImage` 相关方法，使其完全摆脱对 `image` 依赖库的引用。

---

### 步骤三：重构 `crates/gpui/src/elements/img.rs` 的解码逻辑
修改 `ImageCache::load` 方法，直接利用 `imgpix::decode_to_bgra` 处理图片字节流，免去之前 RGBA 到 BGRA 频繁的 CPU 换道开销。

---

### 步骤四：重构 `clipboard.rs`，使用无损 JXL 数据格式读写剪贴板
修改 [`crates/gpui/src/platform/windows/clipboard.rs`](file:///d:/float/OneDrive/ONE/velotype/crates/gpui/src/platform/windows/clipboard.rs)：

1. 注册用于存放 JXL 的剪贴板格式：
```rust
static CLIPBOARD_JXL_FORMAT: LazyLock<u32> =
	LazyLock::new(|| register_clipboard_format(windows::core::w!("image/jxl")));
```

2. 移除原有的 PNG 格式转换，取而代之的是 `convert_image_to_jxl_format` 方法。先用 `imgpix` 解码源数据为 BGRA 像素，再用新导出的 `encode_to_jxl` 对其做无损 JXL 压缩，并写入剪贴板：
```rust
fn convert_image_to_jxl_format(bytes: &[u8]) -> Result<Vec<u8>> {
	let (pixels, width, height) = crate::platform::windows::imgpix::decode_to_bgra(bytes)
		.map_err(|e| anyhow::anyhow!("imgpix decode error for clipboard: {}", e))?;
	let jxl_bytes = crate::platform::windows::imgpix::encode_to_jxl(&pixels, width, height)
		.map_err(|e| anyhow::anyhow!("imgpix jxl encode error for clipboard: {}", e))?;
	Ok(jxl_bytes)
}
```

---

### 步骤五：清理 `Cargo.toml` 依赖项
* 删除 `crates/gpui/Cargo.toml` 和根 `Cargo.toml` 中与 `image` 相关的全部配置。

---

## 3. 验证与排错方案
使用 `cargo check -p gpui` 进行编译校验，并通过 Velotype 的复制粘贴操作，验证 `image/jxl` 格式的无损复制与解码功能正常工作。
