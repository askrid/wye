//! Bar charts drawn in block characters, from tables in the repository's
//! Markdown: other gateways beside Wye, to scale up to a cut-off marked "»".

use crate::markdown;

/// The longest bar of a ratio chart, in cells; a longer value is cut there
/// and marked, its number still printed in full.
const RATIO_CELLS: usize = 20;
/// The longest bar of a two-way chart, in cells.
const VERSUS_CELLS: usize = 12;
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
    let (rows, index) = tables(text)
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
/// each cell's first figure ("1.2" of "1.2 / 3.1 ms", the p50 of a p50 / p99
/// pair) with the cell's unit.
pub fn versus(text: &str, row_start: &str) -> Result<String, String> {
    let (header, row) = tables(text)
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
    let mut bars = Vec::new();
    for (name, cell) in header.iter().zip(&row).skip(1) {
        let first = cell.split(" / ").next().unwrap_or(cell).trim();
        let figure = first.split(' ').next().unwrap_or(first);
        let unit = cell.rsplit(' ').next().unwrap_or_default();
        let value = number(figure)
            .ok_or_else(|| format!("{row_start:?}: {cell:?} does not start with a figure"))?;
        bars.push(Bar {
            name: name.clone(),
            label: format!("{figure} {unit}"),
            value,
            us: name == "Wye",
        });
    }
    let longest = bars.iter().map(|bar| bar.value).fold(0.0, f64::max);
    if bars.len() < 2 || !bars.iter().any(|bar| bar.us) || longest <= 0.0 {
        return Err(format!(
            "{row_start:?}: no figures for Wye and another gateway"
        ));
    }
    Ok(draw(bars, longest / VERSUS_CELLS as f64, VERSUS_CELLS))
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
        .unwrap_or(0);
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

/// Every Markdown table in `text`, each as rows of trimmed cells, header
/// first, the separator row left out.
fn tables(text: &str) -> Vec<Vec<Vec<String>>> {
    let mut tables = Vec::new();
    let mut rows: Vec<Vec<String>> = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(inner) = line.strip_prefix('|') {
            let cells: Vec<String> = inner
                .strip_suffix('|')
                .unwrap_or(inner)
                .split('|')
                .map(|cell| cell.trim().to_owned())
                .collect();
            let separator = cells
                .iter()
                .all(|cell| !cell.is_empty() && cell.chars().all(|c| matches!(c, '-' | ':')));
            if !separator {
                rows.push(cells);
            }
        } else if !rows.is_empty() {
            tables.push(std::mem::take(&mut rows));
        }
    }
    if !rows.is_empty() {
        tables.push(rows);
    }
    tables
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
    fn draws_multiples_with_wye_as_one() {
        assert_eq!(
            ratios(RATIOS, "Added latency (p50)").unwrap(),
            "<pre class=\"bars\" tabindex=\"0\">\
             <span class=\"us\">Wye             1×  <span class=\"fill\" aria-hidden=\"true\">█</span></span>\n\
             KrakenD       1.4×  <span class=\"fill\" aria-hidden=\"true\">█▍</span>\n\
             agentgateway    7×  <span class=\"fill\" aria-hidden=\"true\">███████</span>\n\
             LiteLLM       170×  <span class=\"fill\" aria-hidden=\"true\">████████████████████»</span>\
             </pre>"
        );
    }

    #[test]
    fn leaves_out_what_is_not_a_multiple() {
        let chart = ratios(RATIOS, "Memory").unwrap();
        assert!(chart.contains("320×") && !chart.contains("KrakenD"));
        assert!(ratios(RATIOS, "Nothing").is_err());
    }

    #[test]
    fn draws_one_row_against_another_gateway() {
        let text = "| Metric | Wye | agentgateway |\n|---|---|---|\n\
                    | Paced: event delay p50 / p99 | 1.2 / 3.1 ms | 1.0 / 3.6 ms |\n";
        assert_eq!(
            versus(text, "Paced: event delay").unwrap(),
            "<pre class=\"bars\" tabindex=\"0\">\
             agentgateway  1.0 ms  <span class=\"fill\" aria-hidden=\"true\">██████████</span>\n\
             <span class=\"us\">Wye           1.2 ms  <span class=\"fill\" aria-hidden=\"true\">████████████</span></span>\
             </pre>"
        );
        assert!(versus(text, "Other").is_err());
    }
}
