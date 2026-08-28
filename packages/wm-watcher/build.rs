#[path = "../windows_version.rs"]
mod windows_version;

fn main() {
  if cfg!(not(target_os = "windows")) {
    panic!("wm-watcher is only supported on Windows.");
  }

  let mut res = tauri_winres::WindowsResource::new();

  res.set_icon("../../resources/assets/icon.ico");

  // Set language to English (US).
  res.set_language(0x0409);

  res.set("OriginalFilename", "glazewm-watcher.exe");
  res.set("ProductName", "GlazeWM Watcher");
  res.set("FileDescription", "GlazeWM Watcher");

  windows_version::apply_package_version(&mut res);

  res.compile().unwrap();
}
