# Velotype 图片上下巨大空白间隙排查与修复指南

本指南记录了 Velotype 在所见即所得 (WYSIWYG) 预览模式下渲染 Markdown 和 HTML 图片时，图片上下出现巨大空白间隙的根本原因，以及在应用层（不改动 GPUI 框架本身）的优雅解决方案。

---

## 1. 问题根源分析 (Root Cause)

当我们在应用层使用 `img(source)` 创建 GPUI 的图片元素时，如果不指定其绝对尺寸（即使用默认的 `Auto` 宽高），GPUI 的底层布局代码（`crates/gpui/src/elements/img.rs`）在布局阶段会将 `Auto` 宽度和高度强行转换为图片的**原始像素大小**（例如一张分辨率为 `1450x357` 的 Banner 图，会被解析为 `1450px` 宽，`357px` 高）。

在原始代码中，图片渲染方法 `render_image_content` 包含以下链式调用：
```rust
let image = match source {
    ImageResolvedSource::Local(path) => img(path),
    ImageResolvedSource::Remote(uri) => img(uri),
}
.max_w(max_width) // 限制最大宽度（例如 800px）
.max_h(max_height) // 限制最大高度（例如 420px）
.object_fit(ObjectFit::Contain)
```

当这组样式传递给 Taffy 布局引擎时，会发生以下情况：
1. **宽度计算：** 首选宽度为 `1450px`，但被 `.max_w(800.0)` 限制，因此布局盒子的最终宽度被钳制为 `800px`。
2. **高度计算：** 首选高度为原始的 `357px`，且并未超出 `.max_h(420.0)` 的限制。**关键问题在于，因为高度被 GPUI 从 Auto 转换为了明确的绝对值 (357px)，Taffy 布局引擎在计算高度时不再受 aspect-ratio 比例约束，导致最终布局盒子的高度依然停留在 357px，无法随着宽度按比例缩小。**
3. **渲染绘制：** 布局盒子大小最终确定为 `800px * 357px`。在绘制阶段，`ObjectFit::Contain` 会将图片按真实比例（`1450:357`，约 `4.06:1`）缩放绘制。此时，缩放后的真实图片高度仅为 `800 / 4.06 = 197px`。
4. **间隙产生：** 图片在 `357px` 高度的布局盒子内居中对齐，导致图片上下两端各留下了约 `(357 - 197) / 2 = 80px` 的巨大空白，形成了视觉上的“大空隙”。

---

## 2. 解决方案 (Solution)

为了让 Taffy 布局引擎自动、等比例地计算出缩小后的高度，GPUI 的 `img` 元素必须获得一个**确定的绝对宽度** `.w(resolved_width)`。如果宽度为确定的绝对值，GPUI 的底层就会自动应用图片的 Aspect Ratio（纵横比）计算出正确的高度，从而使布局盒子的高宽比完全契合图片的物理高宽比。

由于图片在列容器 (Flex Column) 中布局，我们可以在应用层动态获取当前视口宽度，并根据组件的上下文（普通段落、列表项、表格单元格等）计算出图片实际能获得的最大像素宽度，然后显式调用 `.w(resolved_width)` 限制图片宽度，从而彻底消除高度冗余导致的空白。

### 修改方案设计：
1. 传入 `window: &Window` 到 `render_image_content` 中，获取视口真实像素宽度。
2. 计算当前的宽度预算 `resolved_width`。
3. 将图片控件的约束由 `.max_w(max_width)` 更改为 `.w(resolved_width)`。
4. 在渲染 HTML 文档时，由于 HTML block 也是整体渲染的，需要将 `window` 传给整个 HTML 渲染链路（`render_html_document` -> `render_html_node` -> `render_html_text_node` 等），使其能够流转并提供给 HTML 图像渲染器。

---

## 3. 具体修改点说明

所有的修改均在 [`src/components/block/render.rs`](file:///d:/float/OneDrive/ONE/velotype/src/components/block/render.rs) 中实现。

### 3.1 核心渲染函数重构

重构 `render_image_content` 方法，新增 `window` 参数，并在内部计算布局宽度：

```rust
	fn render_image_content(
		&self,
		runtime: &ImageRuntime,
		max_width: Length,
		max_height: Pixels,
		placeholder_height: Pixels,
		theme: &Theme,
		strings: &I18nStrings,
		window: &Window,
	) -> AnyElement {
		// ...

		// 动态计算图片的绝对布局宽度预算
		let resolved_width = match max_width
		{
			Length::Definite(DefiniteLength::Absolute(px)) => px,
			Length::Definite(DefiniteLength::Fraction(f)) =>
			{
				let viewport_width = f32::from(window.viewport_size().width.max(px(1.0)));
				let budget = match self.kind()
				{
					BlockKind::BulletedListItem | BlockKind::TaskListItem { .. } | BlockKind::NumberedListItem =>
					{
						effective_list_item_image_width(self, viewport_width, d)
					}
					_ =>
					{
						if self.is_table_cell()
						{
							effective_table_width(self, viewport_width, d) / 2.0
						}
						else
						{
							effective_image_width(self, viewport_width, d)
						}
					}
				};
				AbsoluteLength::Pixels(px(budget * f))
			}
			_ =>
			{
				let viewport_width = f32::from(window.viewport_size().width.max(px(1.0)));
				let budget = match self.kind()
				{
					BlockKind::BulletedListItem | BlockKind::TaskListItem { .. } | BlockKind::NumberedListItem =>
					{
						effective_list_item_image_width(self, viewport_width, d)
					}
					_ =>
					{
						if self.is_table_cell()
						{
							effective_table_width(self, viewport_width, d) / 2.0
						}
						else
						{
							effective_image_width(self, viewport_width, d)
						}
					}
				};
				AbsoluteLength::Pixels(px(budget))
			}
		};

		let image = match source
		{
			ImageResolvedSource::Local(path) => img(path),
			ImageResolvedSource::Remote(uri) => img(uri),
		}
		.w(resolved_width) // 限制绝对宽度，激活底层纵横比计算
		.max_h(max_height)
		.object_fit(ObjectFit::Contain)
		// ...
```

### 3.2 HTML 渲染链路流转

为了在解析内联 HTML 图片时也能够正确算得宽度预算，以下函数增加了 `window: &Window` 参数并在递归调用中层层向下传递：

* `render_html_document`
* `render_html_node`
* `render_html_text_node`
* `render_html_inline_container`
* `render_html_image`
* `render_html_table`
* `render_html_table_row`
* `render_html_details`

例如，在 `render_html_image` 内，调用 `render_image_content` 并流转 `window`：
```rust
		let content = self.render_image_content(
			&runtime,
			Length::Definite(relative(zoom)),
			px(theme.dimensions.image_root_max_height * zoom),
			px(theme.dimensions.image_root_placeholder_height * zoom),
			theme,
			&strings,
			window,
		);
```

### 3.3 所有图像渲染调用点适配

对 `render_image_content` 的所有调用点同步做出了修正，包含：
- 表格单元格预览模式图像渲染
- 普通段落预览模式图像渲染
- 无序、有序以及任务列表项预览模式图像渲染
- Markdown 嵌入式独立图像解析模块
- HTML 预览模块下的图片元素渲染

通过上述应用层修正，Velotype 的图片在自适应缩放时已无任何垂直间隙，布局完美契合其自身高宽比。