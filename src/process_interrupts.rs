//! OS signals belong to the executable, never to engine or I/O services.

use gat_engine::TransferCancellation;
use std::sync::{Arc, OnceLock};

#[derive(Clone, Copy)]
enum Interrupt {
    CtrlC,
    #[cfg(unix)]
    Terminate,
}

impl Interrupt {
    const fn exit_code(self) -> u8 {
        match self {
            Self::CtrlC => 130,
            #[cfg(unix)]
            Self::Terminate => 143,
        }
    }
}

pub(super) struct ProcessInterrupts {
    first_interrupt: Arc<OnceLock<Interrupt>>,
    listener: tokio::task::JoinHandle<()>,
}

impl ProcessInterrupts {
    pub(super) fn install(cancellation: TransferCancellation) -> std::io::Result<Self> {
        // Register before returning, so command startup cannot race registration.
        let mut signals = Signals::new()?;
        let first_interrupt = Arc::new(OnceLock::new());
        let recorded = first_interrupt.clone();
        let listener = tokio::spawn(async move {
            while let Some(interrupt) = signals.recv().await {
                if recorded.set(interrupt).is_err() {
                    // A second interrupt explicitly abandons cooperative cleanup.
                    std::process::exit(i32::from(interrupt.exit_code()));
                }
                cancellation.cancel();
            }
        });
        Ok(Self {
            first_interrupt,
            listener,
        })
    }

    pub(super) fn exit_code(&self) -> Option<u8> {
        self.first_interrupt
            .get()
            .copied()
            .map(Interrupt::exit_code)
    }
}

impl Drop for ProcessInterrupts {
    fn drop(&mut self) {
        self.listener.abort();
    }
}

#[cfg(unix)]
struct Signals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl Signals {
    fn new() -> std::io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    async fn recv(&mut self) -> Option<Interrupt> {
        tokio::select! {
            signal = self.interrupt.recv() => signal.map(|()| Interrupt::CtrlC),
            signal = self.terminate.recv() => signal.map(|()| Interrupt::Terminate),
        }
    }
}

#[cfg(windows)]
struct Signals(tokio::signal::windows::CtrlC);

#[cfg(windows)]
impl Signals {
    fn new() -> std::io::Result<Self> {
        tokio::signal::windows::ctrl_c().map(Self)
    }

    async fn recv(&mut self) -> Option<Interrupt> {
        self.0.recv().await.map(|()| Interrupt::CtrlC)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{BufRead, Write};
    use std::process::{Command, Stdio};
    use std::time::Duration;

    const HELPER: &str = "process_interrupts::tests::interrupt_process_helper";

    #[test]
    #[ignore = "subprocess helper, launched with an explicit staging path"]
    fn interrupt_process_helper() {
        let stage = std::path::PathBuf::from(std::env::var_os("GAT_TEST_INTERRUPT_STAGE").unwrap());
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let _entered = runtime.enter();
        let cancellation = TransferCancellation::default();
        let signals = ProcessInterrupts::install(cancellation.clone()).unwrap();
        std::fs::write(&stage, b"owned work awaiting cleanup").unwrap();
        println!("READY");
        runtime.block_on(cancellation.cancelled());
        println!("CANCELLED");
        // A deterministic cleanup gate, not a timing delay. The parent either
        // releases this work or asks a second signal to terminate immediately.
        let mut release = String::new();
        std::io::stdin().read_line(&mut release).unwrap();
        assert_eq!(release, "finish\n");
        std::fs::remove_file(stage).unwrap();
        std::process::exit(i32::from(signals.exit_code().unwrap()));
    }

    #[test]
    fn first_signal_waits_for_cleanup_and_second_signal_forces_exit() {
        for (signal, expected, force) in [
            ("-INT", 130, false),
            ("-TERM", 143, false),
            ("-INT", 130, true),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let stage = temp.path().join("stage");
            let mut cmd = Command::new(std::env::current_exe().unwrap());
            cmd.args(["--ignored", "--exact", HELPER, "--nocapture"])
                .env("GAT_TEST_INTERRUPT_STAGE", &stage)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped());
            let mut child = test_support::ChildGuard::spawn(cmd);
            // Child::wait closes stdin if the Child still owns it. Keep the
            // cleanup gate separately owned so waiting cannot release it.
            let mut input = child.take_stdin().unwrap();
            let stdout = child.take_stdout().unwrap();
            let (sender, receiver) = std::sync::mpsc::channel();
            let reader = std::thread::spawn(move || {
                for line in std::io::BufReader::new(stdout).lines() {
                    if sender.send(line.unwrap()).is_err() {
                        break;
                    }
                }
            });
            let wait_marker = |marker| {
                loop {
                    if receiver.recv_timeout(Duration::from_secs(10)).unwrap() == marker {
                        break;
                    }
                }
            };
            wait_marker("READY");
            let send_signal = || {
                assert!(
                    Command::new("kill")
                        .args([signal, &child.id().to_string()])
                        .status()
                        .unwrap()
                        .success()
                );
            };
            send_signal();
            wait_marker("CANCELLED");
            assert!(
                stage.is_file(),
                "first signal must leave owned cleanup running"
            );
            if force {
                send_signal();
            } else {
                input.write_all(b"finish\n").unwrap();
            }
            assert_eq!(child.wait().unwrap().code(), Some(expected));
            assert_eq!(stage.exists(), force);
            reader.join().unwrap();
        }
    }
}
