use std::sync::{Arc, Mutex};

type Sink = Arc<dyn Fn(&str) + Send + Sync>;

/// One line per action: `<level> <message> key="value" ...`. Callers never pass secrets.
#[derive(Clone)]
pub struct Log {
    sink: Sink,
}

impl Log {
    pub fn stderr() -> Log {
        Log {
            sink: Arc::new(|line| eprintln!("{line}")),
        }
    }

    pub fn silent() -> Log {
        Log {
            sink: Arc::new(|_| {}),
        }
    }

    /// A log that keeps its lines, for tests.
    pub fn memory() -> (Log, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let log = Log {
            sink: Arc::new(move |line: &str| {
                sink.lock().expect("log lines").push(line.to_string())
            }),
        };
        (log, lines)
    }

    pub fn info(&self, message: &str, fields: &[(&str, &str)]) {
        self.emit("info", message, fields);
    }

    pub fn warn(&self, message: &str, fields: &[(&str, &str)]) {
        self.emit("warn", message, fields);
    }

    pub fn error(&self, message: &str, fields: &[(&str, &str)]) {
        self.emit("error", message, fields);
    }

    fn emit(&self, level: &str, message: &str, fields: &[(&str, &str)]) {
        (self.sink)(&format(level, message, fields));
    }
}

pub fn format(level: &str, message: &str, fields: &[(&str, &str)]) -> String {
    let mut line = format!("{level} {message}");
    for (key, value) in fields {
        line.push(' ');
        line.push_str(key);
        line.push('=');
        line.push_str(&serde_json::to_string(value).unwrap_or_default());
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_fields_as_json_strings() {
        assert_eq!(format("info", "started", &[]), "info started");
        assert_eq!(
            format("info", "started", &[("pool", "ci"), ("run", "a\"b")]),
            r#"info started pool="ci" run="a\"b""#
        );
    }

    #[test]
    fn memory_log_keeps_lines() {
        let (log, lines) = Log::memory();
        log.warn("x", &[("k", "v")]);
        assert_eq!(lines.lock().unwrap().as_slice(), [r#"warn x k="v""#]);
    }
}
