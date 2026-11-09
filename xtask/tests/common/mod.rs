//! What the test binaries share.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Writes an executable that a test can run at once. A child process does the
/// writing. If this process held the file open for writing, a `fork` on another
/// thread could copy that descriptor, and until that child reached `exec`,
/// running the file would fail with `Text file busy`.
pub fn write_tool(path: &Path, content: &str) {
    let mut child = Command::new("sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    input.write_all(content.as_bytes()).unwrap();
    drop(input);
    assert!(child.wait().unwrap().success(), "{}", path.display());
}
