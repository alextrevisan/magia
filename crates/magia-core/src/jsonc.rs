//! JSONC support and OpenCode-style substitutions for config files.

use serde_json::Value;
use std::path::Path;

/// Remove `//` and `/* */` comments and trailing commas. Everything removed is replaced by
/// spaces (newlines kept) so parse errors still report the right line and column.
pub fn strip_jsonc(src: &str) -> String {
    let c: Vec<char> = src.chars().collect();
    let mut out: Vec<char> = Vec::with_capacity(c.len());
    let (mut i, mut in_str) = (0, false);
    while i < c.len() {
        let ch = c[i];
        if in_str {
            out.push(ch);
            if ch == '\\' && i + 1 < c.len() {
                out.push(c[i + 1]);
                i += 1;
            } else if ch == '"' {
                in_str = false;
            }
        } else if ch == '"' {
            in_str = true;
            out.push(ch);
        } else if ch == '/' && c.get(i + 1) == Some(&'/') {
            while i < c.len() && c[i] != '\n' {
                out.push(' ');
                i += 1;
            }
            continue;
        } else if ch == '/' && c.get(i + 1) == Some(&'*') {
            while i < c.len() && !(c[i] == '*' && c.get(i + 1) == Some(&'/')) {
                out.push(if c[i] == '\n' { '\n' } else { ' ' });
                i += 1;
            }
            out.extend([' ', ' ']);
            i += 2;
            continue;
        } else {
            out.push(ch);
        }
        i += 1;
    }
    // Trailing commas: a `,` outside strings followed only by whitespace and `}` / `]`.
    let mut in_str = false;
    let mut j = 0;
    while j < out.len() {
        let ch = out[j];
        if in_str {
            if ch == '\\' {
                j += 1;
            } else if ch == '"' {
                in_str = false;
            }
        } else if ch == '"' {
            in_str = true;
        } else if ch == ','
            && let Some(next) = out[j + 1..].iter().find(|c| !c.is_whitespace())
            && (*next == '}' || *next == ']')
        {
            out[j] = ' ';
        }
        j += 1;
    }
    out.into_iter().collect()
}

/// Recursively merge `over` into `base`: objects merge key by key, everything else is replaced.
pub fn deep_merge(base: &mut Value, over: Value) {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(existing) => deep_merge(existing, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, o) => *b = o,
    }
}

/// Replace `{env:NAME}` and `{file:PATH}` inside every string value. An unset variable or a
/// missing file becomes an empty string (same as OpenCode). Paths may be absolute, `~/…`, or
/// relative to the config file.
pub fn substitute(v: &mut Value, base_dir: &Path) {
    match v {
        Value::String(s) => *s = expand_str(s, base_dir),
        Value::Array(a) => a.iter_mut().for_each(|x| substitute(x, base_dir)),
        Value::Object(o) => o.values_mut().for_each(|x| substitute(x, base_dir)),
        _ => {}
    }
}

fn expand_str(s: &str, base_dir: &Path) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let Some(end) = tail.find('}') else {
            out.push_str(tail);
            return out;
        };
        let inner = &tail[1..end];
        if let Some(name) = inner.strip_prefix("env:") {
            out.push_str(&std::env::var(name.trim()).unwrap_or_default());
        } else if let Some(p) = inner.strip_prefix("file:") {
            let p = p.trim();
            let path = if let Some(h) = p.strip_prefix("~/") {
                std::env::var_os("HOME")
                    .map(|home| Path::new(&home).join(h))
                    .unwrap_or_default()
            } else if Path::new(p).is_absolute() {
                p.into()
            } else {
                base_dir.join(p)
            };
            out.push_str(
                std::fs::read_to_string(path)
                    .unwrap_or_default()
                    .trim_end_matches(['\n', '\r']),
            );
        } else {
            out.push_str(&tail[..=end]); // not ours: keep as is
        }
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strips_comments_and_trailing_commas_but_not_inside_strings() {
        let src = "{\n // line\n \"a\": \"http://x // not a comment\", /* block */\n \"b\": [1, 2,],\n}\n";
        let v: Value = serde_json::from_str(&strip_jsonc(src)).unwrap();
        assert_eq!(v, json!({"a": "http://x // not a comment", "b": [1, 2]}));
    }

    #[test]
    fn substitutes_env_and_file() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("key.txt"), "from-file\n").unwrap();
        // SAFETY: tests in this module are the only users of this variable.
        unsafe { std::env::set_var("MAGIA_TEST_VAR", "from-env") };
        let mut v = json!({"a": "{env:MAGIA_TEST_VAR}", "b": ["x-{file:key.txt}-y"], "c": "{env:MAGIA_UNSET_XYZ}", "d": "{keep}"});
        substitute(&mut v, d.path());
        assert_eq!(
            v,
            json!({"a": "from-env", "b": ["x-from-file-y"], "c": "", "d": "{keep}"})
        );
    }

    #[test]
    fn deep_merge_objects_replace_scalars() {
        let mut a = json!({"p": {"x": 1, "y": 2}, "m": "a", "l": [1]});
        deep_merge(&mut a, json!({"p": {"y": 3, "z": 4}, "m": "b", "l": [2]}));
        assert_eq!(
            a,
            json!({"p": {"x": 1, "y": 3, "z": 4}, "m": "b", "l": [2]})
        );
    }
}
