/// Parse a human size string like "500K", "1M", "4G", "1.5GB" into bytes.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let idx = s
        .find(|c: char| c.is_ascii_alphabetic())
        .unwrap_or(s.len());
    let (num_part, suffix) = s.split_at(idx);
    let num: f64 = num_part
        .trim()
        .parse()
        .map_err(|_| format!("invalid size: '{s}'"))?;
    let mult = match suffix.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1f64,
        "K" | "KB" | "KIB" => 1024f64,
        "M" | "MB" | "MIB" => 1024f64.powi(2),
        "G" | "GB" | "GIB" => 1024f64.powi(3),
        "T" | "TB" | "TIB" => 1024f64.powi(4),
        other => return Err(format!("unknown size suffix '{other}'")),
    };
    if num < 0.0 {
        return Err(format!("size cannot be negative: '{s}'"));
    }
    Ok((num * mult) as u64)
}

/// Format bytes as a human-readable string.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

/// Current UTC time as "YYYY-MM-DD HH:MM UTC" without pulling in a date crate.
pub fn now_utc_string() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m) = (rem / 3600, (rem % 3600) / 60);
    // civil_from_days (Howard Hinnant), epoch 1970-01-01.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{d:02} {h:02}:{m:02} UTC")
}

/// Format a count with thousands separators.
pub fn commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}
