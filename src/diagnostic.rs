use std::fmt::{Display, Formatter};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Span {
    pub file: String,
    pub line: usize,
    pub column: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub code: &'static str,
    pub severity: Severity,
    pub message: String,
    pub span: Span,
    pub path: Vec<String>,
    pub hint: Option<String>,
}

impl Diagnostic {
    pub fn error(code: &'static str, message: impl Into<String>, span: Span) -> Self {
        Self {
            code,
            severity: Severity::Error,
            message: message.into(),
            span,
            path: Vec::new(),
            hint: None,
        }
    }

    /// No check reports a warning today (W2101 was replaced by the parser's E7001); the
    /// severity stays for `warnings_as_errors` and future lints.
    #[allow(dead_code)]
    pub fn warning(code: &'static str, message: impl Into<String>, span: Span) -> Self {
        Self {
            code,
            severity: Severity::Warning,
            message: message.into(),
            span,
            path: Vec::new(),
            hint: None,
        }
    }

    pub fn with_path(mut self, path: Vec<String>) -> Self {
        self.path = path;
        self
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl Display for Diagnostic {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let severity = match self.severity {
            Severity::Error => "BIND ERROR",
            Severity::Warning => "BIND WARNING",
        };
        writeln!(f, "{severity} {}", self.code)?;
        if !self.span.file.is_empty() {
            writeln!(
                f,
                "at {}:{}:{}",
                self.span.file, self.span.line, self.span.column
            )?;
        }
        writeln!(f, "\n{}", self.message)?;
        if !self.path.is_empty() {
            writeln!(f, "\nBinding path:")?;
            for (index, item) in self.path.iter().enumerate() {
                if index == 0 {
                    writeln!(f, "    {item}")?;
                } else {
                    writeln!(f, "      -> {item}")?;
                }
            }
        }
        if let Some(hint) = &self.hint {
            writeln!(f, "\nHint: {hint}")?;
        }
        Ok(())
    }
}

pub fn has_errors(diagnostics: &[Diagnostic]) -> bool {
    diagnostics
        .iter()
        .any(|item| item.severity == Severity::Error)
}
