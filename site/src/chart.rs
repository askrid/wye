//! Bar charts drawn in block characters, from tables in the repository's
//! Markdown: other gateways beside Wye, to scale up to a cut-off marked "»".

use crate::markdown;

/// The longest bar of a ratio chart, in cells; a longer value is cut there
/// and marked, its number still printed in full.
const RATIO_CELLS: usize = 20;
/// The narrowest column of printed values ("1,738×"), so charts stacked in
/// one section start their bars at the same place.
const LABEL_CELLS: usize = 6;
/// Eighths of a cell, for the end of a bar.
const PARTIAL: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];

struct Bar {
    name: String,
    label: String,
    value: f64,
    us: bool,
}

/// A chart of one column of a table whose cells are other gateways' figures
/// as multiples of Wye's ("7×"), with Wye added as 1×. A cell that is not a
/// multiple ("cannot stream") is left out.
pub fn ratios(text: &str, column: &str) -> Result<String, String> {
    let (rows, index) = markdown::tables(text)
        .into_iter()
        .find_map(|rows| {
            let index = rows.first()?.iter().position(|cell| cell == column)?;
            Some((rows, index))
        })
        .ok_or_else(|| format!("no table with a column {column:?}"))?;
    let mut bars = vec![Bar {
        name: "Wye".into(),
        label: "1×".into(),
        value: 1.0,
        us: true,
    }];
    for row in rows.iter().skip(1) {
        let (Some(first), Some(cell)) = (row.first(), row.get(index)) else {
            continue;
        };
        let label = cell.split(' ').next().unwrap_or(cell);
        let Some(value) = label.strip_suffix('×').and_then(number) else {
            continue;
        };
        bars.push(Bar {
            name: name(first),
            label: label.to_owned(),
            value,
            us: false,
        });
    }
    if bars.len() < 2 {
        return Err(format!("the {column:?} column holds no multiples"));
    }
    Ok(draw(bars, 1.0, RATIO_CELLS))
}

/// A chart of one row of a table whose columns are gateways, Wye among them:
/// each gateway's first figure ("282 µs" of "282 µs (230-367)") as a multiple
/// of Wye's, units normalized. A cell without a figure ("not run") is left
/// out.
pub fn relative(text: &str, row_start: &str) -> Result<String, String> {
    let (header, row) = markdown::tables(text)
        .into_iter()
        .find_map(|rows| {
            let header = rows.first()?.clone();
            let row = rows
                .iter()
                .skip(1)
                .find(|row| row.first().is_some_and(|cell| cell.starts_with(row_start)))?
                .clone();
            Some((header, row))
        })
        .ok_or_else(|| format!("no table row starts with {row_start:?}"))?;
    let figures: Vec<(String, f64)> = header
        .iter()
        .zip(&row)
        .skip(1)
        .filter_map(|(name, cell)| Some((name.clone(), measure(cell)?)))
        .collect();
    let wye = figures
        .iter()
        .find(|(name, _)| name == "Wye")
        .map(|(_, value)| *value)
        .filter(|value| *value > 0.0)
        .ok_or_else(|| format!("{row_start:?}: no figure for Wye"))?;
    let bars: Vec<Bar> = figures
        .into_iter()
        .map(|(name, value)| {
            let times = value / wye;
            Bar {
                us: name == "Wye",
                label: multiple(times),
                name,
                value: times,
            }
        })
        .collect();
    if bars.len() < 2 {
        return Err(format!("{row_start:?}: no figure beside Wye's"));
    }
    Ok(draw(bars, 1.0, RATIO_CELLS))
}

/// A cell's first figure in base units: "1.0 ms (0.7-1.1)" is 0.001.
fn measure(cell: &str) -> Option<f64> {
    let mut words = cell.split(' ');
    let value = number(words.next()?)?;
    let scale = match words.next()? {
        "ns" => 1e-9,
        "µs" => 1e-6,
        "ms" => 1e-3,
        "s" => 1.0,
        "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        _ => return None,
    };
    Some(value * scale)
}

/// A multiple as the comparison table prints one: "1×", "3.5×", "1,738×".
fn multiple(times: f64) -> String {
    if times < 9.95 {
        let text = format!("{times:.1}");
        return format!("{}×", text.strip_suffix(".0").unwrap_or(&text));
    }
    let whole = format!("{}", times.round() as u64);
    let mut grouped = String::new();
    for (i, digit) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    format!("{grouped}×")
}

/// One line per bar, shortest first: the name, the value as printed, and the
/// bar, `per_cell` to a cell. Bars are hidden from screen readers, which
/// read the names and values.
fn draw(mut bars: Vec<Bar>, per_cell: f64, max_cells: usize) -> String {
    bars.sort_by(|a, b| a.value.total_cmp(&b.value));
    let name_width = bars
        .iter()
        .map(|bar| bar.name.chars().count())
        .max()
        .unwrap_or(0);
    let label_width = bars
        .iter()
        .map(|bar| bar.label.chars().count())
        .max()
        .unwrap_or(0)
        .max(LABEL_CELLS);
    let lines: Vec<String> = bars
        .iter()
        .map(|bar| {
            let cells = bar.value / per_cell;
            let cut = cells > max_cells as f64;
            let cells = cells.min(max_cells as f64);
            let mut full = cells.floor() as usize;
            let mut eighths = ((cells - cells.floor()) * 8.0).round() as usize;
            if eighths == 8 {
                full += 1;
                eighths = 0;
            }
            if full == 0 && eighths == 0 {
                eighths = 1;
            }
            let mut line = format!(
                "{}{}  {}{}  <span class=\"fill\" aria-hidden=\"true\">{}{}{}</span>",
                markdown::escape(&bar.name),
                " ".repeat(name_width - bar.name.chars().count()),
                " ".repeat(label_width - bar.label.chars().count()),
                markdown::escape(&bar.label),
                "█".repeat(full),
                PARTIAL.get(eighths).unwrap_or(&""),
                if cut { "»" } else { "" },
            );
            if bar.us {
                line = format!("<span class=\"us\">{line}</span>");
            }
            line
        })
        .collect();
    // Focusable, so a keyboard can scroll a chart wider than the page.
    format!(
        "<pre class=\"bars\" tabindex=\"0\">{}</pre>",
        lines.join("\n")
    )
}

/// A gateway's name from its table cell: a link's text, or the cell up to
/// its first space.
fn name(cell: &str) -> String {
    match cell.strip_prefix('[').and_then(|rest| rest.split_once(']')) {
        Some((text, _)) => text.to_owned(),
        None => cell.split(' ').next().unwrap_or(cell).to_owned(),
    }
}

/// "1,500" or "1.4" as a number.
fn number(text: &str) -> Option<f64> {
    text.replace(',', "").parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATIOS: &str = "Own numbers:\n\n| What | A |\n|---|---|\n| x | 1 |\n\n\
        | Gateway | Added latency (p50) | Memory |\n|---|---|---|\n\
        | [agentgateway](https://x) 1.7 (Rust) | 7× | 2.9× |\n\
        | [LiteLLM](https://y) 1.105 (Python) | 170× | 320× (at idle) |\n\
        | [KrakenD](https://z) 3.0 CE (Go) | 1.4× | cannot stream |\n";

    #[test]
    fn draws_a_row_relative_to_wye() {
        let text = "| TLS | Wye | Bifrost | KrakenD | LiteLLM |\n|---|---|---|---|---|\n\
                    | Delay p99 | 282 µs (230-367) | 132 ms (94-145) | 1.0 ms (0.7-1.1) | not run |\n";
        let chart = relative(text, "Delay p99").unwrap();
        assert!(chart.contains("Wye          1×"), "{chart}");
        assert!(chart.contains("KrakenD    3.5×"), "{chart}");
        assert!(chart.contains("Bifrost    468×"), "{chart}");
        assert!(!chart.contains("LiteLLM"));
        assert!(relative(text, "Nothing").is_err());
        assert_eq!(multiple(1738.3), "1,738×");
    }

    #[test]
    fn draws_multiples_with_wye_as_one() {
        assert_eq!(
            ratios(RATIOS, "Added latency (p50)").unwrap(),
            "<pre class=\"bars\" tabindex=\"0\">\
             <span class=\"us\">Wye               1×  <span class=\"fill\" aria-hidden=\"true\">█</span></span>\n\
             KrakenD         1.4×  <span class=\"fill\" aria-hidden=\"true\">█▍</span>\n\
             agentgateway      7×  <span class=\"fill\" aria-hidden=\"true\">███████</span>\n\
             LiteLLM         170×  <span class=\"fill\" aria-hidden=\"true\">████████████████████»</span>\
             </pre>"
        );
    }

    #[test]
    fn leaves_out_what_is_not_a_multiple() {
        let chart = ratios(RATIOS, "Memory").unwrap();
        assert!(chart.contains("320×") && !chart.contains("KrakenD"));
        assert!(ratios(RATIOS, "Nothing").is_err());
    }
}
