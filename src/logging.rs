use std::fs::OpenOptions;
use std::io::Write;

/// Write to debug log file if debug mode is enabled
pub fn debug(enabled: bool, msg: &str) {
    if enabled {
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open("gtui_debug.log")
        {
            let _ = writeln!(file, "{}", msg);
        }
    }
}
