use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=ASHE_WORKER_REVISION");
    println!("cargo:rerun-if-changed=assets/ashe-worker.ico");
    let revision = std::env::var("ASHE_WORKER_REVISION")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(git_revision)
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=ASHE_WORKER_REVISION={revision}");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_windows_resources(&revision);
    }
}

/// Stamp the checked-out commit when the build does not name one, adding
/// `-dirty` when the worktree differs from it.
fn git_revision() -> Option<String> {
    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !head.status.success() {
        return None;
    }
    let revision = String::from_utf8(head.stdout).ok()?.trim().to_string();
    let dirty = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()
        .is_some_and(|status| !status.stdout.is_empty());
    Some(if dirty {
        format!("{revision}-dirty")
    } else {
        revision
    })
}

fn embed_windows_resources(revision: &str) {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let icon_path = manifest_dir.join("assets").join("ashe-worker.ico");
    let rc_path = out_dir.join("ashe-worker.rc");
    let short_revision: String = revision.chars().take(12).collect();
    let resource = format!(
        r#"1 ICON "{icon}"
1 VERSIONINFO
FILEVERSION 0,0,0,0
PRODUCTVERSION 0,0,0,0
FILETYPE 1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "FileDescription", "Ashe Worker"
      VALUE "FileVersion", "{short_revision}"
      VALUE "InternalName", "ashe-worker"
      VALUE "OriginalFilename", "ashe-worker.exe"
      VALUE "ProductName", "Ashe Worker"
      VALUE "ProductVersion", "{short_revision}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x0409, 1200
  END
END
"#,
        icon = icon_path.display().to_string().replace('\\', "\\\\"),
    );
    std::fs::write(&rc_path, resource).expect("write the Windows resource script");
    embed_resource::compile_for(&rc_path, ["ashe-worker"], embed_resource::NONE)
        .manifest_optional()
        .expect("compile the Windows resources");
}
