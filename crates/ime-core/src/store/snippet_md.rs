//! SNIP 目录里的 markdown 片段加载器。
//!
//! 每个 `.md` 文件可含**多个** snippet:每段以 `---` 包裹的前导元数据
//! (`name` / `comment` / `params`)开头,紧跟一个三反引号围栏代码块,其内容
//! 即模板。模板首行/结尾换行符被剥掉(内部换行保留)。
//!
//! 示例(hello 片段):
//!
//! ```text
//! ---
//! name: hello
//! comment: 显示在候选区域
//! params: name, age
//! ---
//! hello, my name is ${name}. I am ${age} years old.
//! ```

use std::path::Path;

/// 一条从 SNIP md 加载的片段。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnippetEntry {
    /// 片段名(`#/hello` 的 hello)。
    pub name: String,
    /// 候选区显示的说明文字。
    pub comment: String,
    /// 声明的查询参数名(`?name=Mike` 的 name)。
    pub params: Vec<String>,
    /// 模板正文(含 `${name}`/`${PATH_VAR}`/`${ENV:…}` 变量)。
    pub template: String,
}

/// 解析一个 md 文件内容,返回其中所有片段。
pub fn parse_md(content: &str) -> Vec<SnippetEntry> {
    let mut out = Vec::new();
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0usize;
    while i < lines.len() {
        // 定位前导 `---`
        if lines[i].trim() != "---" {
            i += 1;
            continue;
        }
        i += 1;
        // 收集 frontmatter 到下一个 `---`
        let mut fm: Vec<(String, String)> = Vec::new();
        while i < lines.len() && lines[i].trim() != "---" {
            if let Some((k, v)) = lines[i].split_once(':') {
                fm.push((k.trim().to_string(), v.trim().to_string()));
            }
            i += 1;
        }
        if i >= lines.len() {
            break; // 缺 closing `---`
        }
        i += 1; // 跳过 closing `---`
        // 找代码块开头 ```
        while i < lines.len() && !lines[i].trim_start().starts_with("```") {
            i += 1;
        }
        if i >= lines.len() {
            break;
        }
        i += 1; // 跳过 ```
        // 收集模板到 closing ```
        let mut tpl = String::new();
        while i < lines.len() && !lines[i].trim_start().starts_with("```") {
            tpl.push_str(lines[i]);
            tpl.push('\n');
            i += 1;
        }
        if i < lines.len() {
            i += 1; // 跳过 closing ```
        }
        let template = tpl.trim_matches('\n').to_string();
        let name = fm.iter().find(|(k, _)| k == "name").map(|(_, v)| v.clone()).unwrap_or_default();
        let comment = fm.iter().find(|(k, _)| k == "comment").map(|(_, v)| v.clone()).unwrap_or_default();
        let params = fm
            .iter()
            .find(|(k, _)| k == "params")
            .map(|(_, v)| {
                v.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if !name.is_empty() {
            out.push(SnippetEntry {
                name,
                comment,
                params,
                template,
            });
        }
    }
    out
}

/// 递归加载一个目录下所有 `.md` 片段(非目录/不可读静默跳过)。
pub fn load_dir(dir: &Path) -> Vec<SnippetEntry> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if !p.extension().is_some_and(|x| x == "md") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&p) else {
            tracing::warn!(path = %p.display(), "snippet md 读取失败");
            continue;
        };
        out.extend(parse_md(&content));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_snippet_with_frontmatter() {
        let md = "---\nname: hello\ncomment: 你好\nparams: name, age\n---\n```\nhello, my name is ${name}. I am ${age} years old.\n```\n";
        let got = parse_md(md);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "hello");
        assert_eq!(got[0].comment, "你好");
        assert_eq!(got[0].params, vec!["name", "age"]);
        assert_eq!(got[0].template, "hello, my name is ${name}. I am ${age} years old.");
    }

    #[test]
    fn trims_only_edge_newlines_keeps_inner() {
        let md = "---\nname: angle\ncomment: angle\n---\n```\n   ${PATH_VAR}\n  ${PATH_VAR}\n ${PATH_VAR}\n${PATH_VAR}${PATH_VAR}${PATH_VAR}${PATH_VAR}\n```\n";
        let got = parse_md(md);
        assert_eq!(
            got[0].template,
            "   ${PATH_VAR}\n  ${PATH_VAR}\n ${PATH_VAR}\n${PATH_VAR}${PATH_VAR}${PATH_VAR}${PATH_VAR}"
        );
    }

    #[test]
    fn parses_multiple_snippets_in_one_file() {
        let md = "\
---
name: hello
comment: hi
---
```
hello world
```

---
name: angle
comment: angle
---
```
O
```
";
        let got = parse_md(md);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name, "hello");
        assert_eq!(got[1].name, "angle");
    }
}
