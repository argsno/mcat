# mcat 测试文档

普通段落里有 **粗体**、*斜体*、~~删除线~~、`行内代码`，还有一个
[外部链接](https://example.com/very/long/path?query=1&x=2) 和
![示意图](docs/diagram.png)。

## 二级标题

### 三级标题

- 无序列表第一项
- 第二项里有个 `code`

  还有一段缩进的续行

- 第三项

1. 有序列表
2. 第二条
10. 第十条（CommonMark 会保留起始编号）

- [x] 已完成的任务
- [ ] 没做完的任务

### 嵌套与引用

> 引用里也可以有 **格式** 和 `代码`。
>
> - 引用里的列表
> - 第二条
>
> ```bash
> echo "引用里的代码块"
> ```

## 代码块

```rust
fn main() {
    let numbers: Vec<i32> = (1..=10).collect();
    for n in &numbers {
        if n % 2 == 0 {
            println!("{n} 是偶数");
        }
    }
}
```

没有语言标注的代码块：

```
$ mcat README.md
$ mcat -n src/main.rs
```

缩进四格的代码块：

    indented code block
    second line

## 表格

| 特性 | 支持 | 说明 |
|---|:---:|---:|
| Markdown 渲染 | 是 | 标题、列表、引用、表格 |
| 语法高亮 | 是 | syntect，纯 Rust 后端 |
| 中文对齐 | 是 | 按显示宽度计算 |
| `cat` 兼容 | 部分 | 多文件、stdin、`-n` |

## 分隔线与硬换行

上面是分隔线。

---

这一行末尾有两个空格  
所以在这里换行。

## 图片与脚注

![架构图](https://example.com/arch.png "标题在这里")

脚注引用[^1] 和另一个[^note]。

[^1]: 这是第一个脚注。

[^note]: 这是第二个脚注。
