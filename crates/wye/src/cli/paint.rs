//! Color on a terminal, only where it carries meaning: an error, a check's
//! verdict, a report's section titles. Off when the output is not a
//! terminal, `NO_COLOR` is set (no-color.org), or `TERM` is `dumb`.

use std::io::IsTerminal;

/// Whether to color one output stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Paint(bool);

impl Paint {
    pub const OFF: Self = Self(false);

    /// For standard output.
    pub fn stdout() -> Self {
        Self::detect(std::io::stdout().is_terminal())
    }

    /// For standard error.
    pub fn stderr() -> Self {
        Self::detect(std::io::stderr().is_terminal())
    }

    fn detect(terminal: bool) -> Self {
        let set = |name| std::env::var_os(name).is_some_and(|v| !v.is_empty());
        let dumb = std::env::var_os("TERM").is_some_and(|t| t == "dumb");
        Self(terminal && !set("NO_COLOR") && !dumb)
    }

    fn paint(self, code: &str, text: &str) -> String {
        if self.0 {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }

    pub fn bold(self, text: &str) -> String {
        self.paint("1", text)
    }

    pub fn red(self, text: &str) -> String {
        self.paint("1;31", text)
    }

    pub fn green(self, text: &str) -> String {
        self.paint("32", text)
    }

    /// `text` with each line's verdict colored: `ok` and `FAIL` (doctor,
    /// policy-test), `widens`/`narrows` and the result (policy-diff), and
    /// the decision (explain).
    pub fn verdicts(self, text: &str) -> String {
        if !self.0 {
            return text.to_owned();
        }
        let mut out = String::with_capacity(text.len() + 64);
        for line in text.split_inclusive('\n') {
            let colored = [
                ("ok ", Self::green as fn(Self, &str) -> String),
                ("FAIL ", Self::red),
                ("widens:", Self::red),
                ("narrows:", Self::green),
                ("result: access widened", Self::red),
                ("result: no access widened", Self::green),
                ("decision: forward", Self::green),
                ("decision: refuse", Self::red),
            ]
            .into_iter()
            .find_map(|(prefix, color)| {
                let rest = line.strip_prefix(prefix)?;
                Some(format!("{}{rest}", color(self, prefix)))
            });
            out.push_str(colored.as_deref().unwrap_or(line));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_verdicts_are_colored_and_off_means_plain() {
        let text = "ok    a\nFAIL  b: why\nother ok\ndecision: refuse 403\n";
        assert_eq!(Paint::OFF.verdicts(text), text);
        assert_eq!(
            Paint(true).verdicts(text),
            "\x1b[32mok \x1b[0m   a\n\x1b[1;31mFAIL \x1b[0m b: why\nother ok\n\
             \x1b[1;31mdecision: refuse\x1b[0m 403\n"
        );
        assert_eq!(Paint::OFF.red("wye:"), "wye:");
    }
}
