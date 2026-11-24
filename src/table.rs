//! 表格公共排版：列宽压缩、按显示宽度补齐。
//!
//! Markdown 和 reStructuredText 解析出来的表格结构不同，但终端里的
//! 框线、列宽和折行规则是同一套，放在这里两边共用。

use crate::style::display_width;

/// 单元格对齐方式。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Align {
    #[default]
    None,
    Left,
    Center,
    Right,
}

impl From<pulldown_cmark::Alignment> for Align {
    fn from(align: pulldown_cmark::Alignment) -> Self {
        match align {
            pulldown_cmark::Alignment::Left => Align::Left,
            pulldown_cmark::Alignment::Center => Align::Center,
            pulldown_cmark::Alignment::Right => Align::Right,
            pulldown_cmark::Alignment::None => Align::None,
        }
    }
}

/// 表格能用的内容宽度：终端宽度减掉边框、内边距和行首前缀。
/// 没有终端宽度（管道、重定向）时返回 `usize::MAX`，也就是不折行。
pub fn available(width: Option<usize>, prefix_width: usize, columns: usize) -> usize {
    let Some(term) = width else {
        return usize::MAX;
    };
    let frame = 3 * columns + 1 + prefix_width;
    term.saturating_sub(frame)
}

/// 把各列的自然宽度压进 `budget`：挑当前最宽的列一格格削，直到放得下。
///
/// 每列至少留两格：一个汉字或 emoji 就占两格，再窄就一定会把内容挤到框线外面去。
/// 连这个下限都放不下时保持原样——宁可整张表超出终端宽度（让它自己折行），
/// 也不能把框线挤断。
pub fn fit_widths(natural: &[usize], budget: usize) -> Vec<usize> {
    let floor: Vec<usize> = natural.iter().map(|w| (*w).min(2)).collect();
    if budget < floor.iter().sum::<usize>() {
        return natural.to_vec();
    }
    let mut widths = natural.to_vec();
    while widths.iter().sum::<usize>() > budget {
        let widest = widths
            .iter()
            .zip(&floor)
            .enumerate()
            .filter(|(_, (w, low))| w > low)
            .max_by_key(|(_, (w, _))| **w)
            .map(|(i, _)| i);
        match widest {
            Some(i) => widths[i] -= 1,
            None => break,
        }
    }
    widths
}

/// 单元格内容按对齐方式补齐，两侧各留一格空格。
/// 宽度用的是显示宽度，所以中文也能对齐。
pub fn pad(text: &str, width: usize, align: Align) -> String {
    let fill = width.saturating_sub(display_width(text));
    let body = match align {
        Align::Center => {
            let left = fill / 2;
            format!("{}{}{}", " ".repeat(left), text, " ".repeat(fill - left))
        }
        Align::Right => format!("{}{}", " ".repeat(fill), text),
        _ => format!("{}{}", text, " ".repeat(fill)),
    };
    format!(" {body} ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 宽度按显示宽度补齐() {
        assert_eq!(pad("中文", 4, Align::Left), " 中文 ");
        assert_eq!(pad("x", 4, Align::Right), "    x ");
        assert_eq!(pad("x", 4, Align::Center), "  x   ");
    }

    #[test]
    fn 放不下时保持自然宽度() {
        assert_eq!(fit_widths(&[10, 10], 3), vec![10, 10]);
        assert_eq!(fit_widths(&[10, 10], 20), vec![10, 10]);
        assert_eq!(fit_widths(&[10, 4], 10), vec![6, 4]);
    }
}
