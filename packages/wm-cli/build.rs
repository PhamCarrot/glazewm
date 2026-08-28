#[path = "../windows_version.rs"]
mod windows_version;

fn main() {
  let mut res = tauri_winres::WindowsResource::new();

  res.set_icon("../../resources/assets/icon.ico");

  // Set language to English (US).
  res.set_language(0x0409);

  res.set("OriginalFilename", "glazewm.exe");
  res.set("ProductName", "GlazeWM CLI");
  res.set("FileDescription", "GlazeWM CLI");

  windows_version::apply_package_version(&mut res);

  res.compile().unwrap();
}
