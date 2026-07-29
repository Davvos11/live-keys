use std::path::PathBuf;

/// Standard Linux LV2 install locations, used as a fallback when `LV2_PATH` isn't
/// already set in the environment. lilv's own compiled-in default varies slightly by
/// distro, but scanning a nonexistent directory is a harmless no-op, so this list
/// errs on the side of including more candidates.
const STANDARD_LV2_PATHS: &[&str] = &[
    "~/.lv2",
    "/usr/lib/lv2",
    "/usr/local/lib/lv2",
    "/usr/lib64/lv2",
    "/usr/local/lib64/lv2",
    "/usr/lib/x86_64-linux-gnu/lv2",
    "/usr/local/lib/x86_64-linux-gnu/lv2",
];

/// The LV2 plugin search paths in effect when no extra folders have been added: the
/// process's own `LV2_PATH` if already set (respects however the system/user already
/// configured it), otherwise [`STANDARD_LV2_PATHS`].
pub fn default_plugin_search_paths() -> Vec<String> {
    if let Ok(existing) = std::env::var("LV2_PATH") {
        existing.split(':').map(str::to_string).collect()
    } else {
        STANDARD_LV2_PATHS.iter().map(|p| expand_tilde(p)).collect()
    }
}

fn expand_tilde(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()
            .map(|home| home.join(rest).display().to_string())
            .unwrap_or_else(|| path.to_string()),
        None => path.to_string(),
    }
}

pub(crate) fn build_lv2_path(extra_dirs: &[PathBuf]) -> String {
    let mut paths = default_plugin_search_paths();
    paths.extend(extra_dirs.iter().map(|p| p.display().to_string()));
    paths.join(":")
}
