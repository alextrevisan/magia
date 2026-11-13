use crate::{Tool, ToolCx, ToolOutput, truncate_output};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;

const MAX_OUT: usize = 30_000;

pub fn builtin_tools() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(Read),
        Box::new(Write),
        Box::new(Edit),
        Box::new(Bash),
        Box::new(Ls),
        Box::new(Glob),
        Box::new(Grep),
        Box::new(TodoTool),
        Box::new(crate::web::WebFetch),
        Box::new(crate::web::WebSearch),
    ]
}

fn str_arg<'a>(v: &'a Value, k: &str) -> Result<&'a str> {
    v.get(k)
        .and_then(Value::as_str)
        .with_context(|| format!("missing string argument `{k}`"))
}

// ───────────── read ─────────────
struct Read;
#[async_trait]
impl Tool for Read {
    fn name(&self) -> &'static str {
        "read"
    }
    fn description(&self) -> &'static str {
        "Read a file. Text comes back as numbered lines (use offset/limit for large files). \
An image (PNG, JPEG, GIF, WebP) comes back as the picture itself, so you can look at screenshots and renders."
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{
            "file_path":{"type":"string"},
            "offset":{"type":"integer","description":"1-based first line"},
            "limit":{"type":"integer","description":"max lines (default 2000)"}},
            "required":["file_path"]})
    }
    fn read_only(&self) -> bool {
        true
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> Result<String> {
        Ok(self.call_output(input, cx).await?.text)
    }
    async fn call_output(&self, input: Value, cx: &ToolCx) -> Result<ToolOutput> {
        let path = cx.resolve(str_arg(&input, "file_path")?);
        let size = tokio::fs::metadata(&path)
            .await
            .with_context(|| format!("reading {}", path.display()))?
            .len();
        let offset = input
            .get("offset")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1) as usize;
        let limit = input.get("limit").and_then(Value::as_u64).unwrap_or(2000) as usize;
        if size > WHOLE_READ_MAX {
            // Big file: stream the asked lines instead of loading it all.
            let p = path.clone();
            let out =
                tokio::task::spawn_blocking(move || numbered_lines(&p, offset, limit)).await??;
            cx.read_files.lock().unwrap().insert(path);
            return Ok(truncate_output(out, MAX_OUT).into());
        }
        let bytes = tokio::fs::read(&path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        if let Some(kind) = magia_types::sniff_image(&bytes) {
            return Ok(ToolOutput {
                text: format!(
                    "{} ({kind}, {} KB) is attached as an image.",
                    path.display(),
                    size.div_ceil(1024)
                ),
                images: vec![bytes],
            });
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| anyhow::anyhow!("{} is not valid UTF-8 text", path.display()))?;
        cx.read_files.lock().unwrap().insert(path);
        let mut out = String::new();
        for (i, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
            out.push_str(&format!("{:>6}\t{}\n", i + 1, line));
        }
        Ok(truncate_output(out, MAX_OUT).into())
    }
}

/// Files up to this size are read at once (and may be images); bigger ones are streamed.
const WHOLE_READ_MAX: u64 = 16 * 1024 * 1024;
/// `grep` skips files bigger than this (logs, dumps, data sets).
const GREP_FILE_MAX: u64 = 16 * 1024 * 1024;

/// Lines `offset..offset+limit` of a big text file, numbered, read line by line.
fn numbered_lines(path: &std::path::Path, offset: usize, limit: usize) -> Result<String> {
    use std::io::BufRead;
    let f = std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut out = String::new();
    for (i, line) in std::io::BufReader::new(f)
        .split(b'\n')
        .enumerate()
        .skip(offset - 1)
        .take(limit)
    {
        let line = line?;
        let line = String::from_utf8(line)
            .map_err(|_| anyhow::anyhow!("{} is not valid UTF-8 text", path.display()))?;
        out.push_str(&format!("{:>6}\t{}\n", i + 1, line.trim_end_matches('\r')));
        if out.len() > MAX_OUT {
            break;
        }
    }
    Ok(out)
}

// ───────────── write ─────────────
struct Write;
#[async_trait]
impl Tool for Write {
    fn name(&self) -> &'static str {
        "write"
    }
    fn description(&self) -> &'static str {
        "Create or overwrite a file. Overwriting an existing file requires reading it first."
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{
            "file_path":{"type":"string"},"content":{"type":"string"}},
            "required":["file_path","content"]})
    }
    fn read_only(&self) -> bool {
        false
    }
    fn is_edit(&self) -> bool {
        true
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> Result<String> {
        let path = cx.resolve(str_arg(&input, "file_path")?);
        let content = str_arg(&input, "content")?;
        if path.exists() && !cx.read_files.lock().unwrap().contains(&path) {
            bail!(
                "{} exists; read it first before overwriting",
                path.display()
            );
        }
        cx.checkpoints.snapshot(&path);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, content).await?;
        cx.read_files.lock().unwrap().insert(path.clone());
        Ok(format!(
            "wrote {} bytes to {}",
            content.len(),
            path.display()
        ))
    }
}

// ───────────── edit ─────────────
struct Edit;
#[async_trait]
impl Tool for Edit {
    fn name(&self) -> &'static str {
        "edit"
    }
    fn description(&self) -> &'static str {
        "Replace an exact string in a file you have already read. `old_string` must be unique unless replace_all is true."
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{
            "file_path":{"type":"string"},
            "old_string":{"type":"string"},
            "new_string":{"type":"string"},
            "replace_all":{"type":"boolean"}},
            "required":["file_path","old_string","new_string"]})
    }
    fn read_only(&self) -> bool {
        false
    }
    fn is_edit(&self) -> bool {
        true
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> Result<String> {
        let path = cx.resolve(str_arg(&input, "file_path")?);
        let old = str_arg(&input, "old_string")?;
        let new = str_arg(&input, "new_string")?;
        let all = input
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if old == new {
            bail!("old_string and new_string are identical");
        }
        if !cx.read_files.lock().unwrap().contains(&path) {
            bail!("read {} before editing it", path.display());
        }
        let text = tokio::fs::read_to_string(&path).await?;
        let n = text.matches(old).count();
        if n == 0 {
            bail!("old_string not found in {}", path.display());
        }
        if n > 1 && !all {
            bail!(
                "old_string matches {n} places; add context to make it unique or set replace_all"
            );
        }
        let updated = if all {
            text.replace(old, new)
        } else {
            text.replacen(old, new, 1)
        };
        cx.checkpoints.snapshot(&path); // so `/rewind` can undo it
        tokio::fs::write(&path, updated).await?;
        Ok(format!(
            "edited {} ({n} replacement{})",
            path.display(),
            if n == 1 { "" } else { "s" }
        ))
    }
}

// ───────────── bash ─────────────
struct Bash;
#[async_trait]
impl Tool for Bash {
    fn name(&self) -> &'static str {
        "bash"
    }
    fn description(&self) -> &'static str {
        "Run a shell command in the project directory. Output is truncated; default timeout 120s."
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{
            "command":{"type":"string"},
            "timeout_ms":{"type":"integer","description":"max 600000"}},
            "required":["command"]})
    }
    fn read_only(&self) -> bool {
        false
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> Result<String> {
        let cmd = str_arg(&input, "command")?;
        let ms = input
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(120_000)
            .min(600_000);
        let mut command = if cx.sandbox && which_bwrap() {
            let mut c = tokio::process::Command::new("bwrap");
            let cwd = cx.cwd.to_string_lossy().into_owned();
            c.args([
                "--ro-bind",
                "/",
                "/",
                "--bind",
                &cwd,
                &cwd,
                "--bind",
                "/tmp",
                "/tmp",
            ])
            .args([
                "--dev",
                "/dev",
                "--proc",
                "/proc",
                "--die-with-parent",
                "--chdir",
                &cwd,
            ])
            .args(["bash", "-c", cmd]);
            c
        } else {
            let mut c = tokio::process::Command::new(crate::bash_program());
            c.arg("-c").arg(cmd);
            c
        };
        command
            .current_dir(&cx.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Its own process group, so a timeout or cancel stops what it started too.
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().context("spawning bash")?;
        let (out_buf, err_buf) = (Shared::default(), Shared::default());
        let out_task = tokio::spawn(read_capped(child.stdout.take().unwrap(), out_buf.clone()));
        let err_task = tokio::spawn(read_capped(child.stderr.take().unwrap(), err_buf.clone()));
        let status = tokio::select! {
            s = child.wait() => s?,
            _ = tokio::time::sleep(Duration::from_millis(ms)) => {
                kill_group(&mut child).await;
                bail!("command timed out after {ms} ms");
            }
            _ = cx.cancel.cancelled() => {
                kill_group(&mut child).await;
                bail!("cancelled");
            }
        };
        // A program started in the background (`godot &`) keeps the pipes open after bash
        // exits; don't wait for it to close, take what was written so far.
        let (a, b) = tokio::join!(drain(out_task), drain(err_task));
        let background = a || b;
        let out = std::mem::take(&mut *out_buf.lock().unwrap());
        let err = std::mem::take(&mut *err_buf.lock().unwrap());
        let mut text = String::from_utf8_lossy(&out).into_owned();
        if !err.is_empty() {
            text.push_str("\n[stderr]\n");
            text.push_str(&String::from_utf8_lossy(&err));
        }
        let code = status
            .code()
            .map_or("signal".to_string(), |c| c.to_string());
        let note = if background {
            "\n[a program it started is still running in the background; its later output is not shown]"
        } else {
            ""
        };
        Ok(format!(
            "{}\n[exit {code}]{note}",
            truncate_output(text, MAX_OUT)
        ))
    }
}

/// How long to keep reading after bash exits, for output still in the pipe.
const DRAIN: Duration = Duration::from_millis(1500);

type Shared = std::sync::Arc<std::sync::Mutex<Vec<u8>>>;

/// Let the reader finish; `true` when the pipe was still open (a background program holds
/// it). Its bytes so far are in the shared buffer either way.
async fn drain(mut task: tokio::task::JoinHandle<()>) -> bool {
    if tokio::time::timeout(DRAIN, &mut task).await.is_ok() {
        return false;
    }
    task.abort();
    true
}

async fn kill_group(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // SIGKILL to the whole group (negative pid); `kill` is always there on Unix.
        let _ = tokio::process::Command::new("kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .status()
            .await;
    }
    let _ = child.kill().await;
}

fn which_bwrap() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("bwrap").is_file()))
}

/// Keep draining (so the child never blocks on a full pipe) but cap memory.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(mut r: R, buf: Shared) {
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut b = buf.lock().unwrap();
                if b.len() < 4 * MAX_OUT {
                    b.extend_from_slice(&chunk[..n]);
                }
            }
        }
    }
}

// ───────────── todo ─────────────
struct TodoTool;
#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &'static str {
        "todo"
    }
    fn description(&self) -> &'static str {
        "Replace the task list used to plan and track multi-step work. Send the whole list each time. Each item has content and status: pending, in_progress, completed (really done), or cancelled (decided not to do it; say why)."
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"todos":{"type":"array","items":{"type":"object","properties":{
            "content":{"type":"string"},
            "status":{"type":"string","enum":["pending","in_progress","completed","cancelled"]}},
            "required":["content","status"]}}},"required":["todos"]})
    }
    fn read_only(&self) -> bool {
        true
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> Result<String> {
        let todos: Vec<crate::Todo> =
            serde_json::from_value(input.get("todos").cloned().unwrap_or_default())
                .context("`todos` must be an array of {content, status}")?;
        let n = todos.len();
        *cx.todos.lock().unwrap() = todos;
        Ok(format!("task list updated ({n} items)"))
    }
}

// ───────────── ls ─────────────
struct Ls;
#[async_trait]
impl Tool for Ls {
    fn name(&self) -> &'static str {
        "ls"
    }
    fn description(&self) -> &'static str {
        "List a directory (non-recursive)."
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"}}})
    }
    fn read_only(&self) -> bool {
        true
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> Result<String> {
        let dir = cx.resolve(input.get("path").and_then(Value::as_str).unwrap_or("."));
        let mut rd = tokio::fs::read_dir(&dir)
            .await
            .with_context(|| format!("listing {}", dir.display()))?;
        let mut names = Vec::new();
        while let Some(e) = rd.next_entry().await? {
            let mut n = e.file_name().to_string_lossy().into_owned();
            if e.file_type().await.is_ok_and(|t| t.is_dir()) {
                n.push('/');
            }
            names.push(n);
        }
        names.sort();
        Ok(truncate_output(names.join("\n"), MAX_OUT))
    }
}

// ───────────── glob ─────────────
struct Glob;
#[async_trait]
impl Tool for Glob {
    fn name(&self) -> &'static str {
        "glob"
    }
    fn description(&self) -> &'static str {
        "Find files by glob pattern (e.g. `**/*.rs`), respecting .gitignore."
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{
            "pattern":{"type":"string"},"path":{"type":"string"}},"required":["pattern"]})
    }
    fn read_only(&self) -> bool {
        true
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> Result<String> {
        let pat = str_arg(&input, "pattern")?.to_owned();
        let root = cx.resolve(input.get("path").and_then(Value::as_str).unwrap_or("."));
        tokio::task::spawn_blocking(move || {
            let matcher = globset::Glob::new(&pat)?.compile_matcher();
            let mut hits = Vec::new();
            for e in ignore::WalkBuilder::new(&root).build().flatten() {
                if e.file_type().is_some_and(|t| t.is_file()) {
                    let rel = e.path().strip_prefix(&root).unwrap_or(e.path());
                    if matcher.is_match(rel) {
                        hits.push(crate::slashed(rel));
                    }
                }
            }
            hits.sort();
            Ok(truncate_output(hits.join("\n"), MAX_OUT))
        })
        .await?
    }
}

// ───────────── grep ─────────────
struct Grep;
#[async_trait]
impl Tool for Grep {
    fn name(&self) -> &'static str {
        "grep"
    }
    fn description(&self) -> &'static str {
        "Search file contents with a regex, respecting .gitignore. Returns `path:line:text`."
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{
            "pattern":{"type":"string"},
            "path":{"type":"string"},
            "glob":{"type":"string","description":"only files matching this glob"}},
            "required":["pattern"]})
    }
    fn read_only(&self) -> bool {
        true
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> Result<String> {
        let pat = str_arg(&input, "pattern")?.to_owned();
        let root = cx.resolve(input.get("path").and_then(Value::as_str).unwrap_or("."));
        let glob = input.get("glob").and_then(Value::as_str).map(str::to_owned);
        tokio::task::spawn_blocking(move || {
            let re = regex::Regex::new(&pat)?;
            let gm = glob
                .map(|g| globset::Glob::new(&g))
                .transpose()?
                .map(|g| g.compile_matcher());
            let mut out = String::new();
            'files: for e in ignore::WalkBuilder::new(&root).build().flatten() {
                if !e.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                let rel = e.path().strip_prefix(&root).unwrap_or(e.path());
                if gm.as_ref().is_some_and(|g| !g.is_match(rel)) {
                    continue;
                }
                if e.metadata().is_ok_and(|m| m.len() > GREP_FILE_MAX) {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(e.path()) else {
                    continue;
                };
                for (i, line) in text.lines().enumerate() {
                    if re.is_match(line) {
                        out.push_str(&format!(
                            "{}:{}:{}\n",
                            crate::slashed(rel),
                            i + 1,
                            line.chars().take(300).collect::<String>()
                        ));
                        if out.len() > MAX_OUT {
                            break 'files;
                        }
                    }
                }
            }
            Ok(truncate_output(out, MAX_OUT))
        })
        .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cx(dir: &tempfile::TempDir) -> ToolCx {
        ToolCx::new(dir.path().to_path_buf())
    }

    #[tokio::test]
    async fn edit_requires_read_and_unique_match() {
        let d = tempfile::tempdir().unwrap();
        let cx = cx(&d);
        std::fs::write(d.path().join("a.txt"), "foo bar foo").unwrap();
        let edit = json!({"file_path":"a.txt","old_string":"foo","new_string":"x"});
        assert!(
            Edit.call(edit.clone(), &cx).await.is_err(),
            "must read first"
        );
        Read.call(json!({"file_path":"a.txt"}), &cx).await.unwrap();
        assert!(
            Edit.call(edit, &cx)
                .await
                .unwrap_err()
                .to_string()
                .contains("2 places")
        );
        Edit.call(
            json!({"file_path":"a.txt","old_string":"bar","new_string":"baz"}),
            &cx,
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "foo baz foo"
        );
    }

    #[tokio::test]
    async fn bash_runs_and_times_out() {
        let d = tempfile::tempdir().unwrap();
        let cx = cx(&d);
        let out = Bash
            .call(json!({"command":"echo hi; echo err >&2; exit 3"}), &cx)
            .await
            .unwrap();
        assert!(out.contains("hi") && out.contains("err") && out.contains("[exit 3]"));
        let e = Bash
            .call(json!({"command":"sleep 5","timeout_ms":100}), &cx)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn bash_sandbox_blocks_writes_outside_cwd() {
        if !which_bwrap() {
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let mut cx = cx(&d);
        cx.sandbox = true;
        let probe = format!("{}/magia-sandbox-probe", std::env::var("HOME").unwrap());
        let out = Bash
            .call(
                json!({"command": format!("touch {probe} 2>&1; echo done > ok.txt; cat ok.txt")}),
                &cx,
            )
            .await
            .unwrap();
        // bwrap may be unusable in nested sandboxes; only assert when it actually ran.
        if out.contains("done") {
            assert!(
                !std::path::Path::new(&probe).exists(),
                "write outside cwd must fail"
            );
        }
    }

    #[tokio::test]
    async fn grep_and_glob() {
        let d = tempfile::tempdir().unwrap();
        let cx = cx(&d);
        std::fs::create_dir(d.path().join("src")).unwrap();
        std::fs::write(d.path().join("src/m.rs"), "fn main() {}\n").unwrap();
        let g = Grep.call(json!({"pattern":"fn \\w+"}), &cx).await.unwrap();
        assert!(g.contains("src/m.rs:1:"));
        let f = Glob.call(json!({"pattern":"**/*.rs"}), &cx).await.unwrap();
        assert_eq!(f.trim(), "src/m.rs");
    }

    #[tokio::test]
    async fn read_returns_images_as_images() {
        let dir = tempfile::tempdir().unwrap();
        let cx = ToolCx::new(dir.path().into());
        std::fs::write(dir.path().join("a.png"), b"\x89PNG\r\n\x1a\nrest").unwrap();
        std::fs::write(dir.path().join("t.txt"), "hi").unwrap();
        let out = Read
            .call_output(json!({"file_path": "a.png"}), &cx)
            .await
            .unwrap();
        assert_eq!(out.images.len(), 1);
        assert!(out.text.contains("image/png"));
        let out = Read
            .call_output(json!({"file_path": "t.txt"}), &cx)
            .await
            .unwrap();
        assert!(out.images.is_empty() && out.text.contains("hi"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_program_left_in_the_background_does_not_hold_the_call() {
        let dir = tempfile::tempdir().unwrap();
        let cx = ToolCx::new(dir.path().into());
        let start = std::time::Instant::now();
        let out = Bash
            .call(json!({"command": "echo started; sleep 30 &"}), &cx)
            .await
            .unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "{:?}",
            start.elapsed()
        );
        assert!(out.contains("started") && out.contains("[exit 0]"), "{out}");
        assert!(out.contains("still running in the background"), "{out}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_timeout_stops_what_the_command_started() {
        let dir = tempfile::tempdir().unwrap();
        let cx = ToolCx::new(dir.path().into());
        let marker = dir.path().join("alive");
        let cmd = format!("(sleep 2; touch {}) & sleep 30", marker.display());
        let err = Bash
            .call(json!({"command": cmd, "timeout_ms": 300}), &cx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"));
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            !marker.exists(),
            "the background child was killed with the group"
        );
    }

    #[tokio::test]
    async fn big_files_are_read_line_by_line() {
        let dir = tempfile::tempdir().unwrap();
        let cx = ToolCx::new(dir.path().into());
        let line = "x".repeat(99) + "\n";
        let big = line.repeat((WHOLE_READ_MAX as usize / line.len()) + 10);
        std::fs::write(dir.path().join("big.log"), &big).unwrap();
        let out = Read
            .call(
                json!({"file_path": "big.log", "offset": 3, "limit": 2}),
                &cx,
            )
            .await
            .unwrap();
        assert_eq!(out.lines().count(), 2);
        assert!(out.starts_with("     3\t"), "{out}");
    }
}
