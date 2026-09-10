use clap::Parser;
use gat::cli::{self, Cli};
use gat::error::Failure;
use gat::output::error as error_output;
use gat::{app, output};
use gat_engine::Invocation;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut stdout = anstream::AutoStream::auto(std::io::stdout());
    let mut stderr = anstream::AutoStream::auto(std::io::stderr());
    let mut output = output::Output::new(&mut stdout, &mut stderr);
    refresh_output_layouts(&mut output, false);
    match run(&mut output) {
        Ok(code) => ExitCode::from(code),
        Err(ProcessFailure::Usage(error)) => {
            let (stream, code) = if error.use_stderr() {
                (output::Stream::Stderr, 2)
            } else {
                (output::Stream::Stdout, 0)
            };
            match error.print() {
                Ok(()) => ExitCode::from(code),
                Err(source) => report_output_failure(
                    &mut output,
                    output::WriteFailure { stream, source },
                    code,
                ),
            }
        }
        Err(ProcessFailure::Command(failure)) => report_failure(&mut output, &failure),
        Err(ProcessFailure::Output(error)) => report_output_failure(&mut output, error, 0),
    }
}

// Terminal discovery stays at the process boundary; rendering uses injected layouts.
fn refresh_output_layouts(output: &mut output::Output<'_>, full_output: bool) {
    let layout = |size: Option<(terminal_size::Width, terminal_size::Height)>| {
        size.map_or_else(output::OutputLayout::default, |(width, _)| {
            output::OutputLayout::bounded(usize::from(width.0))
        })
        .with_full_output(full_output)
    };
    output.set_layouts(
        layout(terminal_size::terminal_size_of(std::io::stdout())),
        layout(terminal_size::terminal_size_of(std::io::stderr())),
    );
}

fn report_failure(output: &mut output::Output<'_>, failure: &Failure) -> ExitCode {
    // If stderr fails, retain the failure's exit code and stop. Never recurse
    // into diagnostic rendering or send a fallback diagnostic to stdout.
    let _ = error_output::render(output, failure.diagnostic());
    ExitCode::from(failure.exit_code())
}

fn report_output_failure(
    output: &mut output::Output<'_>,
    error: output::WriteFailure,
    original_code: u8,
) -> ExitCode {
    if error.source.kind() == std::io::ErrorKind::BrokenPipe {
        return ExitCode::from(original_code);
    }
    match error.stream {
        output::Stream::Stdout => report_failure(output, &Failure::from(error)),
        output::Stream::Stderr => ExitCode::from(original_code.max(1)),
    }
}

enum ProcessFailure {
    Usage(clap::Error),
    Command(Failure),
    Output(output::WriteFailure),
}

impl From<Failure> for ProcessFailure {
    fn from(failure: Failure) -> Self {
        Self::Command(failure)
    }
}

// Broken pipes mean the consumer has finished reading. Preserve the command's
// exit status; every other output failure becomes a safe application failure.
fn output_result(result: Result<(), output::WriteFailure>) -> Result<(), ProcessFailure> {
    match result {
        Err(error) if error.source.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => other.map_err(ProcessFailure::Output),
    }
}

fn run(output: &mut output::Output<'_>) -> Result<u8, ProcessFailure> {
    let cli = Cli::try_parse().map_err(ProcessFailure::Usage)?;
    let invocation = Invocation::capture_process().map_err(Failure::from)?;
    // opendal auto-registers enabled services (S3/Azblob/Fs/...) via a
    // ctor at load time; call this explicitly too so `Operator::from_uri`
    // works even in link configurations where ctors don't run.
    gat_engine::initialize_backends();

    // The process bootstrap owns one explicitly sized Rayon pool and Tokio
    // runtime. The runtime is only needed so opendal's blocking::Operator
    // (used by push/fetch) has an executor to hand work to; the rest of gat
    // remains synchronous at the command boundary.
    let rt = gat::process_resources::initialize()
        .map_err(gat::error::map::runtime_bootstrap::runtime_start_failed)?;
    let _guard = rt.enter();

    let full_output = cli.full_output;
    output.set_full_output(full_output);
    let repo = invocation.discover().map_err(Failure::from)?;
    let context = app::Context::new(repo);

    // Hooks stay silent regardless of TTY (handled inside `commands::hook`
    // itself via a hard-coded `NoopProgress`); every other invocation's
    // progress policy is decided once, from execution settings -- never
    // re-derived by individual commands.
    let hook_mode = matches!(cli.command, cli::Command::Hook { .. });
    let progress = output::progress::for_environment(output::progress::ProgressOptions {
        hook_mode,
        quiet: false,
    });

    let result = app::run(cli, &context, progress.as_ref());
    // Clear any transient progress UI before emitting notices/rendering the
    // durable outcome or error, so final output never interleaves with (or
    // gets clobbered by) a spinner still mid-redraw.
    progress.finish_all();
    // A long operation may outlive a resize. Refresh after clearing progress so
    // notices, results, and command failures use the current per-stream widths.
    refresh_output_layouts(output, full_output);
    // Notices recorded into `context.lifecycle` (by CLI dispatch, `gat
    // config` read/write, or actual consumption of a persisted config
    // value) are emitted unconditionally, before the command's durable
    // result/error is handled: a failing experimental command must still
    // tell the user it's experimental, not just a successful one.
    output_result(output::notices::emit(output, &context.lifecycle))?;
    let outcome = result?;
    let code = app::exit_code(&outcome);
    output_result(output::render::render(output, outcome))?;
    Ok(code)
}
