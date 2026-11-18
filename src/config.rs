//! 配置文件。
//!
//! 默认值放在 TOML 文件里，命令行显式给出的参数优先。合并的做法是把配置项
//! 翻译成命令行参数、插在真实参数前面再解析一次——覆盖关系交给 clap 的
//! `args_override_self`，不用为每个选项再写一套「命令行有没有给」的判断。

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// 所有配置项。名字就是命令行选项的长名，下划线写法。
const KEYS: &[&str] = &[
    "theme",
    "mermaid_theme",
    "image_protocol",
    "image_rows",
    "image_cols",
    "numbers",
    "no_color",
    "no_images",
];

/// 配置文件里给每一项设的默认值。`None` 表示没写，保持命令行的默认。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    theme: Option<String>,
    mermaid_theme: Option<String>,
    image_protocol: Option<String>,
    image_rows: Option<usize>,
    image_cols: Option<usize>,
    numbers: Option<bool>,
    no_color: Option<bool>,
    no_images: Option<bool>,
}

impl Config {
    pub fn from_toml(text: &str) -> Result<Config, String> {
        let value: toml::Value =
            toml::from_str(text).map_err(|e| e.to_string().trim_end().to_string())?;
        let table = value.as_table().expect("TOML 顶层一定是表");
        let mut config = Config::default();
        let mut seen: Vec<String> = Vec::new();
        for (raw, value) in table {
            let key = raw.replace('-', "_");
            if !KEYS.contains(&key.as_str()) {
                return Err(format!("未知配置项 {raw:?}，可用：{}", KEYS.join("、")));
            }
            // 同一个键用 `image_rows` 和 `image-rows` 各写一遍
            if seen.contains(&key) {
                return Err(format!("配置项 {key} 写了不止一次"));
            }
            seen.push(key.clone());
            config.set(&key, value)?;
        }
        Ok(config)
    }

    fn set(&mut self, key: &str, value: &toml::Value) -> Result<(), String> {
        match key {
            "theme" => self.theme = Some(text(key, value)?),
            "mermaid_theme" => self.mermaid_theme = Some(text(key, value)?),
            "image_protocol" => self.image_protocol = Some(text(key, value)?),
            "image_rows" => self.image_rows = Some(count(key, value)?),
            "image_cols" => self.image_cols = Some(count(key, value)?),
            "numbers" => self.numbers = Some(flag(key, value)?),
            "no_color" => self.no_color = Some(flag(key, value)?),
            "no_images" => self.no_images = Some(flag(key, value)?),
            _ => unreachable!("配置项已经按 KEYS 校验过"),
        }
        Ok(())
    }

    /// 翻译成命令行参数。布尔项的正反两个方向都显式给出来，
    /// 这样 `false` 也不依赖命令行的默认值。
    pub fn args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(theme) = &self.theme {
            args.push(format!("--theme={theme}"));
        }
        if let Some(theme) = &self.mermaid_theme {
            args.push(format!("--mermaid-theme={theme}"));
        }
        if let Some(protocol) = &self.image_protocol {
            args.push(format!("--image-protocol={protocol}"));
        }
        if let Some(rows) = self.image_rows {
            args.push(format!("--image-rows={rows}"));
        }
        if let Some(cols) = self.image_cols {
            args.push(format!("--image-cols={cols}"));
        }
        if let Some(value) = self.numbers {
            args.push(if value { "--numbers" } else { "--no-numbers" }.to_string());
        }
        if let Some(value) = self.no_color {
            args.push(if value { "--no-color" } else { "--color" }.to_string());
        }
        if let Some(value) = self.no_images {
            args.push(if value { "--no-images" } else { "--images" }.to_string());
        }
        args
    }
}

fn text(key: &str, value: &toml::Value) -> Result<String, String> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{key} 需要字符串，实际是{}", kind(value)))
}

fn count(key: &str, value: &toml::Value) -> Result<usize, String> {
    let Some(n) = value.as_integer() else {
        return Err(format!("{key} 需要非负整数，实际是{}", kind(value)));
    };
    usize::try_from(n).map_err(|_| format!("{key} 需要非负整数，实际是 {n}"))
}

fn flag(key: &str, value: &toml::Value) -> Result<bool, String> {
    value
        .as_bool()
        .ok_or_else(|| format!("{key} 需要 true 或 false，实际是{}", kind(value)))
}

fn kind(value: &toml::Value) -> &'static str {
    match value {
        toml::Value::String(_) => "字符串",
        toml::Value::Integer(_) => "整数",
        toml::Value::Float(_) => "浮点数",
        toml::Value::Boolean(_) => "布尔值",
        toml::Value::Datetime(_) => "日期",
        toml::Value::Array(_) => "数组",
        toml::Value::Table(_) => "表",
    }
}

/// 配置文件从哪来。显式指定的文件不存在是错误，默认路径不存在就跳过。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    Explicit,
    Env,
    Default,
}

fn resolve(
    explicit: Option<&Path>,
    env: Option<&OsStr>,
    xdg: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Option<(PathBuf, Origin)> {
    if let Some(path) = explicit {
        return Some((path.to_path_buf(), Origin::Explicit));
    }
    if let Some(path) = env.filter(|path| !path.is_empty()) {
        return Some((PathBuf::from(path), Origin::Env));
    }
    // XDG 规范里空值等同于没设置
    if let Some(dir) = xdg.filter(|dir| !dir.is_empty()) {
        return Some((PathBuf::from(dir).join("mcat/config.toml"), Origin::Default));
    }
    if let Some(home) = home.filter(|home| !home.is_empty()) {
        return Some((
            PathBuf::from(home).join(".config/mcat/config.toml"),
            Origin::Default,
        ));
    }
    None
}

/// 读配置文件。返回 `None` 表示没有配置文件，不是错误。
pub fn load(explicit: Option<&Path>) -> Result<Option<(PathBuf, Config)>, String> {
    let env = std::env::var_os("MCAT_CONFIG");
    let xdg = std::env::var_os("XDG_CONFIG_HOME");
    let home = std::env::var_os("HOME");
    let Some((path, origin)) = resolve(explicit, env.as_deref(), xdg.as_deref(), home.as_deref())
    else {
        return Ok(None);
    };
    load_at(path, origin)
}

fn load_at(path: PathBuf, origin: Origin) -> Result<Option<(PathBuf, Config)>, String> {
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && origin == Origin::Default => {
            return Ok(None);
        }
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            return Err(format!("配置文件 {} 不是 UTF-8：{e}", path.display()));
        }
        Err(e) => return Err(format!("配置文件 {} 读不了：{e}", path.display())),
    };
    let config =
        Config::from_toml(&text).map_err(|e| format!("配置文件 {} 有误：{e}", path.display()))?;
    Ok(Some((path, config)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config, String> {
        Config::from_toml(text)
    }

    fn temp_file(name: &str, contents: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(format!("mcat-config-{}-{name}", std::process::id()));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn 仓库里的示例配置能解析() {
        let text = include_str!("../examples/config.toml");
        Config::from_toml(text).unwrap();
    }

    #[test]
    fn 解析全部配置项() {
        let config = parse(
            r#"
# 注释
theme = "base16-mocha.dark"
mermaid_theme = "dark"
image_protocol = "kitty"
image_rows = 12
image_cols = 80
numbers = true
no_color = true
no_images = false
"#,
        )
        .unwrap();
        assert_eq!(
            config,
            Config {
                theme: Some("base16-mocha.dark".into()),
                mermaid_theme: Some("dark".into()),
                image_protocol: Some("kitty".into()),
                image_rows: Some(12),
                image_cols: Some(80),
                numbers: Some(true),
                no_color: Some(true),
                no_images: Some(false),
            }
        );
    }

    #[test]
    fn 键名也认连字符写法() {
        let config = parse(
            r#"
mermaid-theme = "light"
image-rows = 8
no-color = true
"#,
        )
        .unwrap();
        assert_eq!(config.mermaid_theme.as_deref(), Some("light"));
        assert_eq!(config.image_rows, Some(8));
        assert_eq!(config.no_color, Some(true));
    }

    #[test]
    fn 同一项用两种写法写两遍会报错() {
        let e = parse("image_rows = 1\nimage-rows = 2").unwrap_err();
        assert!(e.contains("不止一次"), "{e}");
    }

    #[test]
    fn 未知配置项报错并列出可用项() {
        let e = parse("colour = true").unwrap_err();
        assert!(e.contains("未知配置项"), "{e}");
        assert!(e.contains("theme"), "{e}");
        assert!(e.contains("no_color"), "{e}");
    }

    #[test]
    fn 类型不对时报出期望类型和实际类型() {
        assert!(parse("theme = 1").unwrap_err().contains("需要字符串"));
        assert!(
            parse("numbers = \"yes\"")
                .unwrap_err()
                .contains("需要 true 或 false")
        );
        assert!(
            parse("image_rows = \"12\"")
                .unwrap_err()
                .contains("需要非负整数")
        );
        assert!(
            parse("image_rows = -1")
                .unwrap_err()
                .contains("需要非负整数")
        );
        assert!(parse("theme = 1.5").unwrap_err().contains("浮点数"));
    }

    #[test]
    fn 空配置和只有注释的配置都是空表() {
        assert_eq!(parse("").unwrap(), Config::default());
        assert_eq!(parse("# 注释\n\n").unwrap(), Config::default());
    }

    #[test]
    fn 布尔项翻译成正反两个参数() {
        let config = parse("numbers = true\nno_color = false\nno_images = true").unwrap();
        assert_eq!(config.args(), ["--numbers", "--color", "--no-images"]);
    }

    #[test]
    fn 数值项翻译成等号参数() {
        let config = parse("image_rows = 12\nimage_cols = 80").unwrap();
        assert_eq!(config.args(), ["--image-rows=12", "--image-cols=80"]);
    }

    #[test]
    fn 路径优先级是_config_然后环境变量然后_xdg_然后_home() {
        let explicit = Path::new("/tmp/explicit.toml");
        let env = OsStr::new("/tmp/env.toml");
        let xdg = OsStr::new("/xdg");
        let home = OsStr::new("/home/u");

        assert_eq!(
            resolve(Some(explicit), Some(env), Some(xdg), Some(home)),
            Some((explicit.to_path_buf(), Origin::Explicit))
        );
        assert_eq!(
            resolve(None, Some(env), Some(xdg), Some(home)),
            Some((PathBuf::from("/tmp/env.toml"), Origin::Env))
        );
        assert_eq!(
            resolve(None, None, Some(xdg), Some(home)),
            Some((PathBuf::from("/xdg/mcat/config.toml"), Origin::Default))
        );
        assert_eq!(
            resolve(None, None, None, Some(home)),
            Some((
                PathBuf::from("/home/u/.config/mcat/config.toml"),
                Origin::Default
            ))
        );
        assert_eq!(resolve(None, None, None, None), None);
    }

    #[test]
    fn 环境变量为空时当作没设置() {
        assert_eq!(
            resolve(None, Some(OsStr::new("")), Some(OsStr::new("/xdg")), None),
            Some((PathBuf::from("/xdg/mcat/config.toml"), Origin::Default))
        );
    }

    #[test]
    fn 默认路径不存在时静默跳过() {
        let path = std::env::temp_dir().join(format!("mcat-missing-{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert_eq!(load_at(path, Origin::Default), Ok(None));
    }

    #[test]
    fn 显式指定的文件不存在时报错() {
        let path = std::env::temp_dir().join(format!("mcat-missing-{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let e = load_at(path.clone(), Origin::Explicit).unwrap_err();
        assert!(e.contains(&path.display().to_string()), "{e}");
        assert!(e.contains("读不了"), "{e}");
    }

    #[test]
    fn 从文件读配置() {
        let path = temp_file("ok.toml", b"theme = \"base16-mocha.dark\"\n");
        let (read, config) = load_at(path.clone(), Origin::Explicit).unwrap().unwrap();
        assert_eq!(read, path);
        assert_eq!(config.theme.as_deref(), Some("base16-mocha.dark"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn 解析失败时错误里带文件路径() {
        let path = temp_file("bad.toml", b"theme = \n");
        let e = load_at(path.clone(), Origin::Explicit).unwrap_err();
        assert!(e.contains(&path.display().to_string()), "{e}");
        assert!(e.contains("TOML parse error"), "{e}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn 不是_utf8_时报错() {
        let path = temp_file("latin1.toml", b"theme = \"\xff\"\n");
        let e = load_at(path.clone(), Origin::Explicit).unwrap_err();
        assert!(e.contains("不是 UTF-8"), "{e}");
        let _ = std::fs::remove_file(path);
    }
}
