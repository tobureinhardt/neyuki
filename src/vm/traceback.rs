//! Stack trace capture and formatted runtime error reporting.

use std::rc::Rc;

use crate::vm::machine::VM;

/// One line of a traceback: where a frame was when an error was raised.
#[derive(Clone, Debug)]
pub struct TraceFrame {
    pub function: Option<String>,
    pub source: Option<Rc<str>>,
    pub line: u32,
}

impl std::fmt::Display for TraceFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let source = self.source.as_deref().unwrap_or("?");
        match self.function.as_deref() {
            None | Some("main") => write!(f, "{}:{} in main chunk", source, self.line),
            Some(name) => write!(f, "{}:{} in function '{}'", source, self.line, name),
        }
    }
}

impl VM {
    /// Where every live frame is, innermost first. Deep stacks keep their
    /// innermost and outermost frames.
    pub fn capture_trace(&self) -> Vec<TraceFrame> {
        const KEEP: usize = 12;
        let frame_at = |frame: &crate::vm::frame::CallFrame| {
            let proto = &frame.closure.proto;
            TraceFrame {
                function: proto.name.clone(),
                source: proto.source.clone(),
                line: proto
                    .lines
                    .get(frame.ip.saturating_sub(1))
                    .copied()
                    .unwrap_or(0),
            }
        };
        let frames: Vec<_> = self.frames.iter().rev().collect();
        if frames.len() <= KEEP * 2 {
            return frames.into_iter().map(frame_at).collect();
        }
        let mut trace: Vec<TraceFrame> = frames[..KEEP].iter().map(|f| frame_at(f)).collect();
        trace.push(TraceFrame {
            function: Some(format!("... {} more frames", frames.len() - KEEP * 2)),
            source: None,
            line: 0,
        });
        trace.extend(frames[frames.len() - KEEP..].iter().map(|f| frame_at(f)));
        trace
    }

    /// An error message followed by the traceback recorded for it, if any.
    pub fn describe_error(&self, message: &str) -> String {
        let mut out = format!(
            "runtime error[{}]: {}\n",
            crate::diagnostics::error_code::ErrorCode::E0200,
            message
        );
        if let Some(trace) = &self.error_trace {
            if let Some(top) = trace.first() {
                let src_name = top.source.as_deref().unwrap_or("<unknown>");
                let line = top.line.max(1);
                let pad = line.to_string().len().max(2);
                out.push_str(&format!(
                    "{:pad$}--> {}:{}:1\n",
                    "",
                    src_name,
                    line,
                    pad = pad
                ));
                out.push_str(&format!("{:pad$} |\n", "", pad = pad));
                let bundled = crate::app::read(src_name)
                    .and_then(Result::ok)
                    .and_then(|bytes| String::from_utf8(bytes).ok());
                if let Some(content) = bundled.or_else(|| std::fs::read_to_string(src_name).ok())
                    && let Some(line_content) = content.lines().nth((line - 1) as usize)
                {
                    out.push_str(&format!("{:pad$} | {}\n", line, line_content, pad = pad));
                    out.push_str(&format!("{:pad$} | ^ {}\n", "", message, pad = pad));
                }
                out.push_str(&format!("{:pad$} |\n", "", pad = pad));
            }
            out.push_str("  = stack backtrace:\n");
            for (idx, frame) in trace.iter().enumerate() {
                if frame.source.is_none() && frame.line == 0 {
                    out.push_str(&format!(
                        "    {:2}: {}\n",
                        idx,
                        frame.function.as_deref().unwrap_or("...")
                    ));
                } else {
                    let func = frame.function.as_deref().unwrap_or("<main>");
                    let src = frame.source.as_deref().unwrap_or("?");
                    out.push_str(&format!(
                        "    {:2}: {} at {}:{}\n",
                        idx, func, src, frame.line
                    ));
                }
            }
        }
        out
    }
}
