//! Text for a terminal: lines that fit [`WIDTH`] columns, and counts,
//! sizes, and lists as people write them.

/// CLI text is wrapped to this width, which fits a split terminal.
pub const WIDTH: usize = 80;

/// `text` wrapped at word boundaries to [`WIDTH`] characters, every line
/// indented by `indent`; ends with a newline. Only ASCII whitespace breaks a
/// line, so a no-break space keeps two words together.
pub fn wrap(text: &str, indent: usize) -> String {
    let mut out = String::new();
    let mut line = 0;
    for word in text
        .split(|c: char| c.is_ascii_whitespace())
        .filter(|w| !w.is_empty())
    {
        let width = word.chars().count();
        if line > indent && line + 1 + width > WIDTH {
            out.push('\n');
            line = 0;
        }
        if line == 0 {
            out.push_str(&" ".repeat(indent));
            line = indent;
        } else {
            out.push(' ');
            line += 1;
        }
        out.push_str(word);
        line += width;
    }
    out.push('\n');
    out
}

/// `n` and the noun in the number it takes: "1 session", "2 sessions".
pub fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Model patterns as a config lists them.
pub fn model_list(patterns: &[gateway_core::policy::ModelPattern]) -> String {
    patterns
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A configured size as people write it: "32 MiB", "64 KiB", or "1000 bytes".
pub fn size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    match bytes {
        0 => "0 bytes".to_owned(),
        b if b % MIB == 0 => format!("{} MiB", b / MIB),
        b if b % KIB == 0 => format!("{} KiB", b / KIB),
        b => plural(b, "byte", "bytes"),
    }
}

/// A file's size as people read it: `912 KiB`, `4.2 MiB`.
pub fn file_size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.1} GiB", b as f64 / f64::from(1u32 << 30)),
        b if b >= 1 << 20 => format!("{:.1} MiB", b as f64 / f64::from(1u32 << 20)),
        b if b >= 1 << 10 => format!("{} KiB", b >> 10),
        b => format!("{b} bytes"),
    }
}
