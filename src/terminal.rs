//! 终端背景探测：判断当前终端是深色还是浅色。
//!
//! mermaid 图的配色要和终端背景一致，否则深色终端上会出现一块白底。
//! 先用 OSC 11 问终端要背景色，问不到再看 `COLORFGBG`，都没有就返回
//! `None`，由调用方按亮色处理。
//!
//! 查询只在真的需要画图时发生：一次运行只查一次，没有 mermaid 块的文档
//! 不会为此等待。

use std::io::{Read, Write};

/// 终端背景是不是深色。探测不出来返回 `None`。
pub fn is_dark() -> Option<bool> {
    #[cfg(unix)]
    if let Some(rgb) = query_rgb() {
        return Some(luma(rgb) < 0.5);
    }
    colorfgbg()
}

/// OSC 11 查询。终端回答形如 `ESC ] 11 ; rgb:rrrr/gggg/bbbb BEL`。
///
/// 直接开 `/dev/tty`：标准输入、输出、错误都可能被重定向，但控制终端
/// 只有一个。这样 `mcat --check-images > 报告.txt` 也能问出真实背景。
/// 没有控制终端（比如在 CI 或后台跑）时才退回 `COLORFGBG`。
#[cfg(unix)]
fn query_rgb() -> Option<(u8, u8, u8)> {
    use rustix::termios::{OptionalActions, SpecialCodeIndex, Termios, tcgetattr, tcsetattr};
    use std::time::{Duration, Instant};

    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()?;

    // 先把恢复用的 fd 复制出来：等终端进了原始模式再失败，就没人把它
    // 改回去了。复制失败时还没动过终端属性，直接退出是安全的。
    let restore_fd = tty.try_clone().ok()?;

    // 原始模式：回复里没有换行，规范模式会一直等到用户按回车。
    let saved = tcgetattr(&tty).ok()?;
    let mut raw = saved.clone();
    raw.make_raw();
    // VMIN=0 + VTIME=1：读不到数据时最多等 0.1 秒，不把 mcat 卡住。
    raw.special_codes[SpecialCodeIndex::VMIN] = 0;
    raw.special_codes[SpecialCodeIndex::VTIME] = 1;
    tcsetattr(&tty, OptionalActions::Now, &raw).ok()?;

    // 不管怎么退出都要恢复终端属性，否则会把用户的终端留在原始模式。
    // 用 dup 出来的 fd：读回复要可变借用原文件，跟守卫错开。
    struct Restore {
        tty: std::fs::File,
        saved: Termios,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = tcsetattr(&self.tty, OptionalActions::Now, &self.saved);
        }
    }
    let _restore = Restore {
        tty: restore_fd,
        saved,
    };

    tty.write_all(query()).ok()?;
    tty.flush().ok()?;

    let mut reply = Vec::new();
    let mut chunk = [0u8; 64];
    // 终端一般几毫秒就回；不支持的终端最多等一次 VTIME（0.1 秒）。
    let deadline = Instant::now() + Duration::from_millis(120);
    loop {
        let n = tty.read(&mut chunk).ok()?;
        if n > 0 {
            reply.extend_from_slice(&chunk[..n]);
            if let Some(rgb) = parse_osc11(&reply) {
                return Some(rgb);
            }
            // 回复不会这么长，再多就是别的输入混进来了
            if reply.len() > 256 {
                return None;
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
    }
}

/// tmux 要把 OSC 序列包进 DCS 才会转给外层终端。
#[cfg(unix)]
fn query() -> &'static [u8] {
    if std::env::var_os("TMUX").is_some() {
        b"\x1bPtmux;\x1b\x1b]11;?\x07\x1b\\"
    } else {
        b"\x1b]11;?\x07"
    }
}

/// 从终端回复里取出 `rgb:` 后面的颜色。
fn parse_osc11(reply: &[u8]) -> Option<(u8, u8, u8)> {
    let text = std::str::from_utf8(reply).ok()?;
    let rest = &text[text.find("]11;rgb:")? + "]11;rgb:".len()..];
    let mut parts = rest.split('/');
    Some((
        hex_byte(parts.next()?)?,
        hex_byte(parts.next()?)?,
        hex_byte(parts.next()?)?,
    ))
}

/// 响应里的每个通道有 1–4 位十六进制，取最高的两位就够判断明暗。
fn hex_byte(value: &str) -> Option<u8> {
    let digits = if value.len() >= 2 && value.is_char_boundary(2) {
        &value[..2]
    } else {
        value
    };
    u8::from_str_radix(digits, 16).ok()
}

/// `COLORFGBG` 形如 `15;0`，最后一段是背景色的 ANSI 编号。
///
/// 0–6 和 8 按深色算，7（浅灰）和 9–15（亮色）按浅色算。
fn colorfgbg() -> Option<bool> {
    from_colorfgbg(&std::env::var("COLORFGBG").ok()?)
}

fn from_colorfgbg(value: &str) -> Option<bool> {
    let background: u8 = value.rsplit(';').next()?.trim().parse().ok()?;
    Some(!matches!(background, 7 | 9..=15))
}

/// 相对亮度，0 是黑、1 是白。
fn luma((r, g, b): (u8, u8, u8)) -> f32 {
    (0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b)) / 255.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析_osc11_回复() {
        assert_eq!(
            parse_osc11(b"\x1b]11;rgb:1e1e/2020/3030\x07"),
            Some((0x1e, 0x20, 0x30))
        );
        // 四位精度：只取最高两位
        assert_eq!(
            parse_osc11(b"\x1b]11;rgb:3838/a4a4/c9c9\x1b\\"),
            Some((0x38, 0xa4, 0xc9))
        );
        // 两位精度
        assert_eq!(
            parse_osc11(b"\x1b]11;rgb:ff/80/00\x07"),
            Some((0xff, 0x80, 0x00))
        );
        // tmux 的 DCS 包装不影响解析
        assert_eq!(
            parse_osc11(b"\x1bPtmux;\x1b\x1b]11;rgb:0000/0000/0000\x1b\x1b\\"),
            Some((0, 0, 0))
        );
    }

    #[test]
    fn 不认识的回复不算数() {
        assert_eq!(parse_osc11(b""), None);
        assert_eq!(parse_osc11(b"\x1b]10;rgb:ffff/ffff/ffff\x07"), None);
        assert_eq!(parse_osc11(b"\x1b]11;rgb:zz/00/00\x07"), None);
        assert_eq!(parse_osc11(b"\x1b]11;rgb:ff/00\x07"), None);
    }

    #[test]
    fn 亮度按背景判断明暗() {
        assert!(luma((0, 0, 0)) < 0.5);
        assert!(luma((30, 32, 48)) < 0.5);
        assert!(luma((255, 255, 255)) > 0.5);
        assert!(luma((250, 240, 220)) > 0.5);
    }

    #[test]
    fn colorfgbg_的最后一段是背景() {
        assert_eq!(from_colorfgbg("15;0"), Some(true)); // 黑底
        assert_eq!(from_colorfgbg("0;8"), Some(true)); // 深灰底
        assert_eq!(from_colorfgbg("0;7"), Some(false)); // 浅灰底
        assert_eq!(from_colorfgbg("0;15"), Some(false)); // 白底
        assert_eq!(from_colorfgbg("default;default"), None);
        assert_eq!(from_colorfgbg(""), None);
    }
}
