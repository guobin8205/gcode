use thiserror::Error;
use xxhash_rust::xxh32::xxh32;

#[derive(Debug, Error)]
pub enum HashlineError {
    #[error("invalid anchor format '{0}', expected '<line>:<4-hex>'")]
    InvalidAnchor(String),
    #[error("line {0} is out of bounds (file has {1} lines)")]
    OutOfBounds(usize, usize),
    #[error("stale read at line {line}: expected hash {expected}, found {actual}")]
    StaleHash {
        line: usize,
        expected: String,
        actual: String,
    },
    #[error("end line {end} is before start line {start}")]
    InvalidRange { start: usize, end: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HashlineAnchor {
    pub line: usize,
    pub hash: String,
}

impl HashlineAnchor {
    pub fn parse(raw: &str) -> Result<Self, HashlineError> {
        let (line_str, hash_str) = raw
            .split_once(':')
            .ok_or_else(|| HashlineError::InvalidAnchor(raw.to_string()))?;
        let line: usize = line_str
            .trim()
            .parse()
            .map_err(|_| HashlineError::InvalidAnchor(raw.to_string()))?;
        if line == 0 || hash_str.len() != 4 {
            return Err(HashlineError::InvalidAnchor(raw.to_string()));
        }
        Ok(Self {
            line,
            hash: hash_str.to_lowercase(),
        })
    }
}

/// 计算单行内容（去除末尾 `\r`/`\n`）的 4 位十六进制短哈希。
pub fn line_hash(line: &str) -> String {
    let trimmed = line.trim_end_matches(&['\r', '\n'][..]);
    let digest = xxh32(trimmed.as_bytes(), 0);
    format!("{:04x}", digest & 0xffff)
}

/// 为文件内容添加 `行号:哈希| 代码` 前缀，供 `read_file` 输出。
pub fn format_with_hashlines(content: &str, start_line: usize) -> String {
    content
        .lines()
        .enumerate()
        .map(|(idx, line)| {
            let line_no = start_line + idx;
            format!("{line_no}:{}| {line}", line_hash(line))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 校验起止行锚点与磁盘内容哈希一致后，替换 `[start..=end]`（从 1 开始计行）范围内的代码。
pub fn apply_hashline_replace(
    content: &str,
    start: &HashlineAnchor,
    end: Option<&HashlineAnchor>,
    replacement: &str,
) -> Result<String, HashlineError> {
    let mut lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let end_anchor = end.unwrap_or(start);

    if end_anchor.line < start.line {
        return Err(HashlineError::InvalidRange {
            start: start.line,
            end: end_anchor.line,
        });
    }
    if start.line > total {
        return Err(HashlineError::OutOfBounds(start.line, total));
    }
    if end_anchor.line > total {
        return Err(HashlineError::OutOfBounds(end_anchor.line, total));
    }

    for anchor in [start, end_anchor] {
        let actual = line_hash(lines[anchor.line - 1]);
        if actual != anchor.hash {
            return Err(HashlineError::StaleHash {
                line: anchor.line,
                expected: anchor.hash.clone(),
                actual,
            });
        }
    }

    let replacement_lines: Vec<&str> = if replacement.is_empty() {
        Vec::new()
    } else {
        replacement.lines().collect()
    };
    lines.splice((start.line - 1)..end_anchor.line, replacement_lines);
    let mut out = lines.join("\n");
    if content.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashline_replace_succeeds_and_rejects_stale_hash() {
        let src = "fn main() {\n    println!(\"old\");\n}\n";
        let anchor = HashlineAnchor {
            line: 2,
            hash: line_hash("    println!(\"old\");"),
        };
        let updated = apply_hashline_replace(src, &anchor, None, "    println!(\"new\");").unwrap();
        assert!(updated.contains("\"new\""));

        let err = apply_hashline_replace(&updated, &anchor, None, "x").unwrap_err();
        assert!(matches!(err, HashlineError::StaleHash { line: 2, .. }));
    }
}
