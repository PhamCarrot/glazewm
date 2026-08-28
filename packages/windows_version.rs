use tauri_winres::{VersionInfo, WindowsResource};

/// Applies the Cargo package version to the executable's Windows
/// resources.
///
/// Windows VERSIONINFO stores four unsigned 16-bit fields. Release builds
/// therefore require the workspace's authoritative Cargo version to be a
/// plain `major.minor.patch` triplet whose components fit that format.
pub fn apply_package_version(resource: &mut WindowsResource) {
  let package_version = env!("CARGO_PKG_VERSION");
  let parts = package_version
    .split('.')
    .map(|part| {
      part.parse::<u16>().unwrap_or_else(|_| {
        panic!(
          "Cargo package version `{package_version}` must be a plain major.minor.patch triplet with 16-bit numeric components"
        )
      })
    })
    .collect::<Vec<_>>();
  let [major, minor, patch] = <[u16; 3]>::try_from(parts)
    .unwrap_or_else(|_| {
      panic!(
        "Cargo package version `{package_version}` must contain exactly three numeric components"
      )
    });

  let windows_version = format!("{major}.{minor}.{patch}.0");
  resource.set("FileVersion", &windows_version);
  resource.set("ProductVersion", &windows_version);

  let packed_version = (u64::from(major) << 48)
    | (u64::from(minor) << 32)
    | (u64::from(patch) << 16);
  resource.set_version_info(VersionInfo::FILEVERSION, packed_version);
  resource.set_version_info(VersionInfo::PRODUCTVERSION, packed_version);
}
