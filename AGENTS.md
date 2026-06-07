项目的初始任务和定位：
项目是Velotype[多平台 Markdown 编辑器]的自用分支，目标是修改为编译 Velotype.dll，以类似Scintilla.dll的方法作为显示Markdown内容的控件实现器。

计划流程：
1，完整解析项目结构，技术栈，工作逻辑
2，进行Windows化，最大程度去除跨平台逻辑，用WINAPI 代替或者直接实现相关方法。比如用Windows类、窗口消息，代替窗口机制(但我不了解gpui 是否使用了类似winit的方法)。我重构这类跨平台项目的经验是首先直接用窗口消息代替过度包装的winit。
3，然后参考Scintilla.dll(源代码在scintilla/)，开始修改。Windows_rust\fog.rs 是一个音乐播放器代码，大量使用WINAPI的方法，也包含窗口消息的大量使用。是可用可行的参考。


作为步骤0，是安装编译环境。如果有无法安装的，可以要求我手动安装

之上任务已经完成。项目内其他.md记录现状。



**Rules for writing new code (do not change existing code styles):**
1. **Naming:** Use Snake Case.
2. **Indent:** Use tabs (`\t`).
3. **Braces `{`:**
   - Definitions (func/class): Keep on the **same line**.
   - Control flow (if/loop): Move to a **new line**.