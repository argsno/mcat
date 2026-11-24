# mcat

纯 Rust 的 `cat`：Markdown / reStructuredText 渲染、语法高亮、终端看图。
用户文档见 [README.md](README.md)，开发约定和渲染取舍见 [CONTRIBUTING.md](CONTRIBUTING.md)。

- 改渲染逻辑前先读 CONTRIBUTING 的「渲染管线上的取舍」「reStructuredText 渲染的取舍」。
- 保持纯 Rust、无 C 依赖；新增依赖前先掂量冷构建时间。
- 提交前跑 `cargo test`、`cargo clippy --all-targets`、`cargo fmt`。
- 代码注释、文档和提交信息用中文，与现有仓库一致。
