//! Borrowed destinations for durable output. Destination policy (including
//! color stripping) belongs to the caller, not the renderer.
use std::fmt;
use std::io::{self, Write};

/// The destination whose write or flush failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// A stream-aware failure; technical source text is never rendered.
#[derive(Debug, thiserror::Error)]
#[error("output write failed")]
pub struct WriteFailure {
    pub stream: Stream,
    #[source]
    pub source: io::Error,
}

/// Writers borrowed for one rendering pass. No operation is executed here.
/// Each write is flushed so buffered destination failures reach the caller.
/// A renderer stops on its first failure, including a broken pipe; the process
/// boundary decides the exit status. A failed write may leave a partial prefix.
pub struct Output<'a> {
    stdout: &'a mut dyn Write,
    stderr: &'a mut dyn Write,
}

impl<'a> Output<'a> {
    pub fn new(stdout: &'a mut dyn Write, stderr: &'a mut dyn Write) -> Self {
        Self { stdout, stderr }
    }

    fn write(&mut self, stream: Stream, args: fmt::Arguments<'_>) -> Result<(), WriteFailure> {
        let writer = match stream {
            Stream::Stdout => &mut self.stdout,
            Stream::Stderr => &mut self.stderr,
        };
        writer
            .write_fmt(args)
            .and_then(|()| writer.flush())
            .map_err(|source| WriteFailure { stream, source })
    }

    fn line(&mut self, stream: Stream, args: fmt::Arguments<'_>) -> Result<(), WriteFailure> {
        self.write(stream, format_args!("{args}\n"))
    }

    pub(crate) fn stdout(&mut self, args: fmt::Arguments<'_>) -> Result<(), WriteFailure> {
        self.line(Stream::Stdout, args)
    }

    pub(crate) fn stderr(&mut self, args: fmt::Arguments<'_>) -> Result<(), WriteFailure> {
        self.line(Stream::Stderr, args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Outcome;
    use crate::error::{Diagnostic, ErrorCode};

    struct FailingWriter {
        bytes: Vec<u8>,
        remaining: usize,
        kind: io::ErrorKind,
        fail_flush: bool,
        failures: usize,
    }

    impl Write for FailingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                self.failures += 1;
                return Err(io::Error::new(self.kind, "SECRET technical sentinel"));
            }
            let n = self.remaining.min(buf.len());
            self.bytes.extend_from_slice(&buf[..n]);
            self.remaining -= n;
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.fail_flush {
                Err(io::Error::new(self.kind, "SECRET flush sentinel"))
            } else {
                Ok(())
            }
        }
    }

    fn status() -> Outcome {
        Outcome::Status(gat_command::StatusOutcome::NoTrackedFiles)
    }

    #[test]
    fn outcome_reports_immediate_partial_and_broken_pipe_writes() {
        let mut expected = Vec::new();
        crate::output::render::render(&mut Output::new(&mut expected, &mut Vec::new()), status())
            .unwrap();
        for kind in [io::ErrorKind::Other, io::ErrorKind::BrokenPipe] {
            for remaining in [0, 5] {
                let mut writer = FailingWriter {
                    bytes: Vec::new(),
                    remaining,
                    kind,
                    fail_flush: false,
                    failures: 0,
                };
                let mut stderr = Vec::new();
                let failure = crate::output::render::render(
                    &mut Output::new(&mut writer, &mut stderr),
                    status(),
                )
                .unwrap_err();
                assert_eq!(failure.stream, Stream::Stdout);
                assert_eq!(failure.source.kind(), kind);
                assert_eq!(writer.bytes, expected[..remaining]);
                assert_eq!(writer.failures, 1);
                assert!(stderr.is_empty());
                let mapped = crate::error::Failure::from(failure);
                assert_eq!(
                    mapped.diagnostic().summary(),
                    "Could not write command output to stdout"
                );
                assert!(!mapped.diagnostic().summary().contains("SECRET"));
            }
        }
    }

    #[test]
    fn failed_error_rendering_stops_at_the_partial_prefix() {
        let diagnostic = Diagnostic::new_for_test(ErrorCode::Internal, "Safe summary")
            .with_hint_for_test("Safe hint");
        let mut expected = Vec::new();
        crate::output::error::render(
            &mut Output::new(&mut Vec::new(), &mut expected),
            &diagnostic,
        )
        .unwrap();
        for remaining in [0, 7] {
            let mut writer = FailingWriter {
                bytes: Vec::new(),
                remaining,
                kind: io::ErrorKind::Other,
                fail_flush: false,
                failures: 0,
            };
            let mut stdout = Vec::new();
            let failure = crate::output::error::render(
                &mut Output::new(&mut stdout, &mut writer),
                &diagnostic,
            )
            .unwrap_err();
            assert_eq!(failure.stream, Stream::Stderr);
            assert_eq!(writer.bytes, expected[..remaining]);
            assert_eq!(writer.failures, 1);
            assert!(stdout.is_empty());
        }
    }

    #[test]
    fn notice_failure_stops_writing_and_drains_observations() {
        use crate::lifecycle::{Lifecycle, LifecycleObserve, Surface};
        for remaining in [0, 7] {
            let lifecycle = Lifecycle::new();
            lifecycle.observe(Surface::Command("gc"));
            let mut writer = FailingWriter {
                bytes: Vec::new(),
                remaining,
                kind: io::ErrorKind::Other,
                fail_flush: false,
                failures: 0,
            };
            let mut stdout = Vec::new();
            let failure = crate::output::notices::emit(
                &mut Output::new(&mut stdout, &mut writer),
                &lifecycle,
            )
            .unwrap_err();
            assert_eq!(failure.stream, Stream::Stderr);
            assert_eq!(writer.bytes.len(), remaining);
            assert_eq!(writer.failures, 1);
            assert!(stdout.is_empty());
            assert!(lifecycle.take_notices().is_empty());
        }
    }

    #[test]
    fn flush_failure_is_reported_on_the_correct_stream() {
        for stream in [Stream::Stdout, Stream::Stderr] {
            let mut writer = FailingWriter {
                bytes: Vec::new(),
                remaining: usize::MAX,
                kind: io::ErrorKind::Other,
                fail_flush: true,
                failures: 0,
            };
            let mut unused = Vec::new();
            let mut output = match stream {
                Stream::Stdout => Output::new(&mut writer, &mut unused),
                Stream::Stderr => Output::new(&mut unused, &mut writer),
            };
            let failure = output.line(stream, format_args!("safe")).unwrap_err();
            assert_eq!(failure.stream, stream);
            assert_eq!(writer.bytes, b"safe\n");
        }
    }
}
