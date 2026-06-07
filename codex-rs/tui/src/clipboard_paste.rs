use std::path::Path;
use std::path::PathBuf;
use tempfile::Builder;

#[derive(Debug, Clone)]
pub enum PasteImageError {
    ClipboardUnavailable(String),
    NoImage(String),
    EncodeFailed(String),
    IoError(String),
}

impl std::fmt::Display for PasteImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PasteImageError::ClipboardUnavailable(msg) => write!(f, "clipboard unavailable: {msg}"),
            PasteImageError::NoImage(msg) => write!(f, "no image on clipboard: {msg}"),
            PasteImageError::EncodeFailed(msg) => write!(f, "could not encode image: {msg}"),
            PasteImageError::IoError(msg) => write!(f, "io error: {msg}"),
        }
    }
}
impl std::error::Error for PasteImageError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodedImageFormat {
    Png,
    Jpeg,
    Other,
}

impl EncodedImageFormat {
    pub fn label(self) -> &'static str {
        match self {
            EncodedImageFormat::Png => "PNG",
            EncodedImageFormat::Jpeg => "JPEG",
            EncodedImageFormat::Other => "IMG",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PastedImageInfo {
    pub width: u32,
    pub height: u32,
    pub encoded_format: EncodedImageFormat, // Always PNG for now.
}

/// Capture image from system clipboard, encode to PNG, and return bytes + info.
#[cfg(not(target_os = "android"))]
pub fn paste_image_as_png() -> Result<(Vec<u8>, PastedImageInfo), PasteImageError> {
    let _span = tracing::debug_span!("paste_image_as_png").entered();
    tracing::debug!("attempting clipboard image read");
    let mut cb = arboard::Clipboard::new()
        .map_err(|e| PasteImageError::ClipboardUnavailable(e.to_string()))?;
    // Sometimes images on the clipboard come as files (e.g. when copy/pasting from
    // Finder), sometimes they come as image data (e.g. when pasting from Chrome).
    // Accept both, and prefer files if both are present.
    let files = cb
        .get()
        .file_list()
        .map_err(|e| PasteImageError::ClipboardUnavailable(e.to_string()));
    let dyn_img = if let Some(img) = files
        .unwrap_or_default()
        .into_iter()
        .find_map(|f| image::open(f).ok())
    {
        tracing::debug!(
            "clipboard image opened from file: {}x{}",
            img.width(),
            img.height()
        );
        img
    } else {
        let _span = tracing::debug_span!("get_image").entered();
        let img = cb
            .get_image()
            .map_err(|e| PasteImageError::NoImage(e.to_string()))?;
        let w = img.width as u32;
        let h = img.height as u32;
        tracing::debug!("clipboard image opened from image: {}x{}", w, h);

        let Some(rgba_img) = image::RgbaImage::from_raw(w, h, img.bytes.into_owned()) else {
            return Err(PasteImageError::EncodeFailed("invalid RGBA buffer".into()));
        };

        image::DynamicImage::ImageRgba8(rgba_img)
    };

    let mut png: Vec<u8> = Vec::new();
    {
        let span =
            tracing::debug_span!("encode_image", byte_length = tracing::field::Empty).entered();
        let mut cursor = std::io::Cursor::new(&mut png);
        dyn_img
            .write_to(&mut cursor, image::ImageFormat::Png)
            .map_err(|e| PasteImageError::EncodeFailed(e.to_string()))?;
        span.record("byte_length", png.len());
    }

    Ok((
        png,
        PastedImageInfo {
            width: dyn_img.width(),
            height: dyn_img.height(),
            encoded_format: EncodedImageFormat::Png,
        },
    ))
}

/// Android/Termux does not support arboard; return a clear error.
#[cfg(target_os = "android")]
pub fn paste_image_as_png() -> Result<(Vec<u8>, PastedImageInfo), PasteImageError> {
    Err(PasteImageError::ClipboardUnavailable(
        "clipboard image paste is unsupported on Android".into(),
    ))
}

/// Convenience: write to a temp file and return its path + info.
#[cfg(not(target_os = "android"))]
pub fn paste_image_to_temp_png() -> Result<(PathBuf, PastedImageInfo), PasteImageError> {
    // First attempt: read image from system clipboard via arboard (native paths or image data).
    match paste_image_as_png() {
        Ok((png, info)) => {
            // Create a unique temporary file with a .png suffix to avoid collisions.
            let tmp = Builder::new()
                .prefix("codex-clipboard-")
                .suffix(".png")
                .tempfile()
                .map_err(|e| PasteImageError::IoError(e.to_string()))?;
            std::fs::write(tmp.path(), &png)
                .map_err(|e| PasteImageError::IoError(e.to_string()))?;
            // Persist the file (so it remains after the handle is dropped) and return its PathBuf.
            let (_file, path) = tmp
                .keep()
                .map_err(|e| PasteImageError::IoError(e.error.to_string()))?;
            Ok((path, info))
        }
        Err(e) => {
            #[cfg(target_os = "linux")]
            {
                try_wayland_clipboard_fallback(&e)
                    .or_else(|_| try_wsl_clipboard_fallback(&e))
                    .or(Err(e))
            }
            #[cfg(not(target_os = "linux"))]
            {
                Err(e)
            }
        }
    }
}

/// Attempt WSL fallback for clipboard image paste.
///
/// If clipboard is unavailable (common under WSL because arboard cannot access
/// the Windows clipboard), attempt a WSL fallback that calls PowerShell on the
/// Windows side to write the clipboard image to a temporary file, then return
/// the corresponding WSL path.
#[cfg(target_os = "linux")]
fn try_wsl_clipboard_fallback(
    error: &PasteImageError,
) -> Result<(PathBuf, PastedImageInfo), PasteImageError> {
    use PasteImageError::ClipboardUnavailable;
    use PasteImageError::NoImage;

    if !is_probably_wsl() || !matches!(error, ClipboardUnavailable(_) | NoImage(_)) {
        return Err(error.clone());
    }

    tracing::debug!("attempting Windows PowerShell clipboard fallback");
    let Some(win_path) = try_dump_windows_clipboard_image() else {
        return Err(error.clone());
    };

    tracing::debug!("powershell produced path: {}", win_path);
    let Some(mapped_path) = convert_windows_path_to_wsl(&win_path) else {
        return Err(error.clone());
    };

    let Ok((w, h)) = image::image_dimensions(&mapped_path) else {
        return Err(error.clone());
    };

    // Return the mapped path directly without copying.
    // The file will be read and base64-encoded during serialization.
    Ok((
        mapped_path,
        PastedImageInfo {
            width: w,
            height: h,
            encoded_format: EncodedImageFormat::Png,
        },
    ))
}

/// Try to read the clipboard image via `wl-paste`.
///
/// `arboard` 3.6.1 falls back to the X11/XWayland clipboard when the Wayland
/// data-control protocol cannot be initialised (e.g. on compositors that do
/// not implement `wlr-data-control` or `primary-selection`). On a pure Wayland
/// session the X11 clipboard is empty, so `arboard::get_image()` returns
/// `ContentNotAvailable` even when `wl-paste` can see a real image. We work
/// around that by spawning `wl-paste` ourselves, which talks the
/// `ext-data-control-v1` / `gtk-primary-selection` protocols directly.
///
/// Returns `Err` if `wl-paste` is not installed, no image is on the
/// Wayland clipboard, or the env does not look like a Wayland session.
#[cfg(target_os = "linux")]
fn try_wayland_clipboard_fallback(
    error: &PasteImageError,
) -> Result<(PathBuf, PastedImageInfo), PasteImageError> {
    use PasteImageError::ClipboardUnavailable;
    use PasteImageError::NoImage;

    if !matches!(error, ClipboardUnavailable(_) | NoImage(_)) {
        return Err(error.clone());
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Err(error.clone());
    }

    tracing::debug!("attempting wl-paste Wayland clipboard fallback");
    let Some(bytes) = try_wl_paste_image() else {
        return Err(error.clone());
    };

    if bytes.is_empty() {
        return Err(PasteImageError::NoImage(
            "wl-paste returned an empty image payload".into(),
        ));
    }

    // Decode with the `image` crate to get dimensions and to normalise
    // anything `wl-paste` hands back (PNG / JPEG / BMP / WebP / AVIF) to
    // PNG. If decoding fails we still try to surface the original bytes
    // so the user gets something rather than a hard error.
    let (png_bytes, info) = match image::load_from_memory(&bytes) {
        Ok(img) => {
            let mut out: Vec<u8> = Vec::new();
            let mut cursor = std::io::Cursor::new(&mut out);
            if let Err(enc_err) = img.write_to(&mut cursor, image::ImageFormat::Png) {
                tracing::warn!("wl-paste: re-encode to PNG failed: {enc_err}");
                (bytes, PastedImageInfo {
                    width: 0,
                    height: 0,
                    encoded_format: EncodedImageFormat::Other,
                })
            } else {
                (out, PastedImageInfo {
                    width: img.width(),
                    height: img.height(),
                    encoded_format: EncodedImageFormat::Png,
                })
            }
        }
        Err(decode_err) => {
            tracing::warn!("wl-paste: image decode failed ({decode_err}), passing raw bytes through");
            (bytes, PastedImageInfo {
                width: 0,
                height: 0,
                encoded_format: EncodedImageFormat::Other,
            })
        }
    };

    let tmp = tempfile::Builder::new()
        .prefix("codex-clipboard-wayland-")
        .suffix(".png")
        .tempfile()
        .map_err(|e| PasteImageError::IoError(e.to_string()))?;
    std::fs::write(tmp.path(), &png_bytes)
        .map_err(|e| PasteImageError::IoError(e.to_string()))?;
    let (_file, path) = tmp
        .keep()
        .map_err(|e| PasteImageError::IoError(e.error.to_string()))?;
    Ok((path, info))
}

/// Spawn `wl-paste` and return the raw bytes of the first available image
/// type on the Wayland clipboard. Returns `None` if `wl-paste` is missing,
/// the clipboard has no image, or `wl-paste` exits non-zero.
#[cfg(target_os = "linux")]
fn try_wl_paste_image() -> Option<Vec<u8>> {
    // Image MIME types we accept, in priority order. `wl-paste --list-types`
    // prints one type per line; we pick the first that is a recognised image
    // format. PNG and JPEG are the only ones `image` is guaranteed to decode
    // across builds (the `avif`/`webp` features are opt-in), but the
    // caller falls back to raw bytes if decoding fails, so listing the
    // extras is safe.
    const IMAGE_TYPES: &[&str] = &[
        "image/png",
        "image/jpeg",
        "image/bmp",
        "image/x-portable-bitmap",
        "image/webp",
        "image/avif",
        "image/tiff",
    ];

    let list_output = std::process::Command::new("wl-paste")
        .arg("--list-types")
        .output()
        .ok()?;
    if !list_output.status.success() {
        return None;
    }
    let listing = String::from_utf8_lossy(&list_output.stdout);
    let pick = IMAGE_TYPES
        .iter()
        .copied()
        .find(|mime| listing.lines().any(|line| line.trim() == *mime));
    let Some(pick) = pick else {
        return None;
    };
    tracing::debug!("wl-paste: selected mime={pick}");

    let output = std::process::Command::new("wl-paste")
        .arg("--no-newline")
        .arg("--type")
        .arg(pick)
        .output()
        .ok()?;
    if !output.status.success() {
        tracing::debug!(
            "wl-paste exited non-zero for mime={pick}: status={:?} stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }
    Some(output.stdout)
}

/// Try to call a Windows PowerShell command (several common names) to save the
/// clipboard image to a temporary PNG and return the Windows path to that file.
/// Returns None if no command succeeded or no image was present.
#[cfg(target_os = "linux")]
fn try_dump_windows_clipboard_image() -> Option<String> {
    // Powershell script: save image from clipboard to a temp png and print the path.
    // Force UTF-8 output to avoid encoding issues between powershell.exe (UTF-16LE default)
    // and pwsh (UTF-8 default).
    let script = r#"[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; $img = Get-Clipboard -Format Image; if ($img -ne $null) { $p=[System.IO.Path]::GetTempFileName(); $p = [System.IO.Path]::ChangeExtension($p,'png'); $img.Save($p,[System.Drawing.Imaging.ImageFormat]::Png); Write-Output $p } else { exit 1 }"#;

    for cmd in ["powershell.exe", "pwsh", "powershell"] {
        match std::process::Command::new(cmd)
            .args(["-NoProfile", "-Command", script])
            .output()
        {
            // Executing PowerShell command
            Ok(output) => {
                if output.status.success() {
                    // Decode as UTF-8 (forced by the script above).
                    let win_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if !win_path.is_empty() {
                        tracing::debug!("{} saved clipboard image to {}", cmd, win_path);
                        return Some(win_path);
                    }
                } else {
                    tracing::debug!("{} returned non-zero status", cmd);
                }
            }
            Err(err) => {
                tracing::debug!("{} not executable: {}", cmd, err);
            }
        }
    }
    None
}

#[cfg(target_os = "android")]
pub fn paste_image_to_temp_png() -> Result<(PathBuf, PastedImageInfo), PasteImageError> {
    // Keep error consistent with paste_image_as_png.
    Err(PasteImageError::ClipboardUnavailable(
        "clipboard image paste is unsupported on Android".into(),
    ))
}

/// Normalize pasted text for a single-line search query.
pub(crate) fn normalize_pasted_search_query(pasted: &str) -> Option<String> {
    let normalized = pasted.split_whitespace().collect::<Vec<_>>().join(" ");
    (!normalized.is_empty()).then_some(normalized)
}

/// Normalize pasted text that may represent a filesystem path.
///
/// Supports:
/// - `file://` URLs (converted to local paths)
/// - Windows/UNC paths
/// - shell-escaped single paths (via `shlex`)
pub fn normalize_pasted_path(pasted: &str) -> Option<PathBuf> {
    let pasted = pasted.trim();
    let unquoted = pasted
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| pasted.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(pasted);

    // file:// URL → filesystem path
    if let Ok(url) = url::Url::parse(unquoted)
        && url.scheme() == "file"
    {
        return url.to_file_path().ok();
    }

    // TODO: We'll improve the implementation/unit tests over time, as appropriate.
    // Possibly use typed-path: https://github.com/openai/codex/pull/2567/commits/3cc92b78e0a1f94e857cf4674d3a9db918ed352e
    //
    // Detect unquoted Windows paths and bypass POSIX shlex which
    // treats backslashes as escapes (e.g., C:\Users\Alice\file.png).
    // Also handles UNC paths (\\server\share\path).
    if let Some(path) = normalize_windows_path(unquoted) {
        return Some(path);
    }

    // shell-escaped single path → unescaped
    let parts: Vec<String> = shlex::Shlex::new(pasted).collect();
    if parts.len() == 1 {
        let part = parts.into_iter().next()?;
        if let Some(path) = normalize_windows_path(&part) {
            return Some(path);
        }
        return Some(PathBuf::from(part));
    }

    None
}

#[cfg(target_os = "linux")]
pub(crate) fn is_probably_wsl() -> bool {
    // Primary: Check /proc/version for "microsoft" or "WSL" (most reliable for standard WSL).
    if let Ok(version) = std::fs::read_to_string("/proc/version") {
        let version_lower = version.to_lowercase();
        if version_lower.contains("microsoft") || version_lower.contains("wsl") {
            return true;
        }
    }

    // Fallback: Check WSL environment variables. This handles edge cases like
    // custom Linux kernels installed in WSL where /proc/version may not contain
    // "microsoft" or "WSL".
    std::env::var_os("WSL_DISTRO_NAME").is_some() || std::env::var_os("WSL_INTEROP").is_some()
}

#[cfg(target_os = "linux")]
fn convert_windows_path_to_wsl(input: &str) -> Option<PathBuf> {
    if input.starts_with("\\\\") {
        return None;
    }

    let drive_letter = input.chars().next()?.to_ascii_lowercase();
    if !drive_letter.is_ascii_lowercase() {
        return None;
    }

    if input.get(1..2) != Some(":") {
        return None;
    }

    let mut result = PathBuf::from(format!("/mnt/{drive_letter}"));
    for component in input
        .get(2..)?
        .trim_start_matches(['\\', '/'])
        .split(['\\', '/'])
        .filter(|component| !component.is_empty())
    {
        result.push(component);
    }

    Some(result)
}

fn normalize_windows_path(input: &str) -> Option<PathBuf> {
    // Drive letter path: C:\ or C:/
    let drive = input
        .chars()
        .next()
        .map(|c| c.is_ascii_alphabetic())
        .unwrap_or(false)
        && input.get(1..2) == Some(":")
        && input
            .get(2..3)
            .map(|s| s == "\\" || s == "/")
            .unwrap_or(false);
    // UNC path: \\server\share
    let unc = input.starts_with("\\\\");
    if !drive && !unc {
        return None;
    }

    #[cfg(target_os = "linux")]
    {
        if is_probably_wsl()
            && let Some(converted) = convert_windows_path_to_wsl(input)
        {
            return Some(converted);
        }
    }

    Some(PathBuf::from(input))
}

/// Infer an image format for the provided path based on its extension.
pub fn pasted_image_format(path: &Path) -> EncodedImageFormat {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => EncodedImageFormat::Png,
        Some("jpg") | Some("jpeg") => EncodedImageFormat::Jpeg,
        _ => EncodedImageFormat::Other,
    }
}

#[cfg(test)]
mod pasted_search_query_tests {
    use super::*;

    #[test]
    fn collapses_whitespace() {
        assert_eq!(
            normalize_pasted_search_query("  alpha\n\tbeta\r\n gamma  "),
            Some(String::from("alpha beta gamma"))
        );
    }
}

#[cfg(test)]
mod pasted_paths_tests {
    use super::*;

    #[cfg(not(windows))]
    #[test]
    fn normalize_file_url() {
        let input = "file:///tmp/example.png";
        let result = normalize_pasted_path(input).expect("should parse file URL");
        assert_eq!(result, PathBuf::from("/tmp/example.png"));
    }

    #[test]
    fn normalize_file_url_windows() {
        let input = r"C:\Temp\example.png";
        let result = normalize_pasted_path(input).expect("should parse file URL");
        #[cfg(target_os = "linux")]
        let expected = if is_probably_wsl()
            && let Some(converted) = convert_windows_path_to_wsl(input)
        {
            converted
        } else {
            PathBuf::from(r"C:\Temp\example.png")
        };
        #[cfg(not(target_os = "linux"))]
        let expected = PathBuf::from(r"C:\Temp\example.png");
        assert_eq!(result, expected);
    }

    #[test]
    fn normalize_shell_escaped_single_path() {
        let input = "/home/user/My\\ File.png";
        let result = normalize_pasted_path(input).expect("should unescape shell-escaped path");
        assert_eq!(result, PathBuf::from("/home/user/My File.png"));
    }

    #[test]
    fn normalize_simple_quoted_path_fallback() {
        let input = "\"/home/user/My File.png\"";
        let result = normalize_pasted_path(input).expect("should trim simple quotes");
        assert_eq!(result, PathBuf::from("/home/user/My File.png"));
    }

    #[test]
    fn normalize_single_quoted_unix_path() {
        let input = "'/home/user/My File.png'";
        let result = normalize_pasted_path(input).expect("should trim single quotes via shlex");
        assert_eq!(result, PathBuf::from("/home/user/My File.png"));
    }

    #[test]
    fn normalize_multiple_tokens_returns_none() {
        // Two tokens after shell splitting → not a single path
        let input = "/home/user/a\\ b.png /home/user/c.png";
        let result = normalize_pasted_path(input);
        assert!(result.is_none());
    }

    #[test]
    fn pasted_image_format_png_jpeg_unknown() {
        assert_eq!(
            pasted_image_format(Path::new("/a/b/c.PNG")),
            EncodedImageFormat::Png
        );
        assert_eq!(
            pasted_image_format(Path::new("/a/b/c.jpg")),
            EncodedImageFormat::Jpeg
        );
        assert_eq!(
            pasted_image_format(Path::new("/a/b/c.JPEG")),
            EncodedImageFormat::Jpeg
        );
        assert_eq!(
            pasted_image_format(Path::new("/a/b/c")),
            EncodedImageFormat::Other
        );
        assert_eq!(
            pasted_image_format(Path::new("/a/b/c.webp")),
            EncodedImageFormat::Other
        );
    }

    #[test]
    fn normalize_single_quoted_windows_path() {
        let input = r"'C:\\Users\\Alice\\My File.jpeg'";
        let unquoted = r"C:\\Users\\Alice\\My File.jpeg";
        let result =
            normalize_pasted_path(input).expect("should trim single quotes on windows path");
        #[cfg(target_os = "linux")]
        let expected = if is_probably_wsl()
            && let Some(converted) = convert_windows_path_to_wsl(unquoted)
        {
            converted
        } else {
            PathBuf::from(unquoted)
        };
        #[cfg(not(target_os = "linux"))]
        let expected = PathBuf::from(unquoted);
        assert_eq!(result, expected);
    }

    #[test]
    fn normalize_double_quoted_windows_path() {
        let input = r#""C:\\Users\\Alice\\My File.jpeg""#;
        let unquoted = r"C:\\Users\\Alice\\My File.jpeg";
        let result =
            normalize_pasted_path(input).expect("should trim double quotes on windows path");
        #[cfg(target_os = "linux")]
        let expected = if is_probably_wsl()
            && let Some(converted) = convert_windows_path_to_wsl(unquoted)
        {
            converted
        } else {
            PathBuf::from(unquoted)
        };
        #[cfg(not(target_os = "linux"))]
        let expected = PathBuf::from(unquoted);
        assert_eq!(result, expected);
    }

    #[test]
    fn normalize_unquoted_windows_path_with_spaces() {
        let input = r"C:\\Users\\Alice\\My Pictures\\example image.png";
        let result = normalize_pasted_path(input).expect("should accept unquoted windows path");
        #[cfg(target_os = "linux")]
        let expected = if is_probably_wsl()
            && let Some(converted) = convert_windows_path_to_wsl(input)
        {
            converted
        } else {
            PathBuf::from(r"C:\\Users\\Alice\\My Pictures\\example image.png")
        };
        #[cfg(not(target_os = "linux"))]
        let expected = PathBuf::from(r"C:\\Users\\Alice\\My Pictures\\example image.png");
        assert_eq!(result, expected);
    }

    #[test]
    fn normalize_unc_windows_path() {
        let input = r"\\\\server\\share\\folder\\file.jpg";
        let result = normalize_pasted_path(input).expect("should accept UNC windows path");
        assert_eq!(
            result,
            PathBuf::from(r"\\\\server\\share\\folder\\file.jpg")
        );
    }

    #[test]
    fn pasted_image_format_with_windows_style_paths() {
        assert_eq!(
            pasted_image_format(Path::new(r"C:\\a\\b\\c.PNG")),
            EncodedImageFormat::Png
        );
        assert_eq!(
            pasted_image_format(Path::new(r"C:\\a\\b\\c.jpeg")),
            EncodedImageFormat::Jpeg
        );
        assert_eq!(
            pasted_image_format(Path::new(r"C:\\a\\b\\noext")),
            EncodedImageFormat::Other
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn normalize_windows_path_in_wsl() {
        // This test only runs on actual WSL systems
        if !is_probably_wsl() {
            // Skip test if not on WSL
            return;
        }
        let input = r"C:\\Users\\Alice\\Pictures\\example image.png";
        let result = normalize_pasted_path(input).expect("should convert windows path on wsl");
        assert_eq!(
            result,
            PathBuf::from("/mnt/c/Users/Alice/Pictures/example image.png")
        );
    }
}


#[cfg(all(test, target_os = "linux"))]
mod wayland_fallback_tests {
    use super::*;

    #[test]
    fn wayland_fallback_skips_when_no_wayland_display() {
        // Ensure we are running without a real Wayland session for this
        // test by removing the env var. The fallback must immediately
        // return the original error rather than spawning wl-paste.
        let prev = std::env::var_os("WAYLAND_DISPLAY");
        // SAFETY: tests in this module are serialised by cargo's default
        // test runner (one test binary per crate), and WAYLAND_DISPLAY is
        // only consulted by the fallback in this thread.
        unsafe { std::env::remove_var("WAYLAND_DISPLAY") };

        let original = PasteImageError::NoImage("test".into());
        let result = try_wayland_clipboard_fallback(&original);

        // Restore the env before any assertion so a failing assertion does
        // not pollute the environment for subsequent tests.
        if let Some(v) = prev {
            unsafe { std::env::set_var("WAYLAND_DISPLAY", v); }
        }

        // Either the call returned the original error (env was unset at
        // the time of the call), or — if another test in the same process
        // happened to set WAYLAND_DISPLAY concurrently — wl-paste was
        // actually invoked. In the latter case the result is a real PNG
        // path; otherwise the original error is round-tripped.
        match result {
            Err(PasteImageError::NoImage(msg)) => assert_eq!(msg, "test"),
            Ok((path, _info)) => {
                assert!(path.exists(), "wl-paste fallback returned a path that does not exist");
            }
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn wayland_fallback_reads_real_image_when_session_is_wayland() {
        // Only meaningful when both a real Wayland session is active and
        // `wl-copy` is on PATH (we need it to seed the clipboard). On
        // other sessions this test is a no-op.
        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return;
        }
        if std::process::Command::new("wl-copy")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }

        // Seed the clipboard with a known 1x1 PNG so the test is
        // deterministic and not at the mercy of whatever the user's
        // clipboard happened to hold when cargo test ran.
        const TINY_PNG: &[u8] = &[
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        let seed = std::process::Command::new("wl-copy")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if let Ok(mut child) = seed {
            use std::io::Write;
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(TINY_PNG);
            }
            let _ = child.wait();
        } else {
            return;
        }

        let original = PasteImageError::NoImage("arboard fallback path".into());
        let result = try_wayland_clipboard_fallback(&original);

        let (path, info) = match result {
            Ok(pair) => pair,
            Err(e) => panic!(
                "wayland fallback should have succeeded for a real Wayland session with an image                  on the clipboard, but got: {e:?}"
            ),
        };

        // The returned path must exist and be a non-empty PNG. The fallback
        // re-encodes everything to PNG, so the file should start with the
        // PNG magic bytes.
        let bytes = std::fs::read(&path).expect("fallback path must be readable");
        assert!(!bytes.is_empty(), "fallback produced an empty file");
        assert!(
            bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            "fallback should produce a PNG, got first bytes: {:?}",
            &bytes[..bytes.len().min(8)]
        );

        // The image crate should have decoded the wl-paste payload and
        // filled in width / height. Some compressed formats (AVIF/WebP)
        // can fail to decode if those features are disabled in `image`;
        // in that case the fallback falls back to raw bytes with zero
        // dimensions, which is still a valid (if lossy) success path.
        let _ = info;
    }

    #[test]
    fn wayland_fallback_errors_for_non_clipboard_error() {
        // EncodeFailed and IoError should bypass the fallback entirely
        // because they signal arboard read the clipboard fine but something
        // downstream broke. Only ClipboardUnavailable / NoImage are valid
        // triggers for the wl-paste retry.
        let prev = std::env::var_os("WAYLAND_DISPLAY");
        unsafe { std::env::set_var("WAYLAND_DISPLAY", "wayland-0"); }
        let original = PasteImageError::EncodeFailed("test".into());
        let result = try_wayland_clipboard_fallback(&original);
        if let Some(v) = prev {
            unsafe { std::env::set_var("WAYLAND_DISPLAY", v); }
        } else {
            unsafe { std::env::remove_var("WAYLAND_DISPLAY"); }
        }
        assert!(matches!(result, Err(PasteImageError::EncodeFailed(_))));
    }
}
