# au-diagnostics

Structured diagnostic records: `Diagnostic { code, severity, span, message, related, fix }`. Stable kebab-case `DiagnosticCode` strings (`Cow<'static, str>` so producer-side `const`s stay zero-cost). JSON serializable.

Spans carry canonical byte offsets plus an optional `line_col` rendering. `LineIndex` is the byte-offset → line/column table (1-based, byte-counted columns), built once per read file and attached by the engine build.

Producers declare codes as module-level `const`s in their own crates; this crate ships only the shape.
