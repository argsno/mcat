# mcat

一个会渲染 Markdown 的 `cat`。

- `.md` 文件渲染成终端样式，**始终带颜色**
- 代码块交给 syntect 做语法高亮
- [链接](https://github.com/argsno/mcat) 和 `行内代码` 随处可用

## 三种用法

- [x] Markdown 渲染
- [x] 语法高亮
- [ ] 等你来发现

> 引用只加一条竖线，不重新排版。

| 特性 | 说明 |
|---|---|
| 表格 | 按显示宽度对齐，中文算两列 |
| 列表 | 紧凑与松散按 CommonMark 区分 |

```rust
fn main() {
    for line in mcat::render("README.md") {
        println!("{line}");
    }
}
```
