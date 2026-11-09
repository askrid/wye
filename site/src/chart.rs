//! Bar charts, as tables of bars that CSS sizes, from tables in the
//! repository's Markdown: other gateways beside Wye, on a log scale (each
//! tenfold the same width, an axis under the bars), so 1× and 1,738× both fit
//! a column.

use crate::markdown;

/// Steps per tenfold: Wye's 1× is one step, so it shows as a short bar,
/// and each tenfold adds sixteen.
const DECADE_STEPS: f64 = 16.0;
/// The scale ends at 10,000×; a longer value is cut there and marked "»",
/// its number still printed in full.
const DECADES: f64 = 4.0;

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
        let gateway = name(first);
        if gateway == "Wye" {
            continue;
        }
        let label = cell.split(' ').next().unwrap_or(cell);
        let Some(value) = label.strip_suffix('×').and_then(number) else {
            continue;
        };
        bars.push(Bar {
            name: gateway,
            label: label.to_owned(),
            value,
            us: false,
        });
    }
    if bars.len() < 2 {
        return Err(format!("the {column:?} column holds no multiples"));
    }
    draw(bars)
}

/// A chart of one row of a table whose columns are gateways, Wye among them:
/// each gateway's first figure ("282 µs" of "282 µs (230-367)") as a multiple
/// of Wye's, units normalized. A cell without a figure ("not run") is left
/// out.
pub fn relative(text: &str, row_start: &str) -> Result<String, String> {
    let tables = markdown::tables(text);
    let (header, row) = tables
        .iter()
        .find_map(|rows| {
            let header = rows.first()?;
            let row = rows
                .iter()
                .skip(1)
                .find(|row| row.first().is_some_and(|cell| cell.starts_with(row_start)))?;
            Some((header, row))
        })
        .ok_or_else(|| format!("no table row starts with {row_start:?}"))?;
    let figures: Vec<(String, f64)> = header
        .iter()
        .zip(row)
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
        .map(|(column, value)| {
            let times = value / wye;
            let name = name(&column);
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
    draw(bars)
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

/// A row per bar, shortest first: the name, the value as printed, and the
/// bar as a share of the space left, on the log scale, so the chart fits
/// any width; then the axis, one label per tenfold. Screen readers read the
/// names and values; bars and axis are hidden from them. The axis row has a
/// header of its own, unseen, as every cell with text in a table needs one.
fn draw(mut bars: Vec<Bar>) -> Result<String, String> {
    // The scale starts at Wye's 1×: a gateway ahead of Wye would draw as a
    // sliver like Wye's, so it is an error, not a bar.
    if let Some(bar) = bars
        .iter()
        .find(|bar| bar.value.is_nan() || bar.value < 1.0)
    {
        return Err(format!(
            "{} is below Wye ({}); the scale starts at 1×",
            bar.name, bar.label
        ));
    }
    bars.sort_by(|a, b| a.value.total_cmp(&b.value));
    // As many tenfolds as the longest bar reaches, at least one.
    let top = bars.last().map_or(0.0, |bar| bar.value.log10());
    let decades = (top - 1e-9).ceil().clamp(1.0, DECADES);
    let span = 1.0 + DECADE_STEPS * decades;
    let share = |steps: f64| format!("{:.4}", (steps / span).clamp(0.0, 1.0));
    let mut out = String::from("<table class=\"bars\"><tbody>");
    for bar in &bars {
        let steps = 1.0 + DECADE_STEPS * bar.value.log10();
        out.push_str(&format!(
            "<tr{}><th scope=\"row\">{}</th><td class=\"v\">{}</td>\
             <td class=\"t\" aria-hidden=\"true\"><span{} style=\"--w:{}\"></span></td></tr>",
            if bar.us { " class=\"us\"" } else { "" },
            markdown::escape(&bar.name),
            markdown::escape(&bar.label),
            if steps > span { " class=\"cut\"" } else { "" },
            share(steps),
        ));
    }
    out.push_str(
        "</tbody><tfoot aria-hidden=\"true\"><tr class=\"axis\"><th scope=\"row\"><span class=\"visually-hidden\">Scale</span></th><td></td><td class=\"t\">",
    );
    for decade in 0..=decades as i32 {
        out.push_str(&format!(
            "<span style=\"--at:{}\">{}</span>",
            share(1.0 + DECADE_STEPS * f64::from(decade)),
            multiple(10f64.powi(decade))
        ));
    }
    out.push_str("</td></tr></tfoot></table>");
    Ok(out)
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

    const ROW: &str = "<th scope=\"row\">";

    #[test]
    fn draws_a_row_relative_to_wye() {
        let text = "| TLS | Wye | Bifrost | KrakenD | LiteLLM |\n|---|---|---|---|---|\n\
                    | Delay p99 | 282 µs (230-367) | 132 ms (94-145) | 1.0 ms (0.7-1.1) | not run |\n";
        let chart = relative(text, "Delay p99").unwrap();
        for row in [
            "Wye</th><td class=\"v\">1×",
            "KrakenD</th><td class=\"v\">3.5×",
            "Bifrost</th><td class=\"v\">468×",
        ] {
            assert!(chart.contains(&format!("{ROW}{row}")), "{chart}");
        }
        assert!(chart.contains("<tr class=\"us\">"), "{chart}");
        assert!(!chart.contains("LiteLLM"));
        assert!(relative(text, "Nothing").is_err());
    }

    #[test]
    fn prints_multiples_as_the_tables_do() {
        assert_eq!(multiple(1.0), "1×");
        assert_eq!(multiple(3.54), "3.5×");
        assert_eq!(multiple(1738.3), "1,738×");
    }

    #[test]
    fn refuses_a_gateway_ahead_of_wye() {
        let text = "| TLS | Wye | Fast |\n|---|---|---|\n| Delay | 282 µs | 10 µs |\n";
        assert!(
            relative(text, "Delay")
                .unwrap_err()
                .contains("Fast is below Wye")
        );
    }

    #[test]
    fn ends_the_axis_at_the_tenfold_reached_and_cuts_past_the_scale() {
        let text = "| TLS | Wye | Ten | Huge |\n|---|---|---|---|\n\
                    | Delay | 1 ms | 10 ms | 20 s |\n";
        let chart = relative(text, "Delay").unwrap();
        assert!(
            chart.contains("class=\"cut\" style=\"--w:1.0000\""),
            "{chart}"
        );
        assert!(
            chart.contains(">10,000×<") && !chart.contains(">100,000×<"),
            "{chart}"
        );
        let tens = "| TLS | Wye | Ten |\n|---|---|---|\n| Delay | 1 ms | 10 ms |\n";
        let chart = relative(tens, "Delay").unwrap();
        assert!(
            chart.contains("--w:1.0000") && chart.contains(">10×<") && !chart.contains(">100×<"),
            "{chart}"
        );
    }

    #[test]
    fn draws_multiples_with_wye_as_one() {
        let chart = ratios(RATIOS, "Added latency (p50)").unwrap();
        // Three tenfolds (170×), 49 steps: 1× is one step, 170× 36.7.
        for (name, label, width) in [
            ("Wye", "1×", "0.0204"),
            ("KrakenD", "1.4×", "0.0681"),
            ("agentgateway", "7×", "0.2964"),
            ("LiteLLM", "170×", "0.7487"),
        ] {
            let row = format!(
                "{ROW}{name}</th><td class=\"v\">{label}</td><td class=\"t\" aria-hidden=\"true\"><span style=\"--w:{width}\">"
            );
            assert!(chart.contains(&row), "{row}\n{chart}");
        }
        assert!(
            chart.contains("<span style=\"--at:1.0000\">1,000×</span>"),
            "{chart}"
        );
    }

    #[test]
    fn leaves_out_what_is_not_a_multiple() {
        let chart = ratios(RATIOS, "Memory").unwrap();
        assert!(chart.contains("320×") && !chart.contains("KrakenD"));
        assert!(ratios(RATIOS, "Nothing").is_err());
    }
}
